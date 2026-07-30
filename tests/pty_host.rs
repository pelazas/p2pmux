use std::{
    fs, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use p2pmux::pty_host::PtyHost;
use portable_pty::{CommandBuilder, PtySize};

fn read_until(host: &mut PtyHost, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
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

    thread::sleep(Duration::from_millis(100));
    host.write_input(b"pwd -P\n")
        .expect("PTY should accept input");
    assert!(
        read_until(&mut host, &expected.display().to_string())
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
    let mut pane =
        PtyHost::spawn_default_shell_with_cwd(size, None, Some(42)).expect("pane PTY should spawn");
    thread::sleep(Duration::from_millis(100));
    pane.write_input(b"printf 'pane=[%s]\\n' \"$P2PMUX_PANE_ID\"\n")
        .expect("PTY should accept input");
    assert!(read_until(&mut pane, "pane=[42]").contains("pane=[42]"));
    pane.shutdown().expect("PTY should shut down cleanly");

    // `p2pmux local` and the single-pane host runtime are not roster panes, so
    // nothing inside them can claim to be a pane that exists.
    let mut plain = PtyHost::spawn_default_shell(size).expect("plain PTY should spawn");
    thread::sleep(Duration::from_millis(100));
    plain
        .write_input(b"printf 'pane=[%s]\\n' \"$P2PMUX_PANE_ID\"\n")
        .expect("PTY should accept input");
    assert!(read_until(&mut plain, "pane=[]").contains("pane=[]"));
    plain.shutdown().expect("PTY should shut down cleanly");
}
