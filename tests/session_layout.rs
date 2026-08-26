use iroh::{EndpointAddr, SecretKey};
use p2pmux::{
    ledger::{GENESIS_PREV_HASH, IntentSigner, LedgerVerifier, LedgerWriter},
    protocol::{
        AgentRoster, AgentRosterEntry, AgentRosterState, CreatePane, CreateTab, DeletePane,
        DeleteTab, LayoutCommit, LayoutRejectReason, LayoutRequest, LedgerEntry, LedgerEntryKind,
        MarkPaneExited, MembershipEvent, MembershipRecord, NewPanePosition, PaneFailed, PaneGrid,
        PaneReady, Presence, RenamePane, SetPaneLock, SetSplitRatio, SplitAxis, UpdatePaneGrids,
    },
    session::{CoordinatorError, CoordinatorResponse, LayoutCoordinator, RosterStatus},
};
use prost::Message;
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

/// Ask the coordinator for something the way a real peer does: signed.
///
/// Every test here used to hand the coordinator a bare request, which the coordinator now
/// refuses -- it records who asked for a change, and an unsigned request names nobody. The
/// signing is mechanical, so it lives here rather than in twenty call sites.
trait AsPeer {
    fn ask(&mut self, peer_id: &[u8], request: LayoutRequest) -> CoordinatorResponse;
    fn ask_at(
        &mut self,
        peer_id: &[u8],
        request: LayoutRequest,
        now: Instant,
    ) -> CoordinatorResponse;
    fn report_ready(&mut self, peer_id: &[u8], ready: PaneReady) -> CoordinatorResponse;
}

impl AsPeer for LayoutCoordinator {
    fn ask(&mut self, peer_id: &[u8], request: LayoutRequest) -> CoordinatorResponse {
        self.handle_request(peer_id, sign_request(peer_id, request))
    }

    fn ask_at(
        &mut self,
        peer_id: &[u8],
        request: LayoutRequest,
        now: Instant,
    ) -> CoordinatorResponse {
        self.handle_request_at(peer_id, sign_request(peer_id, request), now)
    }

    fn report_ready(&mut self, peer_id: &[u8], ready: PaneReady) -> CoordinatorResponse {
        self.handle_pane_ready(peer_id, sign_ready(peer_id, ready))
    }
}

fn sign_request(peer_id: &[u8], mut request: LayoutRequest) -> LayoutRequest {
    request.author_signature = Vec::new();
    request.author_signature =
        signer(peer_id).sign(LedgerEntryKind::LayoutChange, &request.encode_to_vec());
    request
}

fn sign_ready(peer_id: &[u8], mut ready: PaneReady) -> PaneReady {
    ready.author_signature = Vec::new();
    ready.author_signature =
        signer(peer_id).sign(LedgerEntryKind::PaneReady, &ready.encode_to_vec());
    ready
}

/// Every test identity comes from `endpoint(seed, _)`, so the seed can be recovered from
/// the public key rather than threaded through each call.
fn signer(peer_id: &[u8]) -> IntentSigner {
    let seed = (0..=u8::MAX)
        .find(|seed| SecretKey::from_bytes(&[*seed; 32]).public().as_bytes() == peer_id)
        .expect("test peers are all derived from a single-byte seed");
    IntentSigner::new(b"test-session".to_vec(), SecretKey::from_bytes(&[seed; 32]))
}

fn coordinator() -> LayoutCoordinator {
    LayoutCoordinator::new(host_a(), addr_a(), ledger(), 24, 80).expect("valid coordinator")
}

/// A ledger signed with the same key `host_a` is derived from, so entries a test reads back
/// verify against the coordinator it thinks wrote them.
fn ledger() -> LedgerWriter {
    LedgerWriter::new(b"test-session".to_vec(), SecretKey::from_bytes(&[1; 32]))
}

