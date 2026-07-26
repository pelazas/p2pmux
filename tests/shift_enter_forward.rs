use std::{
    thread,
    time::{Duration, Instant},
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
fn shift_enter_forwards_as_newline() {
    let probe = format!(
        "{}/tests/fixtures/agent_newline_probe.js",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut command = CommandBuilder::new("node");
    command.arg(probe);
    let mut host = PtyHost::spawn(
        command,
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
    )
    .expect("Node probe PTY should spawn");

    assert!(read_until(&mut host, "READY").contains("READY"));

    // `encode_key(Enter + SHIFT)` is unit-tested to produce this LF byte.
    host.write_input(b"\n").expect("PTY should accept LF");
    assert!(read_until(&mut host, "NEWLINE").contains("NEWLINE"));

    host.write_input(b"\r").expect("PTY should accept CR");
    assert!(read_until(&mut host, "SUBMIT").contains("SUBMIT"));

    host.shutdown().expect("PTY should shut down cleanly");
}
