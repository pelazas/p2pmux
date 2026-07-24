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