fn request(request_id: u64, base_revision: u64) -> LayoutRequest {
    LayoutRequest {
        request_id,
        base_revision,
        create_pane: None,
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

#[test]
fn admitted_members_set_ratios_and_hosts_reconcile_their_grids() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");
    // Create through the established reservation flow so pane 2 is hosted by B.
    let reservation = coordinator.ask(
        &host_b(),
        LayoutRequest {
            create_pane: Some(p2pmux::protocol::CreatePane {
                target_pane_id: 1,
                axis: Some(SplitAxis::LeftRight as i32),
                grid_rows: 24,
                grid_cols: 80,
                position: None,
                target_peer_id: Default::default(),
                command: Default::default(),
            }),
            ..request(1, 2)
        },
    );
    let reservation = match reservation {
        CoordinatorResponse::Reservation(value) => value,
        other => panic!("reservation: {other:?}"),
    };
    let _ = commit(coordinator.report_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 1,
            author_signature: Vec::new(),
        },
    ));
    let ratio = commit(coordinator.ask(
        &host_a(),
        LayoutRequest {
            set_split_ratio: Some(SetSplitRatio {
                pane_id: 2,
                axis: Some(SplitAxis::LeftRight as i32),
                first_share_bps: 7_500,
            }),
            ..request(2, 3)
        },
    ));
    assert_eq!(ratio.revision, 4);
    let grids = commit(coordinator.ask(
        &host_b(),
        LayoutRequest {
            update_pane_grids: Some(UpdatePaneGrids {
                panes: vec![PaneGrid {
                    pane_id: 2,
                    grid_rows: 30,
                    grid_cols: 100,
                }],
            }),
            ..request(3, 4)
        },
    ));
    assert_eq!(
        grids
            .state
            .expect("state")
            .panes
            .iter()
            .find(|pane| pane.pane_id == 2)
            .expect("pane")
            .grid_rows,
        30
    );
}

#[test]
fn pane_host_can_set_lock_and_guest_is_rejected() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");

    let commit = commit(coordinator.ask(
        &host_a(),
        LayoutRequest {
            set_pane_lock: Some(SetPaneLock {
                pane_id: 1,
                locked: true,
            }),
            ..request(1, 2)
        },
    ));
    assert!(commit.state.expect("state").panes[0].locked);

    let rejection = reject(coordinator.ask(
        &host_b(),
        LayoutRequest {
            set_pane_lock: Some(SetPaneLock {
                pane_id: 1,
                locked: false,
            }),
            ..request(2, 3)
        },
    ));
    assert_eq!(rejection.reason, LayoutRejectReason::NotHost as i32);
}

#[test]
fn pane_host_marks_exit_idempotently_and_guests_are_rejected() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");
    let revision = 2;
    let first = commit(coordinator.ask(
        &host_a(),
        LayoutRequest {
            mark_pane_exited: Some(MarkPaneExited { pane_id: 1 }),
            author_signature: Vec::new(),
            ..request(1, revision)
        },
    ));
    assert_eq!(first.revision, revision + 1);
    assert!(first.state.expect("state").panes[0].exited);

    let repeated = commit(coordinator.ask(
        &host_a(),
        LayoutRequest {
            mark_pane_exited: Some(MarkPaneExited { pane_id: 1 }),
            author_signature: Vec::new(),
            ..request(2, revision + 1)
        },
    ));
    assert_eq!(repeated.revision, revision + 1);
    assert_eq!(
        reject(coordinator.ask(
            &host_b(),
            LayoutRequest {
                mark_pane_exited: Some(MarkPaneExited { pane_id: 1 }),
                author_signature: Vec::new(),
                ..request(3, revision + 1)
            },
        ))
        .reason,
        LayoutRejectReason::NotHost as i32
    );
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

fn presence(peer_id: Vec<u8>, generation: u64, tab_id: u64, pane_id: u64) -> Presence {
    Presence {
        peer_id,
        generation,
        tab_id,
        pane_id,
        attached: true,
    }
}

#[test]
fn presence_is_keyed_by_the_authenticated_peer_and_ignores_stale_generations() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("guest admitted");

    // The claimed peer id never wins: a member cannot move somebody else's marker.
    let accepted = coordinator
        .accept_presence(&host_b(), presence(host_a(), 1, 1, 1))
        .expect("presence accepted");
    assert_eq!(accepted.peer_id, host_b());
    assert_eq!(coordinator.presence(), vec![accepted.clone()]);
    assert_eq!(coordinator.presence_epoch(), 1);

    // A replayed or reordered update is dropped, and costs no epoch.
    assert_eq!(
        coordinator.accept_presence(&host_b(), presence(host_b(), 1, 2, 2)),
        None
    );
    assert_eq!(coordinator.presence(), vec![accepted]);
    assert_eq!(coordinator.presence_epoch(), 1);

    let moved = coordinator
        .accept_presence(&host_b(), presence(host_b(), 2, 2, 2))
        .expect("newer generation accepted");
    assert_eq!((moved.tab_id, moved.pane_id), (2, 2));
    assert_eq!(coordinator.presence_epoch(), 2);
}

