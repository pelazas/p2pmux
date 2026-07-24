use p2pmux::{
    screen::HostScreen,
    session::{ScreenSendPlan, screen_send_plan},
};

#[tokio::test]
async fn coalesced_watch_gap_uses_snapshot_but_contiguous_update_uses_delta() {
    let mut host = HostScreen::new(1, 3).expect("screen");
    let (tx, rx) = tokio::sync::watch::channel(host.current_frame().clone());
    tx.send_replace(host.process_pty(b"a").expect("two"));
    tx.send_replace(host.process_pty(b"b").expect("three"));
    tx.send_replace(host.process_pty(b"c").expect("four"));
    let newest = rx.borrow().clone();
    assert_eq!(newest.sequence, 4);
    assert_eq!(
        screen_send_plan(1, &newest, false),
        ScreenSendPlan::Snapshot
    );
    assert_eq!(screen_send_plan(3, &newest, false), ScreenSendPlan::Delta);
    assert_eq!(screen_send_plan(4, &newest, true), ScreenSendPlan::Snapshot);
}

#[tokio::test]
async fn take_control_queue_has_priority_over_input_queue() {
    let (take_tx, mut take_rx) = tokio::sync::mpsc::channel(16);
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(256);
    input_tx.try_send(b"input".to_vec()).expect("input queued");
    take_tx.try_send(7_u64).expect("take queued");
    let first = tokio::select! {
        biased;
        value = take_rx.recv() => value.map(|_| "take"),
        value = input_rx.recv() => value.map(|_| "input"),
    };
    let second = tokio::select! {
        biased;
        value = take_rx.recv() => value.map(|_| "take"),
        value = input_rx.recv() => value.map(|_| "input"),
    };
    assert_eq!(first, Some("take"));
    assert_eq!(second, Some("input"));
}
