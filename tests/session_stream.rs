use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    lease::LeaseState,
    protocol::{ControlLease, Envelope, Join, PROTOCOL_VERSION, Snapshot, envelope},
    screen::HostScreen,
    session::{DEFAULT_PANE_ID, GuestEvent, HostPaneChannels, HostSession, join_pane},
    transport::{ALPN, Transport},
};
use tokio::time::{sleep, timeout};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

async fn loopback_transport() -> Transport {
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("localhost address")
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .expect("loopback endpoint");
    Transport::from_endpoint(endpoint)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_pane_delivers_snapshot_then_delta_in_order() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
    let mut screen = HostScreen::new(1, 3).expect("screen");
    let (screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 1,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel(8);
    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let incoming = host.accept_incoming().await.expect("incoming");
            host.serve_peer(
                incoming,
                HostPaneChannels {
                    pane_id: DEFAULT_PANE_ID.to_vec(),
                    host_peer_id: host_id,
                    screen_rx,
                    lease_rx,
                    control_tx,
                },
            )
            .await
        })
    };
    let mut pane = join_pane(loopback_transport().await, host.ticket().clone())
        .await
        .expect("join pane");
    assert!(
        matches!(timeout(TEST_TIMEOUT, pane.events.recv()).await.expect("lease timeout"), Some(GuestEvent::Lease(lease)) if lease.lease_epoch == 1)
    );
    assert!(
        matches!(timeout(TEST_TIMEOUT, pane.events.recv()).await.expect("event timeout"), Some(GuestEvent::ScreenSnapshot(snapshot)) if snapshot.sequence == 1)
    );
    screen_tx.send_replace(screen.process_pty(b"abc").expect("screen update"));
    assert!(
        matches!(timeout(TEST_TIMEOUT, pane.events.recv()).await.expect("event timeout"), Some(GuestEvent::ScreenDelta(delta)) if delta.base_sequence == 1 && delta.sequence == 2)
    );
    pane.shutdown().await;
    let _ = timeout(TEST_TIMEOUT, host_task).await;
    host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_welcome_screen_stream_starts_with_a_snapshot_then_sends_delta() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let guest = loopback_transport().await;
    let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
    let mut screen = HostScreen::new(1, 3).expect("screen");
    let (screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 1,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let incoming = host.accept_incoming().await.expect("incoming");
            host.serve_peer(
                incoming,
                HostPaneChannels {
                    pane_id: DEFAULT_PANE_ID.to_vec(),
                    host_peer_id: host_id,
                    screen_rx,
                    lease_rx,
                    control_tx,
                },
            )
            .await
        })
    };
    let connection = guest
        .connect(host.ticket().endpoint_addr().clone())
        .await
        .expect("connect");
    let guest_id = guest.endpoint_id().as_bytes().to_vec();
    let (mut handshake_send, mut handshake_recv) =
        guest.open_bi(&connection).await.expect("handshake");
    guest
        .write_frame(
            &mut handshake_send,
            &Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: guest_id.clone(),
                body: Some(envelope::Body::Join(Join {
                    session_id: host.ticket().session_id().to_vec(),
                    peer_id: guest_id.clone(),
                })),
            },
        )
        .await
        .expect("join");
    let _welcome = guest
        .read_frame(&mut handshake_recv)
        .await
        .expect("welcome");
    let (_screen_send, mut screen_recv) =
        guest.accept_framed_bi(&connection).await.expect("screen");
    let (_control_send, _control_recv) = guest.open_framed_bi(&connection).await.expect("control");
    let first = timeout(TEST_TIMEOUT, screen_recv.read_next())
        .await
        .expect("snapshot timeout")
        .expect("snapshot read")
        .expect("snapshot frame");
    assert!(matches!(
        first.body,
        Some(envelope::Body::Snapshot(Snapshot { sequence: 1, .. }))
    ));
    screen_tx.send_replace(screen.process_pty(b"abc").expect("update"));
    let second = timeout(TEST_TIMEOUT, screen_recv.read_next())
        .await
        .expect("delta timeout")
        .expect("delta read")
        .expect("delta frame");
    assert!(
        matches!(second.body, Some(envelope::Body::Delta(ref delta)) if delta.base_sequence == 1 && delta.sequence == 2)
    );
    assert!(control_rx.try_recv().is_err());
    drop(lease_tx);
    host_task.abort();
    let _ = host_task.await;
    guest.close().await;
    host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_keeps_a_silent_spectator_connected_after_control_stream_setup_window() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let guest = loopback_transport().await;
    let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
    let screen = HostScreen::new(1, 3).expect("screen");
    let (_screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 1,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel(8);
    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let incoming = host.accept_incoming().await.expect("incoming");
            host.serve_peer(
                incoming,
                HostPaneChannels {
                    pane_id: DEFAULT_PANE_ID.to_vec(),
                    host_peer_id: host_id,
                    screen_rx,
                    lease_rx,
                    control_tx,
                },
            )
            .await
        })
    };
    let connection = guest
        .connect(host.ticket().endpoint_addr().clone())
        .await
        .expect("connect");
    let guest_id = guest.endpoint_id().as_bytes().to_vec();
    let (mut handshake_send, mut handshake_recv) =
        guest.open_bi(&connection).await.expect("handshake");
    guest
        .write_frame(
            &mut handshake_send,
            &Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: guest_id.clone(),
                body: Some(envelope::Body::Join(Join {
                    session_id: host.ticket().session_id().to_vec(),
                    peer_id: guest_id,
                })),
            },
        )
        .await
        .expect("join");
    guest
        .read_frame(&mut handshake_recv)
        .await
        .expect("welcome");
    let (_screen_send, mut screen_recv) =
        guest.accept_framed_bi(&connection).await.expect("screen");
    assert!(matches!(
        timeout(TEST_TIMEOUT, screen_recv.read_next())
            .await
            .expect("snapshot timeout")
            .expect("snapshot read"),
        Some(Envelope {
            body: Some(envelope::Body::Snapshot(_)),
            ..
        })
    ));

    sleep(Duration::from_secs(6)).await;
    assert!(
        !host_task.is_finished(),
        "a spectator must not need to send input to keep its screen stream alive"
    );

    connection.close(0u8.into(), b"");
    host_task.abort();
    let _ = host_task.await;
    guest.close().await;
    host.close().await;
}

fn lease(sequence: u64) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: vec![7],
        body: Some(envelope::Body::ControlLease(ControlLease {
            pane_id: b"default-pane".to_vec(),
            controller_peer_id: vec![7],
            lease_epoch: sequence,
        })),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_lived_frame_stream_yields_each_frame_without_finishing() {
    let host = loopback_transport().await;
    let guest = loopback_transport().await;
    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let connection = host.accept_connection().await.expect("connection");
            let (_writer, mut reader) = host.accept_framed_bi(&connection).await.expect("stream");
            for epoch in 1..=3 {
                let frame = reader.read_next().await.expect("read").expect("frame");
                assert_eq!(frame, lease(epoch));
            }
        })
    };
    let connection = guest.connect(host.endpoint_addr()).await.expect("connect");
    let (mut writer, _reader) = guest.open_framed_bi(&connection).await.expect("stream");
    for epoch in 1..=3 {
        writer.write_next(&lease(epoch)).await.expect("write");
    }
    timeout(TEST_TIMEOUT, host_task)
        .await
        .expect("host should finish")
        .expect("host task");
    guest.close().await;
    host.close().await;
}