#[test]
fn presence_from_a_stranger_is_refused() {
    let mut coordinator = coordinator();

    assert_eq!(
        coordinator.accept_presence(&host_b(), presence(host_b(), 1, 1, 1)),
        None
    );
    assert!(coordinator.presence().is_empty());
    assert_eq!(coordinator.presence_epoch(), 0);
}

#[test]
fn detaching_clears_a_location_and_departure_drops_the_member() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("guest admitted");
    coordinator
        .accept_presence(&host_b(), presence(host_b(), 1, 1, 1))
        .expect("presence accepted");

    // A detached member is looking at nothing, whatever location they sent.
    let detached = coordinator
        .accept_presence(
            &host_b(),
            Presence {
                attached: false,
                ..presence(host_b(), 2, 1, 1)
            },
        )
        .expect("detach accepted");
    assert_eq!((detached.tab_id, detached.pane_id), (0, 0));

    let epoch = coordinator.presence_epoch();
    coordinator.remove_member(&host_b()).expect("guest removed");
    assert!(coordinator.presence().is_empty());
    assert!(
        coordinator.presence_epoch() > epoch,
        "a departure has to reach the coordinator's own renderer"
    );
}

#[test]
fn presence_survives_the_pane_it_points_at_being_deleted() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("guest admitted");

    // Focusing a pane in the same breath somebody deletes it must not strand the
    // member: the location simply draws nothing until their next update.
    let accepted = coordinator
        .accept_presence(&host_b(), presence(host_b(), 1, 99, 99))
        .expect("unknown location accepted");
    assert_eq!((accepted.tab_id, accepted.pane_id), (99, 99));
}

