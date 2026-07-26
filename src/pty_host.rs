//! One local PTY process and its blocking I/O handles.

use std::{
    error::Error,
    ffi::OsString,
    io::{Read, Write},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread::{self, JoinHandle},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

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
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/zsh"));
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        command.env("TERM", "xterm-256color");
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
