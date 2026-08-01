use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    lease::LeaseState,
    ledger::entry_hash,
    protocol::{
        AgentRoster, AgentRosterEntry, AgentRosterState, CreatePane, CreateTab, DeletePane,
        DeleteTab, Envelope, Join, LayoutRejectReason, LayoutRequest, MembershipRecord,
        PROTOCOL_VERSION, PaneDescriptor, PaneReady, SplitAxis, envelope,
    },
    screen::HostScreen,
    session::{
        GuestEvent, HostPaneChannels, HostSession, LayoutControlEvent, RosterStatus, SessionError,
        SharedLayoutHost, join_layout, layout_snapshot_from_state, pane_wire_id, subscribe_pane,
    },
    transport::{ALPN, Transport},
};
use prost::Message;
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
async fn control_snapshot_converts_to_a_renderable_local_layout() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut member = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("member joins");
    accept.await.expect("accept task").expect("accept member");

    let LayoutControlEvent::Snapshot(snapshot) = next_event(&mut member).await else {
        panic!("first layout event must be a snapshot");
    };
    let layout = layout_snapshot_from_state(snapshot.state.as_ref().expect("state"))
        .expect("wire layout is renderable");
    assert_eq!(layout.revision, 2);
    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.panes.len(), 1);
    assert!(
        layout
            .panes
            .get(&1)
            .is_some_and(|pane| pane.grid_rows == 24 && pane.grid_cols == 80)
    );

    member.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_carries_host_session_name_from_welcome() {
    let host = HostSession::from_transport_with_session_name(
        loopback_transport().await,
        "lisbon".to_owned(),
    )
    .expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let member = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("member joins");
    let receipt = accept.await.expect("accept task").expect("accept member");

    assert_eq!(receipt.session_name, "lisbon");
    assert_eq!(member.session_name, "lisbon");

    member.shutdown().await;
    coordinator.close().await;
}

async fn next_event(member: &mut p2pmux::session::SharedLayoutMember) -> LayoutControlEvent {
    timeout(TEST_TIMEOUT, member.events.recv())
        .await
        .expect("control event should arrive")
        .expect("control stream should remain open")
}