#[test]
fn agent_rosters_replace_per_host_and_reject_other_hosts_panes() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("guest admitted");

    let forged = AgentRoster {
        host_peer_id: host_a(),
        generation: 1,
        entries: vec![AgentRosterEntry {
            pane_id: 1,
            process_pid: 0,
            agent_kind: String::from("codex"),
            cwd: String::from("/forged"),
            state: AgentRosterState::Working as i32,
            working_since_unix_ms: 0,
            session_name: String::new(),
            in_another_session: false,
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
                    process_pid: 0,
                    agent_kind: String::from("codex"),
                    cwd: String::from("/repo"),
                    state: AgentRosterState::Working as i32,
                    working_since_unix_ms: 0,
                    session_name: String::new(),
                    in_another_session: false,
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });

    let reservation = match coordinator.ask(&host_a(), create) {
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

    let commit = commit(coordinator.report_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 10,
            author_signature: Vec::new(),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let state = commit(coordinator.report_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 15,
            author_signature: Vec::new(),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_a(), create) {
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

    let rejection = reject(coordinator.report_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 101,
            author_signature: Vec::new(),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    assert!(matches!(
        coordinator.ask(&host_a(), next),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_b(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let commit = commit(coordinator.report_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 11,
            author_signature: Vec::new(),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });

    let rejection = reject(coordinator.ask(&host_a(), create));
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

    let rejection = reject(coordinator.ask(&host_b(), delete));
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_b(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    commit(coordinator.report_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 14,
            author_signature: Vec::new(),
        },
    ));
    let mut create_tab = request(15, 3);
    create_tab.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_a(), create_tab) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    commit(coordinator.report_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 3,
            request_id: 15,
            author_signature: Vec::new(),
        },
    ));
    let mut delete = request(16, 4);
    delete.delete_tab = Some(DeleteTab { tab_id: 1 });

    let rejection = reject(coordinator.ask(&host_a(), delete));
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
            target_peer_id: Default::default(),
            command: Default::default(),
        });
        let reservation = match coordinator.ask(&host_a(), create) {
            CoordinatorResponse::Reservation(reservation) => reservation,
            other => panic!("expected reservation, got {other:?}"),
        };
        commit(coordinator.report_ready(
            &host_a(),
            PaneReady {
                reservation_id: reservation.reservation_id,
                base_revision: revision,
                request_id,
                author_signature: Vec::new(),
            },
        ));
        revision += 1;
    }
    let mut one_too_many = request(24, revision);
    one_too_many.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
        target_peer_id: Default::default(),
        command: Default::default(),
    });

    let rejection = reject(coordinator.ask(&host_a(), one_too_many));
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let wrong_creator = reject(coordinator.report_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 18,
            author_signature: Vec::new(),
        },
    ));
    assert_eq!(wrong_creator.request_id, 18);
    assert_eq!(
        wrong_creator.reason,
        LayoutRejectReason::ReservationFailure as i32
    );

    let stale_ready = reject(coordinator.report_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 18,
            author_signature: Vec::new(),
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
        ledger(),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    match coordinator.ask_at(&host_a(), create, now) {
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    assert!(matches!(
        coordinator.ask_at(&host_a(), next, now),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    let failed = coordinator.handle_pane_failed(
        &host_a(),
        PaneFailed {
            reservation_id: reservation.reservation_id,
            request_id: 203,
            base_revision: 1,
            refused: false,
        },
    );
    assert_eq!(failed.peer_id, host_a());
    assert_eq!(failed.reject.request_id, 203);
    let mut next = request(204, 1);
    next.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    assert!(matches!(
        coordinator.ask(&host_a(), next),
        CoordinatorResponse::Reservation(_)
    ));
}

/// The timeout that makes "ask me first" safe.
///
/// A machine configured to be asked holds the request rather than answering it,
/// and nobody may be sitting at that machine. Nothing grants the pane in the
/// meantime: the reservation runs out and the person who asked is told, which
/// is the same outcome as a refusal and the reason the panel can say
/// "unanswered is refused" and mean it.
#[test]
fn a_remote_terminal_nobody_answers_expires_rather_than_opening() {
    let now = Instant::now();
    let mut coordinator = LayoutCoordinator::with_reservation_timeout(
        host_a(),
        addr_a(),
        ledger(),
        24,
        80,
        Duration::from_secs(5),
        now,
    )
    .unwrap();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member B joins");
    let mut create = request(401, 2);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
        target_peer_id: host_b(),
        command: vec![String::from("shell")],
    });
    let reservation = match coordinator.ask_at(&host_a(), create, now) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    assert_eq!(reservation.host_peer_id, host_b());

    // B is holding the question in front of a keyboard nobody is at.
    let expired = coordinator
        .expire_reservation_at(now + Duration::from_secs(5))
        .unwrap()
        .expect("an unanswered reservation expires");

    assert_eq!(expired.peer_id, host_a(), "the machine that asked is told");
    assert_eq!(expired.reject.request_id, 401);
    // And nothing was created while nobody was answering: the next request
    // gets a reservation, which only an empty slot allows.
    let mut next = request(402, 2);
    next.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    assert!(matches!(
        coordinator.ask_at(&host_a(), next, now + Duration::from_secs(6)),
        CoordinatorResponse::Reservation(_)
    ));
}

/// A machine that says no and a pty that would not start are different answers,
/// and the person waiting does different things about them: ask the machine's
/// owner, or try again.
#[test]
fn a_refusal_reaches_the_asker_as_a_refusal_rather_than_a_failure() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member B joins");
    // Revision 2: the coordinator started at 1 and admitting B advanced it.
    let mut create = request(301, 2);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
        target_peer_id: host_b(),
        command: vec![String::from("hermes"), String::from("chat")],
    });
    let reservation = match coordinator.ask(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    assert_eq!(
        reservation.host_peer_id,
        host_b(),
        "the reservation is addressed to the machine being asked"
    );

    let refused = coordinator.handle_pane_failed(
        &host_b(),
        PaneFailed {
            reservation_id: reservation.reservation_id,
            request_id: 301,
            base_revision: reservation.base_revision,
            refused: true,
        },
    );

    assert_eq!(
        refused.peer_id,
        host_a(),
        "the answer goes to the machine that asked, not the one that refused"
    );
    assert_eq!(
        refused.reject.reason,
        LayoutRejectReason::TargetRefused as i32
    );
}

