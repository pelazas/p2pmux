use p2pmux::{
    protocol::{MAX_DELTA_BYTES, MAX_SNAPSHOT_BYTES},
    screen::{ApplyDelta, GuestScreen, HostScreen, SCREEN_CODEC_VERSION},
};

#[test]
fn initial_snapshot_describes_the_empty_fixed_grid() {
    let host = HostScreen::new(2, 3).expect("valid fixed grid");
    let frame = host.current_frame();
    assert_eq!(frame.sequence, 1);
    assert_eq!(frame.base_sequence, 0);
    assert_eq!(frame.snapshot[0], SCREEN_CODEC_VERSION);
    assert_eq!(&frame.snapshot[1..3], &2_u16.to_be_bytes());
    assert_eq!(&frame.snapshot[3..5], &3_u16.to_be_bytes());

    let mut guest = GuestScreen::new();
    guest
        .apply_snapshot(frame.sequence, &frame.snapshot)
        .expect("initial snapshot applies");
    assert_eq!(guest.sequence(), Some(1));
    assert_eq!(guest.screen().expect("screen").size(), (2, 3));
    assert_eq!(
        guest.screen().expect("screen").contents(),
        host.screen().contents()
    );
}

#[test]
fn host_screen_tracks_child_kitty_keyboard_mode() {
    let mut host = HostScreen::new(1, 1).expect("valid fixed grid");
    let pushed = host.process_pty(b"\x1b[>1u").expect("kitty push");
    assert!(host.kitty_keyboard_active());
    assert!(pushed.kitty_keyboard_active);
    let popped = host.process_pty(b"\x1b[<1u").expect("kitty pop");
    assert!(!host.kitty_keyboard_active());
    assert!(!popped.kitty_keyboard_active);

    let mut guest = GuestScreen::new();
    guest
        .apply_snapshot(pushed.sequence, &pushed.snapshot)
        .expect("pushed snapshot applies");
    guest.set_kitty_keyboard_active(pushed.kitty_keyboard_active);
    assert!(guest.kitty_keyboard_active());
    assert_eq!(
        guest.apply_delta(popped.base_sequence, popped.sequence, &popped.delta),
        Ok(ApplyDelta::Applied)
    );
    guest.set_kitty_keyboard_active(popped.kitty_keyboard_active);
    assert!(!guest.kitty_keyboard_active());
}

#[test]
fn host_screen_replies_to_kitty_keyboard_queries() {
    let mut host = HostScreen::new(1, 1).expect("valid fixed grid");
    host.process_pty(b"\x1b[?u").expect("kitty query");
    assert_eq!(
        host.take_kitty_keyboard_query_reply(),
        Some(b"\x1b[?0u".to_vec())
    );
    assert!(!host.kitty_keyboard_active());

    host.process_pty(b"\x1b[>1u").expect("kitty push");
    assert!(host.kitty_keyboard_active());
}

#[test]
fn host_snapshots_the_live_edge_after_scrollback_accumulates() {
    let mut host = HostScreen::new(1, 3).expect("valid fixed grid");
    let frame = host.process_pty(b"a\r\nb\r\nc").expect("scrollback update");
    let mut history = host.screen().clone();
    history.set_scrollback(10_000);
    assert!(history.scrollback() > 0);
    assert_eq!(host.screen().scrollback(), 0);

    let mut guest = GuestScreen::new();
    guest
        .apply_snapshot(frame.sequence, &frame.snapshot)
        .expect("live snapshot applies");
    assert_eq!(
        guest.screen().expect("guest screen").contents(),
        host.screen().contents()
    );
    assert_eq!(guest.screen().expect("guest screen").scrollback(), 0);
}