fn create_request(request_id: u64, base_revision: u64) -> LayoutRequest {
    LayoutRequest {
        request_id,
        base_revision,
        create_pane: Some(CreatePane {
            target_pane_id: 1,
            axis: Some(SplitAxis::LeftRight as i32),
            grid_rows: 24,
            grid_cols: 80,
            position: None,
        }),
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: None,
        update_pane_grids: None,
        rename_pane: None,
        rename_tab: None,
        set_pane_lock: None,
        mark_pane_exited: None,
        author_signature: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_member_receives_a_snapshot_and_existing_members_receive_admission_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");

    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first joins");
    accept_first
        .await
        .expect("accept task")
        .expect("accept first");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Snapshot(snapshot) if snapshot.state.as_ref().is_some_and(|state| state.revision == 2))
    );

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second joins");
    accept_second
        .await
        .expect("accept task")
        .expect("accept second");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 3)
    );
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Snapshot(snapshot) if snapshot.state.as_ref().is_some_and(|state| state.revision == 3))
    );

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_roster_relays_to_members_and_bootstraps_late_joiners() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first joins");
    accept_first.await.unwrap().unwrap();
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Snapshot(_)
    ));

    first
        .try_request(create_request(1, 2))
        .expect("first requests a pane");
    let reservation = match next_event(&mut first).await {
        LayoutControlEvent::Reservation(reservation) => reservation,
        event => panic!("expected pane reservation, got {event:?}"),
    };
    first
        .try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 1,
            author_signature: Vec::new(),
        })
        .expect("first marks pane ready");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Commit(commit) if commit.revision == 3
    ));

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second joins");
    accept_second.await.unwrap().unwrap();
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Commit(commit) if commit.revision == 4
    ));
    assert!(matches!(
        next_event(&mut second).await,
        LayoutControlEvent::Snapshot(_)
    ));

    first
        .try_agent_roster(AgentRoster {
            host_peer_id: first.peer_id.clone(),
            generation: 1,
            entries: vec![AgentRosterEntry {
                pane_id: reservation.pane_id,
                agent_kind: String::from("codex"),
                cwd: String::from("/repo"),
                state: AgentRosterState::Working as i32,
                working_since_unix_ms: 0,
            }],
        })
        .expect("first publishes roster");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::AgentRoster(AgentRoster { entries, .. }) if entries.len() == 1
    ));
    assert!(matches!(
        next_event(&mut second).await,
        LayoutControlEvent::AgentRoster(AgentRoster { entries, .. }) if entries.len() == 1
    ));

    let accept_third = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut third = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("third joins");
    accept_third.await.unwrap().unwrap();
    assert!(matches!(
        next_event(&mut third).await,
        LayoutControlEvent::Snapshot(_)
    ));
    assert!(matches!(
        next_event(&mut third).await,
        LayoutControlEvent::AgentRoster(AgentRoster { entries, .. }) if entries.len() == 1
    ));

    first.shutdown().await;
    second.shutdown().await;
    third.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_joiner_subscribes_after_snapshot_without_preloaded_host_roster() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let guest = loopback_transport().await;
    let pane_server = coordinator.pane_server();
    let host_id = coordinator.ticket().endpoint_addr().id.as_bytes().to_vec();
    let descriptor = PaneDescriptor {
        pane_id: 201,
        host_peer_id: host_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
        title: None,
        locked: false,
        exited: false,
    };
    let screen = HostScreen::new(1, 1).expect("screen");
    let (_screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 9,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, _control_rx) = tokio::sync::mpsc::channel(8);
    pane_server
        .register_pane(
            descriptor.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(201),
                host_peer_id: host_id,
                screen_rx,
                lease_rx,
                control_tx,
            },
        )
        .expect("register local pane");
    let dispatcher = coordinator
        .incoming_dispatcher(pane_server)
        .expect("matching dispatcher services");
    let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });

    let mut member = join_layout(guest.clone(), coordinator.ticket().clone())
        .await
        .expect("layout join");
    assert!(matches!(
        next_event(&mut member).await,
        LayoutControlEvent::Snapshot(_)
    ));
    let mut pane = subscribe_pane(
        guest.clone(),
        coordinator.ticket().session_id().to_vec(),
        coordinator.ticket().endpoint_addr().clone(),
        descriptor,
    )
    .await
    .expect("pane subscription after authoritative snapshot");
    let mut saw_snapshot = false;
    let mut saw_lease = false;
    for _ in 0..2 {
        match timeout(TEST_TIMEOUT, pane.events.recv())
            .await
            .expect("pane event")
        {
            Some(GuestEvent::ScreenSnapshot(_)) => saw_snapshot = true,
            Some(GuestEvent::Lease(lease)) if lease.lease_epoch == 9 => saw_lease = true,
            other => panic!("unexpected pane event: {other:?}"),
        }
    }
    assert!(
        saw_snapshot && saw_lease,
        "direct pane remains independent of layout control"
    );
    pane.shutdown().await;
    member.shutdown().await;
    dispatcher_task.abort();
    let _ = dispatcher_task.await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn departed_member_loses_direct_pane_access_while_healthy_member_receives_departure_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let pane_server = coordinator.pane_server();
    let host_id = coordinator.ticket().endpoint_addr().id.as_bytes().to_vec();
    let descriptor = PaneDescriptor {
        pane_id: 211,
        host_peer_id: host_id.clone(),
        grid_rows: 1,
        grid_cols: 1,
        title: None,
        locked: false,
        exited: false,
    };
    let screen = HostScreen::new(1, 1).expect("screen");
    let (_screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let (_lease_tx, lease_rx) = tokio::sync::watch::channel(LeaseState {
        controller_peer_id: host_id.clone(),
        epoch: 1,
        last_activity: std::time::Instant::now(),
    });
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(1);
    pane_server
        .register_pane(
            descriptor.clone(),
            HostPaneChannels {
                pane_id: pane_wire_id(211),
                host_peer_id: host_id,
                screen_rx,
                lease_rx,
                control_tx,
            },
        )
        .expect("register pane");
    let dispatcher = coordinator
        .incoming_dispatcher(pane_server)
        .expect("dispatcher");
    let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });
    let departed_transport = loopback_transport().await;
    let healthy_transport = loopback_transport().await;
    let mut departed = join_layout(departed_transport.clone(), coordinator.ticket().clone())
        .await
        .expect("departed joins");
    assert!(matches!(
        next_event(&mut departed).await,
        LayoutControlEvent::Snapshot(_)
    ));
    let mut healthy = join_layout(healthy_transport.clone(), coordinator.ticket().clone())
        .await
        .expect("healthy joins");
    assert!(matches!(
        next_event(&mut healthy).await,
        LayoutControlEvent::Snapshot(_)
    ));
    let mut pane = subscribe_pane(
        departed_transport.clone(),
        coordinator.ticket().session_id().to_vec(),
        coordinator.ticket().endpoint_addr().clone(),
        descriptor.clone(),
    )
    .await
    .expect("direct subscribe");
    for _ in 0..2 {
        let _ = timeout(TEST_TIMEOUT, pane.events.recv())
            .await
            .expect("initial pane event");
    }
    departed.disconnect_control();
    assert!(
        matches!(next_event(&mut healthy).await, LayoutControlEvent::Commit(commit) if commit.state.as_ref().is_some_and(|state| !state.members.iter().any(|member| member.peer_id == departed.peer_id)))
    );
    let mut disconnected = false;
    for _ in 0..2 {
        if matches!(
            timeout(TEST_TIMEOUT, pane.events.recv())
                .await
                .expect("disconnect event"),
            Some(GuestEvent::Disconnected)
        ) {
            disconnected = true;
            break;
        }
    }
    assert!(disconnected, "departure must close the direct pane stream");
    let _ = pane.controls.try_input(1, b"late".to_vec());
    assert!(
        !matches!(
            timeout(Duration::from_millis(100), control_rx.recv()).await,
            Ok(Some(_))
        ),
        "departure must block late input"
    );
    assert!(
        subscribe_pane(
            departed_transport.clone(),
            coordinator.ticket().session_id().to_vec(),
            coordinator.ticket().endpoint_addr().clone(),
            descriptor
        )
        .await
        .is_err(),
        "departed peer cannot resubscribe"
    );
    pane.shutdown().await;
    departed_transport.close().await;
    healthy.shutdown().await;
    dispatcher_task.abort();
    let _ = dispatcher_task.await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_post_welcome_sender_is_rejected_without_a_layout_mutation() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let raw = loopback_transport().await;
    let raw_id = raw.endpoint_id().as_bytes().to_vec();
    let connection = raw
        .connect(coordinator.ticket().endpoint_addr().clone())
        .await
        .expect("connect");
    let (mut handshake_send, mut handshake_recv) = raw.open_bi(&connection).await.expect("join");
    raw.write_frame(
        &mut handshake_send,
        &Envelope {
            version: PROTOCOL_VERSION,
            sender_peer_id: raw_id.clone(),
            body: Some(envelope::Body::Join(Join {
                session_id: coordinator.ticket().session_id().to_vec(),
                peer_id: raw_id.clone(),
                endpoint_addr: serde_json::to_vec(&raw.endpoint_addr()).expect("endpoint"),
                display_name: String::new(),
            })),
        },
    )
    .await
    .expect("send Join");
    let _welcome = raw.read_frame(&mut handshake_recv).await.expect("welcome");
    let (mut writer, mut reader) = raw
        .accept_framed_bi(&connection)
        .await
        .expect("control stream");
    assert!(matches!(
        reader.read_next().await.expect("snapshot read"),
        Some(Envelope {
            body: Some(envelope::Body::SessionSnapshot(_)),
            ..
        })
    ));
    accept_first.await.unwrap().unwrap();

    writer
        .write_next(&Envelope {
            version: PROTOCOL_VERSION,
            sender_peer_id: b"forged-peer".to_vec(),
            body: Some(envelope::Body::LayoutRequest(LayoutRequest {
                request_id: 77,
                base_revision: 2,
                create_pane: None,
                delete_pane: None,
                create_tab: Some(CreateTab {
                    grid_rows: 24,
                    grid_cols: 80,
                }),
                delete_tab: None,
                set_split_ratio: None,
                update_pane_grids: None,

                rename_pane: None,
                rename_tab: None,
                set_pane_lock: None,
                mark_pane_exited: None,
                author_signature: Vec::new(),
            })),
        })
        .await
        .expect("write forged request");
    let forged_outcome = timeout(TEST_TIMEOUT, reader.read_next()).await;
    assert!(
        matches!(forged_outcome, Ok(Ok(None)) | Ok(Err(_))),
        "forged outcome: {forged_outcome:?}; the coordinator must close a forged control stream without replying"
    );

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second joins");
    accept_second.await.unwrap().unwrap();
    assert!(matches!(
        next_event(&mut second).await,
        LayoutControlEvent::Snapshot(snapshot)
            if snapshot.state.as_ref().is_some_and(|state| state.revision == 4 && state.tabs.len() == 1 && state.panes.len() == 1)
    ));

    connection.close(0u8.into(), b"");
    raw.close().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_member_owned_tab_broadcasts_the_full_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second");
    accept_second.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let _ = next_event(&mut second).await;

    second
        .try_request(LayoutRequest {
            request_id: 12,
            base_revision: 3,
            create_pane: None,
            delete_pane: None,
            create_tab: Some(CreateTab {
                grid_rows: 24,
                grid_cols: 80,
            }),
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,

            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        })
        .expect("queue tab request");
    let reservation = match next_event(&mut second).await {
        LayoutControlEvent::Reservation(reservation) => reservation,
        event => panic!("expected tab reservation, got {event:?}"),
    };
    second
        .try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 3,
            request_id: 12,
            author_signature: Vec::new(),
        })
        .expect("ready tab");
    let commit = match next_event(&mut first).await {
        LayoutControlEvent::Commit(commit) => commit,
        event => panic!("expected tab commit, got {event:?}"),
    };
    assert_eq!(commit.revision, 4);
    assert_eq!(commit.state.as_ref().expect("state").tabs.len(), 2);
    let _ = next_event(&mut second).await;

    second
        .try_request(LayoutRequest {
            request_id: 13,
            base_revision: 4,
            create_pane: None,
            delete_pane: None,
            create_tab: None,
            delete_tab: Some(DeleteTab {
                tab_id: reservation.tab_id.expect("tab reservation"),
            }),
            set_split_ratio: None,
            update_pane_grids: None,

            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        })
        .expect("queue tab delete");
    for member in [&mut first, &mut second] {
        let commit = match next_event(member).await {
            LayoutControlEvent::Commit(commit) => commit,
            event => panic!("expected delete commit, got {event:?}"),
        };
        assert_eq!(commit.revision, 5);
        assert_eq!(commit.state.expect("full state").tabs.len(), 1);
    }

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reservation_is_targeted_and_ready_broadcasts_the_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second");
    accept_second.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let _ = next_event(&mut second).await;

    second
        .try_request(create_request(7, 3))
        .expect("queue request");
    let reservation = match next_event(&mut second).await {
        LayoutControlEvent::Reservation(reservation) => reservation,
        event => panic!("expected reservation, got {event:?}"),
    };
    assert!(
        timeout(Duration::from_millis(150), first.events.recv())
            .await
            .is_err(),
        "reservation must be targeted"
    );
    second
        .try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 3,
            request_id: 7,
            author_signature: Vec::new(),
        })
        .expect("queue ready");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 4)
    );
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Commit(commit) if commit.revision == 4)
    );

    second
        .try_request(LayoutRequest {
            request_id: 8,
            base_revision: 4,
            create_pane: None,
            delete_pane: Some(DeletePane {
                pane_id: reservation.pane_id,
            }),
            create_tab: None,
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,

            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        })
        .expect("queue deletion");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 5)
    );
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Commit(commit) if commit.revision == 5)
    );

    first
        .try_request(LayoutRequest {
            request_id: 9,
            base_revision: 5,
            create_pane: None,
            delete_pane: Some(DeletePane { pane_id: 1 }),
            create_tab: None,
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,

            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        })
        .expect("queue foreign deletion");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Reject(reject)
            if reject.request_id == 9 && reject.reason == LayoutRejectReason::NotHost as i32
    ));

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_requests_are_rejected_only_for_the_requester() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut member = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("member");
    accept.await.unwrap().unwrap();
    let _ = next_event(&mut member).await;

    member
        .try_request(create_request(8, 1))
        .expect("queue request");
    assert!(
        matches!(next_event(&mut member).await, LayoutControlEvent::Reject(reject) if reject.request_id == 8 && reject.reason == LayoutRejectReason::Stale as i32)
    );
    member.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_reservation_rejects_its_creator_and_unblocks_the_next_request() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::with_reservation_timeout(host, 24, 80, Duration::ZERO)
        .expect("shared host");
    let accept = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut member = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("member joins");
    accept.await.unwrap().unwrap();
    let _ = next_event(&mut member).await;

    member
        .try_request(create_request(21, 2))
        .expect("queue creation");
    assert!(matches!(
        next_event(&mut member).await,
        LayoutControlEvent::Reservation(_)
    ));
    assert!(matches!(
        next_event(&mut member).await,
        LayoutControlEvent::Reject(reject)
            if reject.request_id == 21 && reject.reason == LayoutRejectReason::ReservationFailure as i32
    ));
    member
        .try_request(create_request(22, 2))
        .expect("reservation expiry unblocks a new request");
    assert!(matches!(
        next_event(&mut member).await,
        LayoutControlEvent::Reservation(_)
    ));

    member.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_member_requests_publish_only_monotonic_commits() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second");
    accept_second.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let _ = next_event(&mut second).await;

    first
        .try_request(create_request(31, 3))
        .expect("first request");
    second
        .try_request(create_request(32, 3))
        .expect("second request");
    let first_result = next_event(&mut first).await;
    let second_result = next_event(&mut second).await;
    let reservation = match (first_result, second_result) {
        (LayoutControlEvent::Reservation(reservation), LayoutControlEvent::Reject(_)) => {
            first
                .try_ready(PaneReady {
                    reservation_id: reservation.reservation_id,
                    base_revision: 3,
                    request_id: 31,
                    author_signature: Vec::new(),
                })
                .expect("first ready");
            reservation
        }
        (LayoutControlEvent::Reject(_), LayoutControlEvent::Reservation(reservation)) => {
            second
                .try_ready(PaneReady {
                    reservation_id: reservation.reservation_id,
                    base_revision: 3,
                    request_id: 32,
                    author_signature: Vec::new(),
                })
                .expect("second ready");
            reservation
        }
        events => panic!("expected one reservation and one reject, got {events:?}"),
    };
    assert_ne!(reservation.reservation_id, 0);
    for member in [&mut first, &mut second] {
        assert!(matches!(
            next_event(member).await,
            LayoutControlEvent::Commit(commit) if commit.revision == 4
        ));
    }

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_after_welcome_rolls_back_admission_before_control_setup() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first joins");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;

    let accept_raw = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let raw = loopback_transport().await;
    let raw_id = raw.endpoint_id().as_bytes().to_vec();
    let connection = raw
        .connect(coordinator.ticket().endpoint_addr().clone())
        .await
        .expect("connect");
    let (mut send, mut recv) = raw.open_bi(&connection).await.expect("handshake");
    raw.write_frame(
        &mut send,
        &Envelope {
            version: PROTOCOL_VERSION,
            sender_peer_id: raw_id.clone(),
            body: Some(envelope::Body::Join(Join {
                session_id: coordinator.ticket().session_id().to_vec(),
                peer_id: raw_id,
                endpoint_addr: serde_json::to_vec(&raw.endpoint_addr()).expect("endpoint"),
                display_name: String::new(),
            })),
        },
    )
    .await
    .expect("Join");
    let _ = raw.read_frame(&mut recv).await.expect("Welcome");
    connection.close(0u8.into(), b"");
    raw.close().await;
    let _ = accept_raw.await.unwrap();
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Commit(commit) if commit.revision == 3
    ));
    let rollback = match next_event(&mut first).await {
        LayoutControlEvent::Commit(commit) => commit,
        event => panic!("expected rollback commit, got {event:?}"),
    };
    assert_eq!(rollback.revision, 4);
    assert_eq!(rollback.state.expect("state").members.len(), 2);

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second joins after rollback");
    accept_second.await.unwrap().unwrap();
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Commit(commit) if commit.revision == 5
    ));
    assert!(matches!(
        next_event(&mut second).await,
        LayoutControlEvent::Snapshot(snapshot)
            if snapshot.state.as_ref().is_some_and(|state| state.revision == 5 && state.members.len() == 3)
    ));

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_invalidates_a_member_reservation_and_notifies_its_creator() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    first
        .try_request(create_request(10, 2))
        .expect("queue reservation request");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Reservation(_)
    ));

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second");
    accept_second.await.unwrap().unwrap();
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 3)
    );
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Reject(reject)
            if reject.request_id == 10 && reject.reason == LayoutRejectReason::Stale as i32
    ));
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Snapshot(snapshot) if snapshot.state.as_ref().is_some_and(|state| state.revision == 3))
    );

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

