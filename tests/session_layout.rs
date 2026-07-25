use p2pmux::{
    protocol::{
        CreatePane, CreateTab, DeletePane, DeleteTab, LayoutCommit, LayoutRejectReason,
        LayoutRequest, PaneReady, SplitAxis,
    },
    session::{CoordinatorResponse, LayoutCoordinator},
};

const HOST_A: &[u8] = b"host-a";
const HOST_B: &[u8] = b"host-b";
const ADDR_A: &[u8] = b"endpoint-a";
const ADDR_B: &[u8] = b"endpoint-b";

fn coordinator() -> LayoutCoordinator {
    LayoutCoordinator::new(HOST_A.to_vec(), ADDR_A.to_vec(), 24, 80).expect("valid coordinator")
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
    assert_eq!(state.members[0].peer_id, HOST_A);
    assert_eq!(state.members[0].endpoint_addr, ADDR_A);
    assert_eq!(state.panes.len(), 1);
    assert_eq!(state.panes[0].pane_id, 1);
    assert_eq!(state.panes[0].host_peer_id, HOST_A);
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
        .admit(HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("guest is admitted");

    assert_eq!(commit.revision, 2);
    let state = commit.state.expect("state present");
    assert!(
        state
            .members
            .iter()
            .any(|member| member.peer_id == HOST_B && member.endpoint_addr == ADDR_B)
    );
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
    });

    let reservation = match coordinator.handle_request(HOST_A, create) {
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
        HOST_A,
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
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
fn admitted_guest_hosts_its_own_pane_after_ready() {
    let mut coordinator = coordinator();
    coordinator.admit(HOST_B.to_vec(), ADDR_B.to_vec()).unwrap();
    let mut create = request(11, 2);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::TopBottom as i32),
        grid_rows: 31,
        grid_cols: 101,
    });
    let reservation = match coordinator.handle_request(HOST_B, create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let commit = commit(coordinator.handle_pane_ready(
        HOST_B,
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
        },
    ));
    let state = commit.state.unwrap();
    let pane = state
        .panes
        .iter()
        .find(|pane| pane.pane_id == reservation.pane_id)
        .expect("reserved pane committed");
    assert_eq!(pane.host_peer_id, HOST_B);
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

    let rejection = reject(coordinator.handle_request(HOST_A, create));
    assert_eq!(rejection.reason, LayoutRejectReason::Stale as i32);
    let state = coordinator.session_snapshot().unwrap().state.unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.tabs.len(), 1);
}

#[test]
fn foreign_pane_deletion_is_rejected() {
    let mut coordinator = coordinator();
    coordinator.admit(HOST_B.to_vec(), ADDR_B.to_vec()).unwrap();
    let mut delete = request(13, 2);
    delete.delete_pane = Some(DeletePane { pane_id: 1 });

    let rejection = reject(coordinator.handle_request(HOST_B, delete));
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
    coordinator.admit(HOST_B.to_vec(), ADDR_B.to_vec()).unwrap();
    let mut create = request(14, 2);
    create.create_pane = Some(CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::LeftRight as i32),
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(HOST_B, create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    commit(coordinator.handle_pane_ready(
        HOST_B,
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
        },
    ));
    let mut create_tab = request(15, 3);
    create_tab.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(HOST_A, create_tab) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };
    commit(coordinator.handle_pane_ready(
        HOST_A,
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 3,
        },
    ));
    let mut delete = request(16, 4);
    delete.delete_tab = Some(DeleteTab { tab_id: 1 });

    let rejection = reject(coordinator.handle_request(HOST_A, delete));
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
        let reservation = match coordinator.handle_request(HOST_A, create) {
            CoordinatorResponse::Reservation(reservation) => reservation,
            other => panic!("expected reservation, got {other:?}"),
        };
        commit(coordinator.handle_pane_ready(
            HOST_A,
            PaneReady {
                reservation_id: reservation.reservation_id,
                base_revision: revision,
            },
        ));
        revision += 1;
    }
    let mut one_too_many = request(24, revision);
    one_too_many.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });

    let rejection = reject(coordinator.handle_request(HOST_A, one_too_many));
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
    coordinator.admit(HOST_B.to_vec(), ADDR_B.to_vec()).unwrap();
    let mut create = request(18, 2);
    create.create_tab = Some(CreateTab {
        grid_rows: 24,
        grid_cols: 80,
    });
    let reservation = match coordinator.handle_request(HOST_A, create) {
        CoordinatorResponse::Reservation(reservation) => reservation,
        other => panic!("expected reservation, got {other:?}"),
    };

    let wrong_creator = reject(coordinator.handle_pane_ready(
        HOST_B,
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 2,
        },
    ));
    assert_eq!(wrong_creator.request_id, 18);
    assert_eq!(
        wrong_creator.reason,
        LayoutRejectReason::ReservationFailure as i32
    );

    let stale_ready = reject(coordinator.handle_pane_ready(
        HOST_A,
        PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 1,
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
