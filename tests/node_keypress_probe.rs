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
fn node_keypress_distinguishes_cr_lf_and_escape_cr() {
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

    host.write_input(b"\r").expect("PTY should accept CR");
    assert!(read_until(&mut host, "SUBMIT").contains("SUBMIT"));

    host.write_input(b"\n").expect("PTY should accept LF");
    assert!(read_until(&mut host, "NEWLINE").contains("NEWLINE"));

    host.write_input(b"\x1b\r")
        .expect("PTY should accept Escape + CR");
    let escape_cr = read_until(&mut host, "\"meta\":true");
    assert!(
        escape_cr.contains("\"name\":\"return\"") && escape_cr.contains("\"meta\":true"),
        "Escape + CR should be meta-return, not the plain submit or newline path: {escape_cr:?}"
    );

    host.shutdown().expect("PTY should shut down cleanly");
}