/// Drain control events until the member's stream reports it has been dropped.
///
/// The eviction is announced by closing the connection, and whatever commits were already
/// in flight arrive first, so the test cannot assume `Disconnected` is the very next event.
async fn next_disconnect(member: &mut p2pmux::session::SharedLayoutMember) {
    for _ in 0..16 {
        if matches!(next_event(member).await, LayoutControlEvent::Disconnected) {
            return;
        }
    }
    panic!("member should have been disconnected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_a_member_evicts_it_and_refuses_the_key_afterwards() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let member_transport = loopback_transport().await;
    let mut member = join_layout(member_transport.clone(), coordinator.ticket().clone())
        .await
        .expect("member joins");
    accept.await.expect("accept task").expect("accept member");
    let member_id = member.peer_id.clone();

    assert!(
        coordinator.revoke_member(&member_id).expect("revoke"),
        "revoking a seated member changes the roster"
    );
    next_disconnect(&mut member).await;

    // The session is never locked in this test: a revoked key has to be turned away on its
    // own account, or "kick" would only work while the door was shut to everyone.
    assert!(!coordinator.is_session_locked().expect("lock state"));
    let accept_again = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let rejoin = join_layout(member_transport.clone(), coordinator.ticket().clone()).await;
    assert!(
        matches!(rejoin, Err(SessionError::MembershipRevoked)),
        "a revoked key must be told why, not merely dropped"
    );
    assert!(matches!(
        accept_again.await.expect("accept task"),
        Err(SessionError::MembershipRevoked)
    ));

    assert_eq!(
        coordinator
            .roster()
            .expect("roster")
            .into_iter()
            .find(|(peer_id, _)| peer_id == &member_id)
            .map(|(_, status)| status),
        Some(RosterStatus::Revoked)
    );

    drop(member);
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_the_coordinators_own_key_is_refused() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let own_key = coordinator.ticket().endpoint_addr().id.as_bytes().to_vec();

    assert!(
        !coordinator.revoke_member(&own_key).expect("revoke"),
        "a session whose only authority cannot rejoin it is not a session"
    );
    assert!(coordinator.roster().expect("roster").is_empty());

    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_receives_a_chained_ledger_alongside_every_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");

    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first joins");
    accept_first
        .await
        .expect("accept task")
        .expect("accept first");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Snapshot(_)
    ));

    // A second member joining, then leaving, is two changes the coordinator authored on its
    // own account -- the pair the first member has to be able to follow.
    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second joins");
    accept_second
        .await
        .expect("accept task")
        .expect("accept second");
    let second_id = second.peer_id.clone();

    let LayoutControlEvent::Commit(admission) = next_event(&mut first).await else {
        panic!("the first member should see the second arrive");
    };
    coordinator.revoke_member(&second_id).expect("revoke");
    let LayoutControlEvent::Commit(departure) = next_event(&mut first).await else {
        panic!("the first member should see the second leave");
    };

    let admission = admission.entry.expect("admission is sealed");
    let departure = departure.entry.expect("departure is sealed");
    assert_eq!(departure.seq, admission.seq + 1);
    assert_eq!(
        departure.prev_hash,
        entry_hash(coordinator.ticket().session_id(), &admission).to_vec(),
        "each entry names the one before it, so a dropped commit cannot pass unnoticed"
    );
    assert_eq!(
        MembershipRecord::decode(departure.payload.as_slice())
            .expect("payload is a membership record")
            .peer_id,
        second_id
    );

    drop(second);
    first.shutdown().await;
    coordinator.close().await;
}