#[test]
fn endpoints_and_ready_request_ids_must_match_authenticated_reservations() {
    assert!(matches!(
        LayoutCoordinator::new(host_a(), addr_b(), ledger(), 24, 80),
        Err(CoordinatorError::EndpointIdentityMismatch)
    ));
    assert!(matches!(
        LayoutCoordinator::new(
            host_a(),
            EndpointAddr::new(SecretKey::from_bytes(&[1; 32]).public()),
            ledger(),
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
        target_peer_id: Default::default(),
        command: Default::default(),
    });
    let reservation = match coordinator.ask(&host_a(), create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    let rejected = reject(coordinator.report_ready(
        &host_a(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
            request_id: 206,
            author_signature: Vec::new(),
        },
    ));
    assert_eq!(rejected.request_id, 206);
    assert_eq!(
        rejected.reason,
        LayoutRejectReason::ReservationFailure as i32
    );
}

#[test]
fn a_new_session_is_open_and_the_lock_toggles() {
    let mut coordinator = coordinator();

    assert!(!coordinator.is_locked(), "sessions start open");
    assert!(
        coordinator.set_locked(true),
        "locking an open session changes it"
    );
    assert!(coordinator.is_locked());
    assert!(
        !coordinator.set_locked(true),
        "locking an already locked session is a no-op"
    );
    assert!(coordinator.set_locked(false));
    assert!(!coordinator.is_locked());
}

#[test]
fn locking_governs_the_door_without_evicting_anyone() {
    // The lock must not throw out the people already working, and a peer that has been
    // admitted once has to survive a reconnect through a locked door -- otherwise a
    // transient drop would permanently exile a teammate mid-session.
    let mut coordinator = coordinator();
    coordinator.remember_admitted(host_b());

    coordinator.set_locked(true);

    assert!(
        coordinator.is_admitted(&host_b()),
        "an already-admitted peer must still be recognised while locked"
    );
    assert!(
        !coordinator.is_admitted(endpoint(3, 4103).id.as_bytes().as_ref()),
        "a peer that never joined is not admitted by locking"
    );
}

#[test]
fn admission_is_remembered_independently_of_the_member_roster() {
    // `is_admitted` deliberately does not read the member list: a member removed during a
    // disconnect must still be let back in, which is the whole reason for a second set.
    let mut coordinator = coordinator();
    let stranger = endpoint(4, 4104).id.as_bytes().to_vec();

    assert!(!coordinator.is_admitted(&stranger));
    coordinator.remember_admitted(stranger.clone());
    assert!(coordinator.is_admitted(&stranger));
}

#[test]
fn a_locked_refusal_is_distinct_from_a_full_one() {
    // A joiner has to be able to tell "come back later, it is full" from "the host shut
    // the door", so these must never collapse to one reason code.
    assert_ne!(
        LayoutRejectReason::Locked as i32,
        LayoutRejectReason::Limit as i32
    );
    assert_eq!(
        LayoutRejectReason::try_from(LayoutRejectReason::Locked as i32),
        Ok(LayoutRejectReason::Locked)
    );
}

#[test]
fn a_revoked_key_stays_revoked_when_it_knocks_again() {
    let mut coordinator = coordinator();
    coordinator.remember_admitted(host_b());
    assert!(coordinator.is_admitted(&host_b()));

    assert!(
        coordinator.revoke(host_b()),
        "first revoke changes the roster"
    );
    assert!(coordinator.is_revoked(&host_b()));
    assert!(!coordinator.is_admitted(&host_b()));

    // Knocking again is exactly what a revoked peer does next. Honouring it would make
    // revoking a suggestion rather than a decision.
    coordinator.remember_admitted(host_b());
    assert!(coordinator.is_revoked(&host_b()));
    assert!(!coordinator.is_admitted(&host_b()));
    assert!(
        !coordinator.revoke(host_b()),
        "revoking an already-revoked key changes nothing"
    );
}

#[test]
fn the_roster_names_every_key_the_session_has_an_opinion_about() {
    let mut coordinator = coordinator();
    let stranger = endpoint(5, 4105).id.as_bytes().to_vec();
    coordinator.remember_admitted(host_b());
    coordinator.revoke(stranger.clone());

    let roster: Vec<(Vec<u8>, RosterStatus)> = coordinator
        .roster()
        .entries()
        .map(|(peer_id, status)| (peer_id.to_vec(), status))
        .collect();

    assert_eq!(roster.len(), 2);
    assert_eq!(
        roster
            .iter()
            .find(|(peer_id, _)| peer_id == &host_b())
            .map(|(_, status)| *status),
        Some(RosterStatus::Admitted)
    );
    assert_eq!(
        roster
            .iter()
            .find(|(peer_id, _)| peer_id == &stranger)
            .map(|(_, status)| *status),
        Some(RosterStatus::Revoked)
    );
}

#[test]
fn a_revoked_refusal_is_distinct_from_a_locked_one() {
    // A lock lifts for everybody at once; a revocation names one key and survives the
    // unlock. A joiner that cannot tell them apart will keep retrying forever.
    assert_ne!(
        LayoutRejectReason::Revoked as i32,
        LayoutRejectReason::Locked as i32
    );
    assert_eq!(
        LayoutRejectReason::try_from(LayoutRejectReason::Revoked as i32),
        Ok(LayoutRejectReason::Revoked)
    );
}

fn ledger_verifier() -> LedgerVerifier {
    LedgerVerifier::new(
        b"test-session".to_vec(),
        SecretKey::from_bytes(&[1; 32]).public(),
    )
}

fn entry(commit: &LayoutCommit) -> LedgerEntry {
    commit.entry.clone().expect("every commit seals an entry")
}

#[test]
fn the_chain_a_session_writes_verifies_against_its_coordinator_key() {
    let mut coordinator = coordinator();
    let mut verifier = ledger_verifier();

    let admission = coordinator.admit(host_b(), addr_b()).expect("admitted");
    let first = entry(&admission.commit);
    assert_eq!(first.seq, 1);
    assert_eq!(first.prev_hash, GENESIS_PREV_HASH.to_vec());
    assert_eq!(verifier.accept(&first), Ok(()));

    for (step, request_id) in (2..=4u64).enumerate() {
        let renamed = commit(coordinator.ask(
            &host_a(),
            LayoutRequest {
                rename_pane: Some(RenamePane {
                    pane_id: 1,
                    title: format!("pass {request_id}"),
                }),
                ..request(request_id, renamed_base_revision(step))
            },
        ));
        let sealed = entry(&renamed);
        assert_eq!(sealed.seq, request_id);
        assert_eq!(verifier.accept(&sealed), Ok(()));
    }
}

/// Each rename advances the revision by one, starting from the admission at revision 2.
fn renamed_base_revision(step: usize) -> u64 {
    2 + step as u64
}

#[test]
fn the_ledger_records_the_request_that_was_made() {
    let mut coordinator = coordinator();
    let renamed = commit(coordinator.ask(
        &host_a(),
        LayoutRequest {
            rename_pane: Some(RenamePane {
                pane_id: 1,
                title: String::from("build"),
            }),
            ..request(7, 1)
        },
    ));

    let sealed = entry(&renamed);
    assert_eq!(sealed.kind, LedgerEntryKind::LayoutChange as i32);
    assert_eq!(sealed.author_peer_id, host_a());
    // The entry holds the request itself, not a summary of it: a reader a week later has to
    // be able to see what was asked for, not what somebody later decided it meant.
    let recorded =
        LayoutRequest::decode(sealed.payload.as_slice()).expect("the payload is the request");
    assert_eq!(recorded.request_id, 7);
    assert_eq!(recorded.rename_pane.expect("rename").title, "build");
}

#[test]
fn a_change_is_recorded_under_the_key_that_asked_for_it() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");

    let reservation = match coordinator.ask(
        &host_b(),
        LayoutRequest {
            create_tab: Some(CreateTab {
                grid_rows: 24,
                grid_cols: 80,
                target_peer_id: Default::default(),
                command: Default::default(),
            }),
            ..request(1, 2)
        },
    ) {
        CoordinatorResponse::Reservation(value) => value,
        other => panic!("expected reservation, got {other:?}"),
    };
    let ready = commit(coordinator.report_ready(
        &host_b(),
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
            request_id: 1,
            author_signature: Vec::new(),
        },
    ));

    let sealed = entry(&ready);
    assert_eq!(sealed.author_peer_id, host_b());
    assert_eq!(sealed.kind, LedgerEntryKind::PaneReady as i32);
    // A reservation is not a change, so it must not have taken a place in the chain: the
    // admission is 1 and this is 2.
    assert_eq!(sealed.seq, 2);
}

