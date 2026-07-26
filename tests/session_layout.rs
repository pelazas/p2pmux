use iroh::{EndpointAddr, SecretKey};
use p2pmux::{
    protocol::{
        AgentRoster, AgentRosterEntry, AgentRosterState, CreatePane, CreateTab, DeletePane,
        DeleteTab, LayoutCommit, LayoutRejectReason, LayoutRequest, NewPanePosition, PaneFailed,
        PaneReady, SplitAxis,
    },
    session::{CoordinatorError, CoordinatorResponse, LayoutCoordinator},
};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

fn endpoint(seed: u8, port: u16) -> EndpointAddr {
    EndpointAddr::new(SecretKey::from_bytes(&[seed; 32]).public())
        .with_ip_addr(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn host_a() -> Vec<u8> {
    endpoint(1, 4101).id.as_bytes().to_vec()
}
fn host_b() -> Vec<u8> {
    endpoint(2, 4102).id.as_bytes().to_vec()
}
fn addr_a() -> EndpointAddr {
    endpoint(1, 4101)
}
fn addr_b() -> EndpointAddr {
    endpoint(2, 4102)
}

fn coordinator() -> LayoutCoordinator {
    LayoutCoordinator::new(host_a(), addr_a(), 24, 80).expect("valid coordinator")
}

fn request(request_id: u64, base_revision: u64) -> LayoutRequest {
    LayoutRequest {
        request_id,
        base_revision,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
    }
}

fn commit(response: CoordinatorResponse) -> LayoutCommit {
    match response {
        CoordinatorResponse::Commit(commit) => commit,
        other => panic!("expected commit, got {other:?}"),
    }
}

fn reject(response: CoordinatorResponse) -> p2pmux::protocol::LayoutReject {
    match response {
        CoordinatorResponse::Reject(reject) => reject,
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn initial_snapshot_faithfully_converts_the_authoritative_layout() {
    let coordinator = coordinator();
    let snapshot = coordinator.session_snapshot().expect("snapshot converts");
    let state = snapshot.state.expect("state present");

    assert_eq!(state.revision, 1);
    assert_eq!(state.members.len(), 1);
    assert_eq!(state.members[0].peer_id, host_a());
    assert_eq!(
        state.members[0].endpoint_addr,
        serde_json::to_vec(&addr_a()).unwrap()
    );
    assert_eq!(state.panes.len(), 1);
    assert_eq!(state.panes[0].pane_id, 1);
    assert_eq!(state.panes[0].host_peer_id, host_a());
    assert_eq!(
        (state.panes[0].grid_rows, state.panes[0].grid_cols),
        (24, 80)
    );
    assert_eq!(state.tabs.len(), 1);
    assert_eq!(state.tabs[0].tab_id, 1);
    assert_eq!(state.tabs[0].root.as_ref().unwrap().leaf_pane_id, Some(1));
}

#[test]
fn admission_advances_revision_and_publishes_the_member_endpoint() {
    let mut coordinator = coordinator();
    let commit = coordinator
        .admit(host_b(), addr_b())
        .expect("guest is admitted");

    assert_eq!(commit.commit.revision, 2);
    let state = commit.commit.state.expect("state present");
    assert!(state.members.iter().any(|member| member.peer_id == host_b()
        && member.endpoint_addr == serde_json::to_vec(&addr_b()).unwrap()));
}

#[test]
fn agent_rosters_replace_per_host_and_reject_other_hosts_panes() {
    let mut coordinator = coordinator();
    coordinator.admit(host_b(), addr_b()).expect("guest admitted");

    let forged = AgentRoster {
        host_peer_id: host_a(),
        generation: 1,
        entries: vec![AgentRosterEntry {
            pane_id: 1,
            agent_kind: String::from("codex"),
            cwd: String::from("/forged"),
            state: AgentRosterState::Working as i32,
        }],
    };
    assert_eq!(coordinator.accept_agent_roster(&host_b(), forged), None);

    let accepted = coordinator
        .accept_agent_roster(
            &host_a(),
            AgentRoster {
                host_peer_id: b"mismatch-is-overwritten".to_vec(),
                generation: 1,
                entries: vec![AgentRosterEntry {
                    pane_id: 1,
                    agent_kind: String::from("codex"),
                    cwd: String::from("/repo"),
                    state: AgentRosterState::Working as i32,
                }],
            },
        )
        .expect("host roster accepted");
    assert_eq!(accepted.host_peer_id, host_a());
    assert_eq!(coordinator.agent_rosters(), vec![accepted]);

    let cleared = coordinator
        .accept_agent_roster(
            &host_a(),
            AgentRoster {
                host_peer_id: host_a(),
                generation: 2,
                entries: Vec::new(),
            },
        )
        .expect("empty replacement clears host rows");
    assert!(cleared.entries.is_empty());
    assert_eq!(coordinator.agent_rosters(), vec![cleared]);
}

#[test]
fn pane_creation_is_hidden_until_its_creator_marks_the_reservation_ready() {
    let mut coordinator = coordinator();
    let mut create = request(10, 1);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::LeftRight as i32),
        grid_rows: 30,
        grid_cols: 100,
        position: None,
    });

    let reservation = match coordinator.handle_request(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    assert_eq!(
        coordinator
            .session_snapshot()
            .unwrap()
            .state
            .unwrap()
            .panes
            .len(),
        1
    );

    let commit = commit(coordinator.handle_pane_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 10,
        },
    ));
    let state = commit.state.expect("state present");
    assert_eq!(commit.revision, 2);
    assert!(
        state
            .panes
            .iter()
            .any(|pane| pane.pane_id == reservation.pane_id)
    );
}

