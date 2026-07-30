//! One local PTY process and its blocking I/O handles.

use std::{
    error::Error,
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Environment variable naming the pane a process is running in.
pub const PANE_ID_ENV: &str = "P2PMUX_PANE_ID";
/// Environment variable naming this node's local IPC socket.
pub const SOCKET_ENV: &str = "P2PMUX_SOCK";

static AGENT_SOCKET_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Publish this node's local IPC socket path to every PTY spawned afterwards.
///
/// Together with `P2PMUX_PANE_ID`, this is what lets a program running inside a
/// pane — an agent hook, a build script — report back to the mux: it knows
/// which pane it is and where to say so. Both are set on the pane's PTY rather
/// than inherited from the client, because a peer's client process is on a
/// different machine from the node that owns the pane.
///
/// The path is process-global rather than a spawn argument because the socket
/// belongs to the node, not to any one pane; this mirrors how `agent_detect`
/// installs its notification tuning. Called once during node startup before the
/// first pane spawns; later calls are ignored.
pub fn set_agent_socket_path(path: PathBuf) {
    let _ = AGENT_SOCKET_PATH.set(path);
}

/// The single local shell PTY used by the Spike 1 terminal.
pub struct PtyHost {
    writer: Option<Box<dyn Write + Send>>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    output_rx: Receiver<std::io::Result<Vec<u8>>>,
    output_closed: bool,
    reader_join: Option<JoinHandle<()>>,
}

impl PtyHost {
    /// Spawn `command` in one fixed-size PTY and start its output reader.
    pub fn spawn(command: CommandBuilder, size: PtySize) -> Result<Self, Box<dyn Error>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size)?;
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let child = pair.slave.spawn_command(command)?;
        let (output_tx, output_rx) = mpsc::channel();
        let reader_join = thread::spawn(move || {
            let mut buffer = vec![0; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if output_tx.send(Ok(buffer[..count].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = output_tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            writer: Some(writer),
            master: Some(pair.master),
            child: Some(child),
            output_rx,
            output_closed: false,
            reader_join: Some(reader_join),
        })
    }

    /// Spawn the user's login shell, or `/bin/zsh` when `SHELL` is unset.
    pub fn spawn_default_shell(size: PtySize) -> Result<Self, Box<dyn Error>> {
        Self::spawn_default_shell_with_cwd(size, None, None)
    }

    /// Spawn the user's login shell with an optional working directory.
    ///
    /// `pane_id` is `None` for the shells that are not roster panes — the plain
    /// `p2pmux local` terminal and the single-pane host runtime — so nothing
    /// running in them can claim to be a pane that exists.
    pub fn spawn_default_shell_with_cwd(
        size: PtySize,
        cwd: Option<&Path>,
        pane_id: Option<u64>,
    ) -> Result<Self, Box<dyn Error>> {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/zsh"));
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        command.env("TERM", "xterm-256color");
        if let Some(pane_id) = pane_id {
            command.env(PANE_ID_ENV, pane_id.to_string());
            if let Some(socket) = AGENT_SOCKET_PATH.get() {
                command.env(SOCKET_ENV, socket);
            }
        }
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        }
        Self::spawn(command, size)
    }

    /// Return the next currently available output chunk, if any.
    pub fn try_read_output(&mut self) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        match self.output_rx.try_recv() {
            Ok(Ok(bytes)) if bytes.is_empty() => Ok(None),
            Ok(Ok(bytes)) => Ok(Some(bytes)),
            Ok(Err(error)) => Err(error.into()),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.output_closed = true;
                Ok(None)
            }
        }
    }

    /// Return whether the PTY reader reached EOF.
    pub fn output_closed(&self) -> bool {
        self.output_closed
    }

    /// Return the PTY session child PID while the child is still owned.
    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(|child| child.process_id())
    }

    /// Reap an exited child without releasing its PTY handles or joining the reader.
    pub fn try_wait(&mut self) -> Result<bool, Box<dyn Error>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(true);
        };
        if child.try_wait()?.is_some() {
            self.child.take();
            return Ok(true);
        }
        Ok(false)
    }

    /// Write terminal input to the child PTY immediately.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        let writer = self.writer.as_mut().ok_or("PTY writer is shut down")?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize the local PTY while retaining its existing reader and writer handles.
    pub fn resize(&mut self, size: PtySize) -> Result<(), Box<dyn Error>> {
        self.master
            .as_mut()
            .ok_or("PTY master is shut down")?
            .resize(size)?;
        Ok(())
    }

    /// Stop the child, release its PTY handles, and join the reader once.
    pub fn shutdown(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(mut child) = self.child.take()
            && child.try_wait()?.is_none()
        {
            child.kill()?;
            child.wait()?;
        }
        self.writer.take();
        self.master.take();
        if let Some(reader_join) = self.reader_join.take() {
            reader_join
                .join()
                .map_err(|_| "PTY reader thread panicked")?;
        }
        self.output_closed = true;
        Ok(())
    }
}

impl Drop for PtyHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