#[test]
fn a_departure_is_authored_by_the_coordinator_because_nobody_asked_for_it() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");

    let departure = coordinator
        .remove_member(&host_b())
        .expect("member removed");
    let sealed = entry(&departure.commit);

    assert_eq!(
        sealed.author_peer_id,
        host_a(),
        "a dropped connection is the coordinator's own observation, not a member's request"
    );
    assert!(
        sealed.author_signature.is_empty(),
        "there is no member intent to carry"
    );
    let record = MembershipRecord::decode(sealed.payload.as_slice())
        .expect("payload is a membership record");
    assert_eq!(record.peer_id, host_b());
    assert_eq!(record.event, MembershipEvent::Left as i32);
}

fn rename_pane_request(request_id: u64, base_revision: u64, title: &str) -> LayoutRequest {
    LayoutRequest {
        rename_pane: Some(RenamePane {
            pane_id: 1,
            title: String::from(title),
        }),
        ..request(request_id, base_revision)
    }
}

#[test]
fn an_unsigned_request_is_refused_because_it_names_nobody() {
    let mut coordinator = coordinator();

    // Deliberately not through `ask`: this is the raw request a peer would send if it
    // skipped signing. The connection is authentic, but the ledger records authorship for
    // readers who were never on it, and there is nothing here to record.
    let rejection =
        reject(coordinator.handle_request(&host_a(), rename_pane_request(1, 1, "build")));

    assert_eq!(rejection.reason, LayoutRejectReason::Unsigned as i32);
    assert_eq!(rejection.request_id, 1);
}

