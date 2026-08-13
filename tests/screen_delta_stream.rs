//! A guest that follows the frame stream must end up with the host's screen,
//! batch after batch, for the kind of traffic a scrolling program emits.

use p2pmux::screen::{ApplyDelta, GuestScreen, HostScreen};

fn rows(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    (0..rows)
        .map(|row| screen.contents_between(row, 0, row, cols))
        .collect()
}

/// Deterministic stand-in for a program painting, scrolling and repainting.
struct Traffic {
    seed: u64,
}

impl Traffic {
    fn next(&mut self) -> u64 {
        self.seed = self
            .seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.seed >> 33
    }

    fn batch(&mut self, rows: u16, cols: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..(self.next() % 6 + 1) {
            let row = (self.next() % u64::from(rows)) as u16 + 1;
            let col = (self.next() % u64::from(cols)) as u16 + 1;
            match self.next() % 8 {
                // Absolute cursor moves, in both spellings: CUP and the HVP
                // that the host rewrites on its way to the parser.
                0 => out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes()),
                1 => out.extend_from_slice(format!("\x1b[{row};{col}f").as_bytes()),
                // A scrolling region, then output inside it: how a full-screen
                // program scrolls without repainting every row.
                2 => {
                    let bottom = row.max(2);
                    out.extend_from_slice(format!("\x1b[1;{bottom}r").as_bytes());
                    out.extend_from_slice(b"\x1b[H");
                }
                3 => out.extend_from_slice(b"\x1bM"),
                4 => out.extend_from_slice(b"\x1b[2K"),
                5 => out.extend_from_slice(b"\x1b[J"),
                6 => out.extend_from_slice("\x1b[1;35m\u{2014} wide \u{4f60}\u{597d}".as_bytes()),
                _ => out.extend_from_slice(b"\x1b[m"),
            }
            let text = format!(
                "r{row}c{col} the quick brown fox jumps over the lazy dog\r\n",
                row = row,
                col = col
            );
            out.extend_from_slice(&text.as_bytes()[..(self.next() as usize % text.len()).max(1)]);
        }
        out
    }
}

#[test]
fn a_guest_following_the_frame_stream_matches_the_host() {
    let (rows_n, cols_n) = (24_u16, 80_u16);
    let mut host = HostScreen::new(rows_n, cols_n).expect("host screen");
    let mut guest = GuestScreen::new();
    let frame = host.current_frame().clone();
    guest
        .apply_snapshot(frame.sequence, &frame.snapshot)
        .expect("initial snapshot");

    let mut traffic = Traffic { seed: 0x5eed };
    for batch in 0..400 {
        let bytes = traffic.batch(rows_n, cols_n);
        let frame = host.process_pty(&bytes).expect("host accepts output");
        if frame.base_sequence == 0 {
            guest
                .apply_snapshot(frame.sequence, &frame.snapshot)
                .expect("replacement snapshot");
        } else {
            match guest
                .apply_delta(frame.base_sequence, frame.sequence, &frame.delta)
                .expect("delta applies")
            {
                ApplyDelta::Applied => {}
                ApplyDelta::NeedsSnapshot => guest
                    .apply_snapshot(frame.sequence, &frame.snapshot)
                    .expect("fallback snapshot"),
            }
        }

        assert_eq!(
            rows(guest.screen().expect("guest screen")),
            rows(host.screen()),
            "guest drifted from the host at batch {batch}"
        );
    }
}
