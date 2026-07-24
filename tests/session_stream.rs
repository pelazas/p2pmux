use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    protocol::{ControlLease, Envelope, Join, PROTOCOL_VERSION, Snapshot, envelope},
    screen::HostScreen,
    session::{DEFAULT_PANE_ID, HostPaneChannels, HostSession},
    transport::{ALPN, Transport},
};
use tokio::time::timeout;

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
async fn post_welcome_screen_stream_starts_with_a_snapshot_then_sends_delta() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let guest = loopback_transport().await;
    let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
    let mut screen = HostScreen::new(1, 3).expect("screen");
    let (screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (lease_tx, lease_rx) = tokio::sync::watch::channel(ControlLease {
        pane_id: DEFAULT_PANE_ID.to_vec(),
        controller_peer_id: host_id.clone(),
        lease_epoch: 1,
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
