use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    lease::LeaseState,
    protocol::{
        ControlLease, Envelope, Input, Join, PROTOCOL_VERSION, PaneDescriptor, PaneSubscribe,
        Snapshot, envelope,
    },
    screen::HostScreen,
    session::{
        DEFAULT_PANE_ID, GuestEvent, HostPaneChannels, HostSession, PaneServer, join_pane,
        pane_wire_id, subscribe_pane,
    },
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

fn encoded_endpoint_addr(transport: &Transport) -> Vec<u8> {
    serde_json::to_vec(&transport.endpoint_addr()).expect("endpoint address should serialize")
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
        matches!(timeout(TEST_TIMEOUT, pane.events.recv()).await.expect("lease timeout"), Some(GuestEvent::InitialLease(lease)) if lease.lease_epoch == 1)
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
async fn direct_subscription_streams_a_registered_non_default_pane_without_synthetic_lease() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let guest = loopback_transport().await;
    let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
    let pane_id = 42;
    let descriptor = PaneDescriptor {
        pane_id,
        host_peer_id: host_id.clone(),
        grid_rows: 1,
        grid_cols: 3,
    };
    let mut screen = HostScreen::new(1, 3).expect("screen");
    let (screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 7,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel(8);
    let server = host.pane_server();
    server
        .add_member(
            guest.endpoint_id().as_bytes().to_vec(),
            guest.endpoint_addr(),
        )
        .expect("admit guest");
    server
        .register_pane(
            descriptor.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(pane_id),
                host_peer_id: host_id,
                screen_rx,
                lease_rx,
                control_tx,
            },
        )
        .expect("register pane");
    let server_task = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_one().await })
    };

    let mut pane = subscribe_pane(
        guest.clone(),
        host.ticket().session_id().to_vec(),
        host.ticket().endpoint_addr().clone(),
        descriptor,
    )
    .await
    .expect("subscribe");
    assert_eq!(pane.pane_id, pane_wire_id(pane_id));
    // The first two events can arrive in either stream order, but they must describe this pane.
    let mut saw_snapshot = false;
    let mut saw_lease = false;
    for _ in 0..4 {
        match timeout(TEST_TIMEOUT, pane.events.recv())
            .await
            .expect("event timeout")
        {
            Some(GuestEvent::ScreenSnapshot(snapshot)) => {
                assert_eq!(snapshot.pane_id, pane_wire_id(pane_id));
                saw_snapshot = true;
            }
            Some(GuestEvent::Lease(lease)) => {
                assert_eq!(lease.pane_id, pane_wire_id(pane_id));
                assert_eq!(lease.lease_epoch, 7);
                saw_lease = true;
            }
            Some(GuestEvent::InitialLease(_)) => {
                panic!("direct subscriptions must not synthesize a lease")
            }
            Some(GuestEvent::Disconnected) => panic!("direct pane disconnected before its lease"),
            other => panic!("unexpected direct pane event: {other:?}"),
        }
        if saw_snapshot && saw_lease {
            break;
        }
    }
    assert!(
        saw_snapshot && saw_lease,
        "host must send snapshot and actual lease"
    );
    screen_tx.send_replace(screen.process_pty(b"abc").expect("screen update"));
    loop {
        if matches!(timeout(TEST_TIMEOUT, pane.events.recv()).await.expect("delta timeout"), Some(GuestEvent::ScreenDelta(delta)) if delta.sequence == 2)
        {
            break;
        }
    }
    pane.shutdown().await;
    timeout(TEST_TIMEOUT, server_task)
        .await
        .expect("server task")
        .expect("server join")
        .expect("serve pane");
    guest.close().await;
    host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_viewer_subscribes_to_panes_owned_by_two_different_hosts() {
    let first_host = loopback_transport().await;
    let second_host = loopback_transport().await;
    let viewer = loopback_transport().await;
    let session_id = b"shared-session".to_vec();
    let first_id = first_host.endpoint_id().as_bytes().to_vec();
    let second_id = second_host.endpoint_id().as_bytes().to_vec();
    let viewer_id = viewer.endpoint_id().as_bytes().to_vec();
    let first_server =
        PaneServer::new(first_host.clone(), session_id.clone()).expect("first server");
    let second_server =
        PaneServer::new(second_host.clone(), session_id.clone()).expect("second server");
    first_server
        .add_member(viewer_id.clone(), viewer.endpoint_addr())
        .expect("admit viewer to first host");
    second_server
        .add_member(viewer_id, viewer.endpoint_addr())
        .expect("admit viewer to second host");

    let first_descriptor = PaneDescriptor {
        pane_id: 11,
        host_peer_id: first_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
    };
    let second_descriptor = PaneDescriptor {
        pane_id: 12,
        host_peer_id: second_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
    };
    let first_screen = HostScreen::new(1, 1).expect("first screen");
    let second_screen = HostScreen::new(1, 1).expect("second screen");
    let (_first_screen_tx, first_screen_rx) =
        tokio::sync::watch::channel(first_screen.current_frame().clone());
    let (_second_screen_tx, second_screen_rx) =
        tokio::sync::watch::channel(second_screen.current_frame().clone());
    let (_first_lease_tx, first_lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: first_id.clone(),
        epoch: 3,
        last_activity: std::time::Instant::now(),
    });
    let (_second_lease_tx, second_lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: second_id.clone(),
        epoch: 4,
        last_activity: std::time::Instant::now(),
    });
    let (first_control_tx, _first_control_rx) = tokio::sync::mpsc::channel(8);
    let (second_control_tx, _second_control_rx) = tokio::sync::mpsc::channel(8);
    first_server
        .register_pane(
            first_descriptor.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(11),
                host_peer_id: first_id.clone(),
                screen_rx: first_screen_rx,
                lease_rx: first_lease_rx,
                control_tx: first_control_tx,
            },
        )
        .expect("register first pane");
    second_server
        .register_pane(
            second_descriptor.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(12),
                host_peer_id: second_id.clone(),
                screen_rx: second_screen_rx,
                lease_rx: second_lease_rx,
                control_tx: second_control_tx,
            },
        )
        .expect("register second pane");
    let first_task = {
        let server = first_server.clone();
        tokio::spawn(async move { server.accept_one().await })
    };
    let second_task = {
        let server = second_server.clone();
        tokio::spawn(async move { server.accept_one().await })
    };

    let mut first_pane = subscribe_pane(
        viewer.clone(),
        session_id.clone(),
        first_host.endpoint_addr(),
        first_descriptor,
    )
    .await
    .expect("subscribe first pane");
    let mut second_pane = subscribe_pane(
        viewer.clone(),
        session_id,
        second_host.endpoint_addr(),
        second_descriptor,
    )
    .await
    .expect("subscribe second pane");
    assert_eq!(first_pane.host_peer_id, first_id);
    assert_eq!(second_pane.host_peer_id, second_id);
    for pane in [&mut first_pane, &mut second_pane] {
        let mut saw_snapshot = false;
        let mut saw_lease = false;
        for _ in 0..2 {
            match timeout(TEST_TIMEOUT, pane.events.recv())
                .await
                .expect("pane event")
            {
                Some(GuestEvent::ScreenSnapshot(_)) => saw_snapshot = true,
                Some(GuestEvent::Lease(_)) => saw_lease = true,
                other => panic!("unexpected pane event: {other:?}"),
            }
        }
        assert!(
            saw_snapshot && saw_lease,
            "each host serves its own pane streams"
        );
    }
    first_pane.shutdown().await;
    second_pane.shutdown().await;
    timeout(TEST_TIMEOUT, first_task)
        .await
        .expect("first server task")
        .expect("first task join")
        .expect("first serve");
    timeout(TEST_TIMEOUT, second_task)
        .await
        .expect("second server task")
        .expect("second task join")
        .expect("second serve");
    viewer.close().await;
    first_host.close().await;
    second_host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_subscriptions_reject_unknown_nonmember_and_mismatched_hosts() {
    let host = loopback_transport().await;
    let member = loopback_transport().await;
    let outsider = loopback_transport().await;
    let server = PaneServer::new(host.clone(), b"shared-session".to_vec()).expect("server");
    let host_id = host.endpoint_id().as_bytes().to_vec();
    server
        .add_member(
            member.endpoint_id().as_bytes().to_vec(),
            member.endpoint_addr(),
        )
        .expect("member");
    let unknown = PaneDescriptor {
        pane_id: 91,
        host_peer_id: host_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
    };
    let unknown_task = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_one().await })
    };
    assert!(
        subscribe_pane(
            member.clone(),
            b"shared-session".to_vec(),
            host.endpoint_addr(),
            unknown
        )
        .await
        .is_err()
    );
    assert!(
        timeout(TEST_TIMEOUT, unknown_task)
            .await
            .expect("unknown task")
            .expect("unknown join")
            .is_err()
    );

    let screen = HostScreen::new(1, 1).expect("screen");
    let (_screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 1,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel(1);
    let registered = PaneDescriptor {
        pane_id: 92,
        host_peer_id: host_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
    };
    server
        .register_pane(
            registered.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(92),
                host_peer_id: host_id.clone(),
                screen_rx,
                lease_rx,
                control_tx,
            },
        )
        .expect("register");
    let outsider_task = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_one().await })
    };
    assert!(
        subscribe_pane(
            outsider.clone(),
            b"shared-session".to_vec(),
            host.endpoint_addr(),
            registered.clone()
        )
        .await
        .is_err()
    );
    assert!(
        timeout(TEST_TIMEOUT, outsider_task)
            .await
            .expect("outsider task")
            .expect("outsider join")
            .is_err()
    );

    let mismatched = PaneDescriptor {
        host_peer_id: outsider.endpoint_id().as_bytes().to_vec(),
        ..registered
    };
    assert!(
        subscribe_pane(
            member.clone(),
            b"shared-session".to_vec(),
            host.endpoint_addr(),
            mismatched
        )
        .await
        .is_err()
    );
    member.close().await;
    outsider.close().await;
    host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_direct_subscriber_cannot_inject_control_into_a_registered_pane() {
    let host = loopback_transport().await;
    let attacker = loopback_transport().await;
    let server = PaneServer::new(host.clone(), b"shared-session".to_vec()).expect("server");
    let host_id = host.endpoint_id().as_bytes().to_vec();
    server
        .add_member(
            attacker.endpoint_id().as_bytes().to_vec(),
            attacker.endpoint_addr(),
        )
        .expect("member");
    let screen = HostScreen::new(1, 1).expect("screen");
    let (_screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 1,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(1);
    server
        .register_pane(
            PaneDescriptor {
                pane_id: 101,
                host_peer_id: host_id.clone(),
                grid_rows: 1,
                grid_cols: 1,
            },
            HostPaneChannels {
                pane_id: pane_wire_id(101),
                host_peer_id: host_id,
                screen_rx,
                lease_rx,
                control_tx,
            },
        )
        .expect("register");
    let server_task = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_one().await })
    };
    let connection = attacker
        .connect(host.endpoint_addr())
        .await
        .expect("connect");
    let attacker_id = attacker.endpoint_id().as_bytes().to_vec();
    let (mut subscribe_writer, _subscribe_reader) = attacker
        .open_bi(&connection)
        .await
        .expect("subscribe stream");
    attacker
        .write_frame(
            &mut subscribe_writer,
            &Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: attacker_id.clone(),
                body: Some(envelope::Body::PaneSubscribe(PaneSubscribe {
                    session_id: b"shared-session".to_vec(),
                    peer_id: attacker_id.clone(),
                    pane_id: 102,
                })),
            },
        )
        .await
        .expect("send rejected subscription");
    if let Ok((mut control_writer, _)) = attacker.open_framed_bi(&connection).await {
        let _ = control_writer
            .write_next(&Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: attacker_id,
                body: Some(envelope::Body::Input(Input {
                    pane_id: pane_wire_id(101),
                    lease_epoch: 1,
                    data: b"bad".to_vec(),
                })),
            })
            .await;
    }
    assert!(
        timeout(TEST_TIMEOUT, server_task)
            .await
            .expect("server task")
            .expect("server join")
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(100), control_rx.recv())
            .await
            .is_err(),
        "unregistered subscription must not reach pane control"
    );
    connection.close(0u8.into(), b"");
    attacker.close().await;
    host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pane_server_accept_loop_serves_two_viewers_without_waiting_for_the_first() {
    let host = loopback_transport().await;
    let first_viewer = loopback_transport().await;
    let second_viewer = loopback_transport().await;
    let server = PaneServer::new(host.clone(), b"shared-session".to_vec()).expect("server");
    let host_id = host.endpoint_id().as_bytes().to_vec();
    server
        .add_member(
            first_viewer.endpoint_id().as_bytes().to_vec(),
            first_viewer.endpoint_addr(),
        )
        .expect("first member");
    server
        .add_member(
            second_viewer.endpoint_id().as_bytes().to_vec(),
            second_viewer.endpoint_addr(),
        )
        .expect("second member");
    let screen = HostScreen::new(1, 1).expect("screen");
    let (_screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 5,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel(8);
    let descriptor = PaneDescriptor {
        pane_id: 151,
        host_peer_id: host_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
    };
    server
        .register_pane(
            descriptor.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(151),
                host_peer_id: host_id,
                screen_rx,
                lease_rx,
                control_tx,
            },
        )
        .expect("register pane");
    let accept_loop = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_loop().await })
    };
    let (first, second) = tokio::join!(
        subscribe_pane(
            first_viewer.clone(),
            b"shared-session".to_vec(),
            host.endpoint_addr(),
            descriptor.clone()
        ),
        subscribe_pane(
            second_viewer.clone(),
            b"shared-session".to_vec(),
            host.endpoint_addr(),
            descriptor
        )
    );
    let mut first = first.expect("first subscription");
    let mut second = second.expect("second subscription");
    for pane in [&mut first, &mut second] {
        let mut saw_snapshot = false;
        let mut saw_lease = false;
        for _ in 0..2 {
            match timeout(TEST_TIMEOUT, pane.events.recv())
                .await
                .expect("pane event")
            {
                Some(GuestEvent::ScreenSnapshot(_)) => saw_snapshot = true,
                Some(GuestEvent::Lease(lease)) if lease.lease_epoch == 5 => saw_lease = true,
                other => panic!("unexpected pane event: {other:?}"),
            }
        }
        assert!(
            saw_snapshot && saw_lease,
            "each viewer gets independent streams"
        );
    }
    first.shutdown().await;
    second.shutdown().await;
    accept_loop.abort();
    let _ = accept_loop.await;
    first_viewer.close().await;
    second_viewer.close().await;
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
    let guest_endpoint_addr = encoded_endpoint_addr(&guest);
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
                    endpoint_addr: guest_endpoint_addr,
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
    let guest_endpoint_addr = encoded_endpoint_addr(&guest);
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
                    endpoint_addr: guest_endpoint_addr,
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