#[test]
fn a_request_signed_by_somebody_else_is_refused() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");

    // B's signature, presented over A's authenticated connection. Either half alone looks
    // fine; the point is that they have to be the same peer.
    let borrowed = sign_request(&host_b(), rename_pane_request(1, 2, "build"));
    let rejection = reject(coordinator.handle_request(&host_a(), borrowed));

    assert_eq!(rejection.reason, LayoutRejectReason::Unsigned as i32);
}

#[test]
fn a_request_edited_after_signing_is_refused() {
    let mut coordinator = coordinator();
    let mut tampered = sign_request(&host_a(), rename_pane_request(1, 1, "build"));
    tampered.rename_pane = Some(RenamePane {
        pane_id: 1,
        title: String::from("something else"),
    });

    let rejection = reject(coordinator.handle_request(&host_a(), tampered));

    assert_eq!(rejection.reason, LayoutRejectReason::Unsigned as i32);
}

#[test]
fn the_sealed_entry_carries_the_authors_own_signature() {
    let mut coordinator = coordinator();
    let mut verifier = ledger_verifier();
    let renamed = commit(coordinator.ask(&host_a(), rename_pane_request(1, 1, "build")));

    let sealed = entry(&renamed);
    assert!(
        !sealed.author_signature.is_empty(),
        "the record of who asked has to rest on the asker's key, not the coordinator's word"
    );
    // The verifier checks that signature against the payload, so this passing is the whole
    // claim: even the coordinator could not have pinned this change on somebody else.
    assert_eq!(verifier.accept(&sealed), Ok(()));
}

#[test]
fn a_reattributed_entry_fails_the_members_check() {
    let mut coordinator = coordinator();
    coordinator
        .admit(host_b(), addr_b())
        .expect("member admitted");
    let mut verifier = ledger_verifier();
    verifier
        .accept(&entry(&commit(
            coordinator.ask(&host_a(), rename_pane_request(1, 2, "build")),
        )))
        .expect("the coordinator's own record verifies");

    // A coordinator that wanted to blame B for A's change would have to produce B's
    // signature over it, and it cannot.
    let mut forged = entry(&commit(
        coordinator.ask(&host_a(), rename_pane_request(2, 3, "deploy")),
    ));
    forged.author_peer_id = host_b();

    assert!(verifier.accept(&forged).is_err());
}
