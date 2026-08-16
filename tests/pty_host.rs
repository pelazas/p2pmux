use std::{
    fs, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use p2pmux::pty_host::PtyHost;
use portable_pty::{CommandBuilder, PtySize};

fn read_until(host: &mut PtyHost, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while Instant::now() < deadline {
        while let Some(bytes) = host
            .try_read_output()
            .expect("PTY reader should stay healthy")
        {
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
        if output.contains(expected) {
            return output;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("did not receive {expected:?}; received {output:?}");
}

/// Type a command at an interactive shell and wait for what it prints, re-typing it
/// until something comes back.
///
/// Sending once after a fixed sleep is what made these tests flaky in CI. A shell sets
/// up its line editor with `tcsetattr(TCSAFLUSH)`, which *discards* anything already
/// waiting in the tty input queue, so a command typed before the shell was ready is not
/// buffered -- it is dropped, and the test then waits for output that can never arrive.
/// There is no portable "the shell is ready now" signal to wait on (the prompt differs
/// per shell and per user rc file), so retry until the shell answers.
///
/// Every caller sends an idempotent command, so a repeat that lands after one already
/// worked only prints the same line twice.
fn run_until(host: &mut PtyHost, input: &[u8], expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    let mut next_attempt = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_attempt {
            host.write_input(input).expect("PTY should accept input");
            next_attempt = Instant::now() + Duration::from_millis(250);
        }
        while let Some(bytes) = host
            .try_read_output()
            .expect("PTY reader should stay healthy")
        {
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
        if output.contains(expected) {
            return output;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("did not receive {expected:?}; received {output:?}");
}

#[test]
fn pty_host_reads_output_and_writes_input() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        "printf ready; IFS= read -r line; printf ':reply:%s' \"$line\"",
    ]);
    let mut host = PtyHost::spawn(
        command,
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
    )
    .expect("PTY should spawn");

    assert!(read_until(&mut host, "ready").contains("ready"));
    host.write_input(b"hello from test\n")
        .expect("PTY should accept input");
    assert!(read_until(&mut host, ":reply:hello from test").contains(":reply:hello from test"));
    host.shutdown().expect("PTY should shut down cleanly");
    assert!(host.output_closed());
}

#[test]
fn pty_host_resizes_without_disrupting_io() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        "printf ready; IFS= read -r line; printf ':reply:%s' \"$line\"",
    ]);
    let mut host = PtyHost::spawn(
        command,
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
    )
    .expect("PTY should spawn");
    host.resize(PtySize {
        rows: 30,
        cols: 100,
        pixel_width: 0,
        pixel_height: 0,
    })
    .expect("resize succeeds");
    assert!(read_until(&mut host, "ready").contains("ready"));
    host.write_input(b"still alive\n")
        .expect("writer stays alive");
    assert!(read_until(&mut host, ":reply:still alive").contains("still alive"));
    host.shutdown().expect("clean shutdown");
}

#[test]
fn pty_host_reaps_an_exited_child_without_shutdown() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", "printf final; exit 0"]);
    let mut host = PtyHost::spawn(
        command,
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
    )
    .expect("PTY should spawn");

    assert!(read_until(&mut host, "final").contains("final"));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !host.try_wait().expect("nonblocking reap") {
        assert!(Instant::now() < deadline, "child should exit promptly");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(host.process_id().is_none());
    host.shutdown().expect("explicit shutdown remains clean");
}

#[test]
fn pty_host_default_shell_uses_explicit_working_directory() {
    let directory = std::env::temp_dir().join(format!(
        "p2pmux-pty-host-cwd-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos()
    ));
    fs::create_dir(&directory).expect("create temporary directory");
    let expected = fs::canonicalize(&directory).expect("canonicalize temporary directory");
    let mut host = PtyHost::spawn_default_shell_with_cwd(
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        Some(&directory),
        None,
    )
    .expect("PTY should spawn");

    assert!(
        run_until(&mut host, b"pwd -P\n", &expected.display().to_string())
            .contains(expected.to_str().expect("utf-8 path"))
    );

    host.shutdown().expect("PTY should shut down cleanly");
    fs::remove_dir(&directory).expect("remove temporary directory");
}

#[test]
fn pane_shells_learn_their_pane_id_and_plain_shells_do_not() {
    let size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    // A roster pane's shell can identify itself, which is what lets an agent
    // hook running inside it report status back for the right pane.
    let report = b"printf 'pane=[%s]\\n' \"$P2PMUX_PANE_ID\"\n";
    let mut pane =
        PtyHost::spawn_default_shell_with_cwd(size, None, Some(42)).expect("pane PTY should spawn");
    assert!(run_until(&mut pane, report, "pane=[42]").contains("pane=[42]"));
    pane.shutdown().expect("PTY should shut down cleanly");

    // `p2pmux local` and the single-pane host runtime are not roster panes, so
    // nothing inside them can claim to be a pane that exists.
    //
    // Including when p2pmux itself was started from inside a p2pmux pane, which
    // is how anyone working on this runs it. A PTY inherits this process's
    // environment, so the variable arrives already set and has to be taken back
    // out — otherwise the shell reports the *outer* pane, and a hook in it files
    // its status against a pane it is not in.
    let mut plain = PtyHost::spawn_default_shell(size).expect("plain PTY should spawn");
    assert!(run_until(&mut plain, report, "pane=[]").contains("pane=[]"));
    plain.shutdown().expect("PTY should shut down cleanly");

    // The socket travels with the id or not at all: a fresh pane id beside a
    // stale socket points a hook at the node of the p2pmux this one is running
    // inside, which is the same wrong row by a longer route.
    let both = b"printf 'pair=[%s][%s]\\n' \"$P2PMUX_PANE_ID\" \"$P2PMUX_SOCK\"\n";
    let mut pair =
        PtyHost::spawn_default_shell_with_cwd(size, None, Some(7)).expect("pane PTY should spawn");
    assert!(run_until(&mut pair, both, "pair=[7]").contains("pair=[7][]"));
    pair.shutdown().expect("PTY should shut down cleanly");
}
