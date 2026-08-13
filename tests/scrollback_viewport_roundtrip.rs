//! A scrolled-back viewport must arrive at the client looking like the row the
//! host is showing, for every offset the wheel can reach.

use p2pmux::screen::{GuestScreen, HostScreen, snapshot_payload};

/// Content shaped like what a pane actually holds while scrolling: colour runs,
/// blank gaps, and lines that reuse the row above.
fn busy_output(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for line in 0..lines {
        match line % 5 {
            0 => out.extend_from_slice(
                b"\x1b[32mdef \x1b[mis_struct(annotation: \x1b[36mAny\x1b[m) -> bool:",
            ),
            1 => {
                out.extend_from_slice(b"    \"\"\"Whether `annotation` is a msgspec Struct.\"\"\"")
            }
            2 => out.extend_from_slice(
                "  \x1b[1m*\x1b[m Wrangling\u{2026} (esc to interrupt \u{00b7} 12s)".as_bytes(),
            ),
            3 => out.extend_from_slice(b""),
            _ => out.extend_from_slice(
                format!("\x1b[31m{line:>4}\x1b[m | some text with a trailing pipe        |")
                    .as_bytes(),
            ),
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn rows(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    (0..rows)
        .map(|row| screen.contents_between(row, 0, row, cols))
        .collect()
}

#[test]
fn every_scrollback_offset_survives_the_snapshot_codec() {
    let mut host = HostScreen::new(24, 80).expect("host screen");
    host.process_pty(&busy_output(400)).expect("pty output");

    // The node hands the client one frozen viewport at a time, exactly the way
    // `FrozenScrollback` does: move the offset, encode, decode, compare.
    let mut history = host.screen().clone();
    let total = 200;
    for offset in 0..=total {
        history.set_scrollback(offset);
        let payload = snapshot_payload(&history).expect("viewport encodes");
        let mut guest = GuestScreen::new();
        guest.apply_snapshot(1, &payload).expect("viewport decodes");
        let decoded = guest.screen().expect("decoded viewport");

        assert_eq!(
            rows(decoded),
            rows(&history),
            "offset {offset} did not survive the round trip"
        );
    }
}