#[test]
fn pane_creation_maps_protocol_placement_to_authoritative_child_order() {
    let mut coordinator = coordinator();
    let mut create = request(15, 1);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::LeftRight as i32),
        grid_rows: 24,
        grid_cols: 80,
        position: Some(NewPanePosition::First as i32),
    });
    let reservation = match coordinator.handle_request(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let state = commit(coordinator.handle_pane_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 15,
        },
    ))
    .state
    .expect("state present");
    let split = state.tabs[0]
        .root
        .as_ref()
        .expect("root present")
        .split
        .as_ref();
    let split = split.expect("split present");
    assert_eq!(
        split.first.as_ref().unwrap().leaf_pane_id,
        Some(reservation.pane_id)
    );
    assert_eq!(split.second.as_ref().unwrap().leaf_pane_id, Some(1));
}

#[test]
fn ready_cannot_commit_a_reservation_after_membership_advances_its_revision() {
    let mut coordinator = coordinator();
    let mut create = request(101, 1);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::LeftRight as i32),
        grid_rows: 30,
        grid_cols: 100,
        position: None,
    });
    let reservation = match coordinator.handle_request(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    let membership = coordinator.admit(host_b(), addr_b()).unwrap();
    let invalidation = membership
        .invalidated_reservation
        .expect("reservation invalidated");
    assert_eq!(invalidation.peer_id, host_a());
    assert_eq!(invalidation.reject.request_id, 101);
    assert_eq!(invalidation.reject.reason, LayoutRejectReason::Stale as i32);

    let rejection = reject(coordinator.handle_pane_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 101,
        },
    ));
    assert_eq!(rejection.request_id, 101);
    assert_eq!(
        rejection.reason,
        LayoutRejectReason::ReservationFailure as i32
    );
    let state = coordinator.session_snapshot().unwrap().state.unwrap();
    assert_eq!(state.revision, 2);
    assert_eq!(state.tabs.len(), 1);
    assert_eq!(state.panes.len(), 1);
    let mut next = request(102, 2);
    next.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    assert!(matches!(
        coordinator.handle_request(&host_a(), next),
        CoordinatorResponse::Reservation(_)
    ));
}

#[test]
fn admitted_guest_hosts_its_own_pane_after_ready() {
    let mut coordinator = coordinator();
    coordinator.admit(host_b(), addr_b()).unwrap();
    let mut create = request(11, 2);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::TopBottom as i32),
        grid_rows: 31,
        grid_cols: 101,
        position: None,
    });
    let reservation = match coordinator.handle_request(&host_b(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let commit = commit(coordinator.handle_pane_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 11,
        },
    ));
    let state = commit.state.unwrap();
    let pane = state
        .panes
        .iter()
        .find(|pane| pane.pane_id == reservation.pane_id)
        .expect("reserved pane committed");
    assert_eq!(pane.host_peer_id, host_b());
    assert_eq!((pane.grid_rows, pane.grid_cols), (31, 101));
}

#[test]
fn stale_request_is_rejected_without_a_layout_change() {
    let mut coordinator = coordinator();
    let mut create = request(12, 99);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });

    let rejection = reject(coordinator.handle_request(&host_a(), create));
    assert_eq!(rejection.reason, LayoutRejectReason::Stale as i32);
    let state = coordinator.session_snapshot().unwrap().state.unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.tabs.len(), 1);
}

#[test]
fn foreign_pane_deletion_is_rejected() {
    let mut coordinator = coordinator();
    coordinator.admit(host_b(), addr_b()).unwrap();
    let mut delete = request(13, 2);
    delete.delete_pane = Some(DeletePane { pane_id: 1 });

    let rejection = reject(coordinator.handle_request(&host_b(), delete));
    assert_eq!(rejection.reason, LayoutRejectReason::NotHost as i32);
    assert_eq!(
        coordinator
            .session_snapshot()
            .unwrap()
            .state
            .unwrap()
            .revision,
        2
    );
}

#[test]
fn mixed_host_tab_deletion_is_rejected() {
    let mut coordinator = coordinator();
    coordinator.admit(host_b(), addr_b()).unwrap();
    let mut create = request(14, 2);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::LeftRight as i32),
        grid_rows: 24,
        grid_cols: 80,
        position: None,
    });
    let reservation = match coordinator.handle_request(&host_b(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    commit(coordinator.handle_pane_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 14,
        },
    ));
    let mut create_tab = request(15, 3);
    create_tab.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(&host_a(), create_tab) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    commit(coordinator.handle_pane_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 3,
            request_id: 15,
        },
    ));
    let mut delete = request(16, 4);
    delete.delete_tab = Some(DeleteTab { tab_id: 1 });

    let rejection = reject(coordinator.handle_request(&host_a(), delete));
    assert_eq!(rejection.reason, LayoutRejectReason::MixedTab as i32);
}