#[test]
fn snapshot_then_delta_reproduces_formatted_screen_and_modes() {
    let mut host = HostScreen::new(2, 3).expect("valid grid");
    let first = host
        .process_pty(b"\x1b[31;1mabc\x1b[?1h")
        .expect("first update");
    let second = host.process_pty(b"\r\ndef").expect("second update");
    let mut guest = GuestScreen::new();
    guest
        .apply_snapshot(first.sequence, &first.snapshot)
        .expect("snapshot applies");
    assert_eq!(
        guest.apply_delta(second.base_sequence, second.sequence, &second.delta),
        Ok(ApplyDelta::Applied)
    );
    let screen = guest.screen().expect("screen");
    assert_eq!(screen.contents(), host.screen().contents());
    assert_eq!(
        screen.application_cursor(),
        host.screen().application_cursor()
    );
    assert!(screen.cell(0, 0).expect("cell").bold());
}

#[test]
fn invalid_delta_leaves_the_last_good_screen_intact() {
    let mut host = HostScreen::new(1, 3).expect("grid");
    let first = host.process_pty(b"abc").expect("first");
    let second = host.process_pty(b"\rX").expect("second");
    let mut guest = GuestScreen::new();
    assert_eq!(
        guest.apply_delta(second.base_sequence, second.sequence, &second.delta),
        Ok(ApplyDelta::NeedsSnapshot)
    );
    guest
        .apply_snapshot(first.sequence, &first.snapshot)
        .expect("snapshot");
    let before = guest.screen().expect("screen").contents();
    assert_eq!(
        guest.apply_delta(99, 100, &second.delta),
        Ok(ApplyDelta::NeedsSnapshot)
    );
    assert_eq!(guest.screen().expect("screen").contents(), before);
    assert!(guest.apply_delta(1, 1, &second.delta).is_err());
    assert_eq!(guest.screen().expect("screen").contents(), before);
}

#[test]
fn a_fresh_snapshot_replaces_alternate_screen_state() {
    let mut host = HostScreen::new(1, 3).expect("grid");
    let alternate = host.process_pty(b"\x1b[?1049hALT").expect("alternate");
    let normal = host.process_pty(b"\x1b[?1049lN").expect("normal");
    let mut guest = GuestScreen::new();
    guest
        .apply_snapshot(alternate.sequence, &alternate.snapshot)
        .expect("alternate snapshot");
    assert_eq!(guest.screen().expect("screen").contents(), "ALT");
    guest
        .apply_snapshot(normal.sequence, &normal.snapshot)
        .expect("replacement snapshot");
    assert_eq!(
        guest.screen().expect("screen").contents(),
        host.screen().contents()
    );
}

#[test]
fn malformed_and_oversize_payloads_are_rejected() {
    let mut guest = GuestScreen::new();
    for payload in [vec![], vec![2, 0, 1, 0, 1], vec![1, 0, 0, 0, 1]] {
        assert!(guest.apply_snapshot(1, &payload).is_err());
    }
    assert!(
        guest
            .apply_snapshot(1, &vec![1; MAX_SNAPSHOT_BYTES + 1])
            .is_err()
    );
    assert!(
        guest
            .apply_delta(1, 2, &vec![1; MAX_DELTA_BYTES + 1])
            .is_err()
    );
}

#[test]
fn resize_forces_a_replacement_snapshot_for_guests() {
    let mut host = HostScreen::new(2, 3).expect("grid");
    let before = host.process_pty(b"abc").expect("frame");
    let resized = host.resize(4, 5).expect("resize");
    assert_eq!(host.screen().size(), (4, 5));
    assert!(resized.sequence > before.sequence);
    assert_eq!(resized.base_sequence, 0);
    assert!(resized.delta.is_empty());

    let mut guest = GuestScreen::new();
    guest
        .apply_snapshot(before.sequence, &before.snapshot)
        .expect("old snapshot");
    assert_eq!(
        guest.apply_delta(resized.base_sequence, resized.sequence, &resized.delta),
        Err(p2pmux::screen::ScreenError::InvalidSequence)
    );
    guest
        .apply_snapshot(resized.sequence, &resized.snapshot)
        .expect("replacement snapshot");
    assert_eq!(guest.screen().expect("screen").size(), (4, 5));
}
