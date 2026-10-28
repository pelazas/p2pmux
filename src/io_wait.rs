//! Wake the node when a pane prints, so the socket loop can sleep while idle.
//!
//! The loop used to time out a socket read every millisecond in case a PTY had
//! produced something. Nothing is producing anything for most of a session's
//! life, and that timeout is a wake-up. The reader thread already blocks inside
//! the PTY; it taps this pipe when bytes arrive, and the loop waits on the pipe
//! together with the client socket.

use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use nix::poll::{PollFd, PollTimeout, poll};

struct Pipe {
    read: Mutex<UnixStream>,
    write: Mutex<UnixStream>,
}

static PIPE: OnceLock<Pipe> = OnceLock::new();

fn pipe() -> &'static Pipe {
    PIPE.get_or_init(|| {
        let (read, write) = UnixStream::pair().expect("idle wake pipe");
        read.set_nonblocking(true).expect("wake read end");
        write.set_nonblocking(true).expect("wake write end");
        Pipe {
            read: Mutex::new(read),
            write: Mutex::new(write),
        }
    })
}

/// Tell the socket loop that a pane has output waiting.
///
/// A no-op until [`enable`] — a PTY that is not owned by the node has nobody
/// waiting, and filling a pipe nobody reads would only block the reader.
pub fn pulse() {
    let Some(pipe) = PIPE.get() else {
        return;
    };
    let mut write = pipe
        .write
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _ = write.write_all(&[1]);
}

/// Arm the pipe. Called once from the node before any pane can print.
pub fn enable() {
    let _ = pipe();
}

/// A duplicate of the read end, so a waiter can poll without holding the lock
/// the drainer needs.
pub fn reader() -> io::Result<UnixStream> {
    pipe()
        .read
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .try_clone()
}

/// Drop queued pulses. The bytes are a count, not a message.
pub fn drain() {
    let Some(pipe) = PIPE.get() else {
        return;
    };
    let mut read = pipe
        .read
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut buffer = [0_u8; 64];
    loop {
        match read.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

/// Block until `timeout` elapses or one of `fds` is ready, then clear pulses.
pub fn park(fds: &mut [PollFd<'_>], timeout: Duration) {
    let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
    let _ = poll(fds, timeout);
    drain();
}