#[test]
fn limits_are_rejected_without_creating_visible_layout() {
    let mut coordinator = coordinator();
    let mut revision = 1;
    for request_id in 16..24 {
        let mut create = request(request_id, revision);
        create.create_tab = Some(CreateTab {
            grid_rows: 24,
            grid_cols: 80,
        });
        let reservation = match coordinator.handle_request(&host_a(), create) {
            CoordinatorResponse::Reservation(reservation) => reservation,
            other => panic!("expected reservation, got {other:?}"),
        };
        commit(coordinator.handle_pane_ready(
            &host_a(),
            PaneReady {
                reservation_id: reservation.reservation_id,
                base_revision: revision,
                request_id,
            },
        ));
        revision += 1;
    }
    let mut one_too_many = request(24, revision);
    one_too_many.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });

    let rejection = reject(coordinator.handle_request(&host_a(), one_too_many));
    assert_eq!(rejection.reason, LayoutRejectReason::Limit as i32);
    assert_eq!(
        coordinator
            .session_snapshot()
            .unwrap()
            .state
            .unwrap()
            .tabs
            .len(),
        9
    );
}

#[test]
fn wrong_creator_and_ready_revision_are_rejected_with_the_original_request_id() {
    let mut coordinator = coordinator();
    coordinator.admit(host_b(), addr_b()).unwrap();
    let mut create = request(18, 2);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let wrong_creator = reject(coordinator.handle_pane_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 18,
        },
    ));
    assert_eq!(wrong_creator.request_id, 18);
    assert_eq!(
        wrong_creator.reason,
        LayoutRejectReason::ReservationFailure as i32
    );

    let stale_ready = reject(coordinator.handle_pane_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 18,
        },
    ));
    assert_eq!(stale_ready.request_id, 18);
    assert_eq!(stale_ready.reason, LayoutRejectReason::Stale as i32);
    assert_eq!(
        coordinator
            .session_snapshot()
            .unwrap()
            .state
            .unwrap()
            .tabs
            .len(),
        1
    );
}

#[test]
fn reservation_expiry_is_deterministic_and_unwedges_new_requests() {
    let now = Instant::now();
    let mut coordinator = LayoutCoordinator::with_reservation_timeout(
        host_a(),
        addr_a(),
        24,
        80,
        Duration::from_secs(5),
        now,
    )
    .unwrap();
    let mut create = request(201, 1);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    match coordinator.handle_request_at(&host_a(), create, now) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    assert!(
        coordinator
            .expire_reservation_at(now + Duration::from_secs(4))
            .unwrap()
            .is_none()
    );
    let expired = coordinator
        .expire_reservation_at(now + Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(expired.peer_id, host_a());
    assert_eq!(expired.reject.request_id, 201);
    assert_eq!(
        expired.reject.reason,
        LayoutRejectReason::ReservationFailure as i32
    );
    let mut next = request(202, 1);
    next.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    assert!(matches!(
        coordinator.handle_request_at(&host_a(), next, now),
        CoordinatorResponse::Reservation(_)
    ));
}

#[test]
fn pane_failure_clears_its_creator_reservation_immediately() {
    let mut coordinator = coordinator();
    let mut create = request(203, 1);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    let failed = coordinator.handle_pane_failed(
        &host_a(),
        PaneFailed {
            reservation_id: reservation.reservation_id,
            request_id: 203,
            base_revision: 1,
        },
    );
    assert_eq!(failed.peer_id, host_a());
    assert_eq!(failed.reject.request_id, 203);
    let mut next = request(204, 1);
    next.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    assert!(matches!(
        coordinator.handle_request(&host_a(), next),
        CoordinatorResponse::Reservation(_)
    ));
}

#[test]
fn endpoints_and_ready_request_ids_must_match_authenticated_reservations() {
    assert!(matches!(
        LayoutCoordinator::new(host_a(), addr_b(), 24, 80),
        Err(CoordinatorError::EndpointIdentityMismatch)
    ));
    assert!(matches!(
        LayoutCoordinator::new(
            host_a(),
            EndpointAddr::new(SecretKey::from_bytes(&[1; 32]).public()),
            24,
            80
        ),
        Err(CoordinatorError::InvalidEndpointAddress)
    ));
    let mut coordinator = coordinator();
    assert!(matches!(
        coordinator.admit(host_b(), addr_a()),
        Err(CoordinatorError::EndpointIdentityMismatch)
    ));
    assert!(matches!(
        coordinator.admit(
            host_b(),
            EndpointAddr::new(SecretKey::from_bytes(&[2; 32]).public())
        ),
        Err(CoordinatorError::InvalidEndpointAddress)
    ));
    let mut create = request(205, 1);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    let rejected = reject(coordinator.handle_pane_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 206,
        },
    ));
    assert_eq!(rejected.request_id, 206);
    assert_eq!(
        rejected.reason,
        LayoutRejectReason::ReservationFailure as i32
    );
}
