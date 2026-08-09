use p2pmux::layout::{
    Axis, DEFAULT_FIRST_SHARE_BPS, LayoutError, LayoutSnapshot, MAX_ENDPOINT_ADDR_BYTES,
    MAX_MEMBERS, MAX_PANES_PER_TAB, MAX_PEER_ID_BYTES, MAX_TABS, NewPanePosition, Node,
    ReservationCommit, SessionState,
};

const HOST_A: &[u8] = b"host-a";
const HOST_B: &[u8] = b"host-b";
const ADDR_A: &[u8] = b"addr-a";
const ADDR_B: &[u8] = b"addr-b";

fn state() -> SessionState {
    SessionState::new(HOST_A.to_vec(), ADDR_A.to_vec(), 24, 80).expect("valid initial layout")
}

fn snapshot(state: &SessionState) -> LayoutSnapshot {
    state.snapshot()
}

#[test]
fn removing_a_member_atomically_prunes_its_panes_and_empty_tabs() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    state
        .create_tab(HOST_B, state.revision(), 24, 80)
        .expect("B creates a tab");

    state.remove_member(HOST_B).expect("member B departs");
    let snapshot = snapshot(&state);
    assert_eq!(snapshot.revision, 4);
    assert_eq!(snapshot.members.len(), 1);
    assert_eq!(snapshot.members[0].peer_id, HOST_A);
    assert_eq!(snapshot.tabs.len(), 1);
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.panes[&1].host_peer_id, HOST_A);
    SessionState::validate_snapshot(&snapshot).expect("departure leaves a valid snapshot");
}

#[test]
fn a_restored_state_is_the_one_the_room_was_already_looking_at() {
    // What a promoted member inherits. Not a fresh session that happens to have the same
    // people in it -- the same tabs, panes and join order, at the same revision.
    let mut original = state();
    original
        .add_member(original.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    original
        .create_tab(HOST_B, original.revision(), 24, 80)
        .expect("B creates a tab");
    let before = snapshot(&original);

    let restored = SessionState::restore(before.clone()).expect("a committed layout restores");

    assert_eq!(snapshot(&restored), before);
    assert_eq!(restored.revision(), original.revision());
    assert_eq!(
        restored
            .members()
            .iter()
            .map(|member| member.peer_id.clone())
            .collect::<Vec<_>>(),
        vec![HOST_A.to_vec(), HOST_B.to_vec()],
        "join order is the succession order, so it has to survive verbatim"
    );
}

#[test]
fn a_restored_state_issues_ids_past_the_ones_already_in_use() {
    // Resuming the counters from zero would hand a new pane the id of one somebody is
    // currently typing into.
    let mut original = state();
    original
        .add_member(original.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    original
        .create_tab(HOST_B, original.revision(), 24, 80)
        .expect("B creates a tab");
    let highest_pane = *snapshot(&original).panes.keys().max().expect("panes exist");

    let mut restored = SessionState::restore(snapshot(&original)).expect("restores");
    let reservation = restored
        .reserve_tab(HOST_A, restored.revision(), 24, 80)
        .expect("the new coordinator can still create");

    assert!(
        reservation.pane_id > highest_pane,
        "a pane created after the takeover must not collide with one created before it"
    );
}

#[test]
fn a_restored_state_keeps_the_departed_coordinator_and_its_panes() {
    // The design calls for unavailable placeholders, not a layout that rearranges itself
    // under people. It also has to hold in the two-person case, where evicting the
    // coordinator's panes would leave the survivor looking at nothing at all.
    let mut original = state();
    original
        .add_member(original.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    let before = snapshot(&original);
    assert!(
        before
            .panes
            .values()
            .all(|pane| pane.host_peer_id == HOST_A),
        "only the coordinator hosts anything, which is the case that would break"
    );

    let restored = SessionState::restore(before).expect("restores");

    assert_eq!(restored.members().len(), 2);
    assert_eq!(restored.panes().count(), 1);
}

#[test]
fn a_layout_that_would_be_refused_from_a_peer_is_refused_from_a_successor_too() {
    // A takeover is not a way to smuggle in a layout the validator would reject: the
    // successor is a member like any other and does not get to invent state.
    let mut malformed = snapshot(&state());
    malformed.revision = 0;

    assert_eq!(
        SessionState::restore(malformed),
        Err(LayoutError::InvalidSnapshot)
    );
}

fn tab(snapshot: &LayoutSnapshot, tab_id: u64) -> &p2pmux::layout::Tab {
    snapshot
        .tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id)
        .expect("tab exists")
}

#[test]
fn initial_state_has_one_tab_and_one_hosted_pane() {
    let state = state();
    let snapshot = snapshot(&state);

    assert_eq!(state.revision(), 1);
    assert_eq!(snapshot.members.len(), 1);
    assert_eq!(snapshot.tabs.len(), 1);
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.tabs[0].root, Node::Leaf { pane_id: 1 });
    assert_eq!(snapshot.panes[&1].host_peer_id, HOST_A);
    assert!(!snapshot.panes[&1].locked);
    assert!(!snapshot.panes[&1].exited);
    assert_eq!(snapshot.members[0].endpoint_addr, ADDR_A);
}

#[test]
fn pane_host_can_lock_and_unlock_while_guests_cannot() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");

    state
        .set_pane_lock(HOST_A, state.revision(), 1, true)
        .expect("host locks pane");
    assert!(state.pane(1).expect("pane").locked);
    assert_eq!(
        state.set_pane_lock(HOST_B, state.revision(), 1, false),
        Err(LayoutError::NotPaneHost { pane_id: 1 })
    );

    state
        .set_pane_lock(HOST_A, state.revision(), 1, false)
        .expect("host unlocks pane");
    assert!(!state.pane(1).expect("pane").locked);
}

#[test]
fn pane_host_marks_exit_once_and_stale_or_guest_requests_fail() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    let revision = state.revision();

    state
        .mark_pane_exited(HOST_A, revision, 1)
        .expect("host marks exit");
    assert!(state.pane(1).expect("pane").exited);
    assert_eq!(state.revision(), revision + 1);

    state
        .mark_pane_exited(HOST_A, state.revision(), 1)
        .expect("matching revision repeat is a no-op");
    assert_eq!(state.revision(), revision + 1);
    assert_eq!(
        state.mark_pane_exited(HOST_B, state.revision(), 1),
        Err(LayoutError::NotPaneHost { pane_id: 1 })
    );
    assert_eq!(
        state.mark_pane_exited(HOST_A, revision, 1),
        Err(LayoutError::StaleRevision {
            expected: revision + 1,
            got: revision,
        })
    );
}

#[test]
fn pane_lock_survives_snapshot_round_trip() {
    let mut state = state();
    state
        .set_pane_lock(HOST_A, state.revision(), 1, true)
        .expect("host locks pane");

    let snapshot = state.snapshot();
    let round_trip: LayoutSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot).expect("serialize snapshot"))
            .expect("deserialize snapshot");
    assert_eq!(round_trip, snapshot);
    assert!(round_trip.panes[&1].locked);
}

#[test]
fn splits_default_to_a_valid_shared_ratio_and_ratio_updates_are_revisioned() {
    let mut state = state();
    let second = state
        .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("split");
    assert!(matches!(
        snapshot(&state).tabs[0].root,
        Node::Split {
            first_share_bps: DEFAULT_FIRST_SHARE_BPS,
            ..
        }
    ));

    state
        .set_split_ratio(HOST_A, state.revision(), second, Axis::LeftRight, 7_500)
        .expect("ratio update");
    assert!(matches!(
        snapshot(&state).tabs[0].root,
        Node::Split {
            first_share_bps: 7_500,
            ..
        }
    ));
}

#[test]
fn ratios_and_host_grid_batches_are_strict_and_atomic() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member");
    let second = state
        .create_pane(HOST_B, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("pane");
    let before = snapshot(&state);
    assert_eq!(
        state.set_split_ratio(HOST_B, state.revision(), second, Axis::TopBottom, 5_000),
        Err(LayoutError::NoMatchingSplit {
            pane_id: second,
            axis: Axis::TopBottom
        })
    );
    assert_eq!(
        state.set_split_ratio(HOST_B, state.revision(), 99, Axis::LeftRight, 5_000),
        Err(LayoutError::UnknownPane { pane_id: 99 })
    );
    assert_eq!(snapshot(&state), before);

    state
        .update_pane_grids(HOST_B, state.revision(), &[(second, 30, 100)])
        .expect("host grid update");
    assert_eq!(state.pane(second).expect("pane").grid_rows, 30);
    let after = snapshot(&state);
    assert_eq!(
        state.update_pane_grids(HOST_A, state.revision(), &[(second, 31, 100)]),
        Err(LayoutError::NotPaneHost { pane_id: second })
    );
    assert_eq!(
        state.update_pane_grids(HOST_B, state.revision(), &[(second, 0, 100)]),
        Err(LayoutError::InvalidGrid)
    );
    assert_eq!(snapshot(&state), after);
}

#[test]
fn initial_layout_rejects_zero_sized_grids_and_invalid_endpoint_addresses() {
    assert_eq!(
        SessionState::new(HOST_A.to_vec(), ADDR_A.to_vec(), 0, 80),
        Err(LayoutError::InvalidGrid)
    );
    assert_eq!(
        SessionState::new(HOST_A.to_vec(), ADDR_A.to_vec(), 24, 0),
        Err(LayoutError::InvalidGrid)
    );
    assert_eq!(
        SessionState::new(HOST_A.to_vec(), Vec::new(), 24, 80),
        Err(LayoutError::InvalidEndpointAddress)
    );
    assert_eq!(
        SessionState::new(
            HOST_A.to_vec(),
            vec![0; MAX_ENDPOINT_ADDR_BYTES + 1],
            24,
            80
        ),
        Err(LayoutError::InvalidEndpointAddress)
    );
    assert_eq!(
        SessionState::new(Vec::new(), ADDR_A.to_vec(), 24, 80),
        Err(LayoutError::InvalidPeerId)
    );
    assert_eq!(
        SessionState::new(vec![0; MAX_PEER_ID_BYTES + 1], ADDR_A.to_vec(), 24, 80),
        Err(LayoutError::InvalidPeerId)
    );
}

#[test]
fn member_setup_rejects_empty_peer_ids_without_mutating_the_snapshot() {
    let mut state = state();
    let before = snapshot(&state);

    assert_eq!(
        state.add_member(state.revision(), Vec::new(), ADDR_B.to_vec()),
        Err(LayoutError::InvalidPeerId)
    );
    assert_eq!(snapshot(&state), before);

    assert_eq!(
        state.add_member(
            state.revision(),
            vec![0; MAX_PEER_ID_BYTES + 1],
            ADDR_B.to_vec()
        ),
        Err(LayoutError::InvalidPeerId)
    );
    assert_eq!(snapshot(&state), before);
}

#[test]
fn creating_panes_uses_the_requested_split_axis() {
    let mut state = state();
    let right = state
        .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 30, 100)
        .expect("right split");
    let down = state
        .create_pane(HOST_A, state.revision(), right, Axis::TopBottom, 20, 70)
        .expect("down split");
    let root = &snapshot(&state).tabs[0].root;

    assert!(matches!(
        root,
        Node::Split {
            axis: Axis::LeftRight,
            second,
            ..
        } if matches!(
            second.as_ref(),
            Node::Split {
                axis: Axis::TopBottom,
                first,
                second,
                ..
            } if matches!(first.as_ref(), Node::Leaf { pane_id } if *pane_id == right)
                && matches!(second.as_ref(), Node::Leaf { pane_id } if *pane_id == down)
        )
    ));
}

#[test]
fn creating_panes_honors_first_or_second_placement_and_defaults_to_second() {
    let mut state = state();
    let second_pane = state
        .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("default second split");
    assert!(matches!(
        snapshot(&state).tabs[0].root,
        Node::Split { ref first, ref second, .. }
            if matches!(first.as_ref(), Node::Leaf { pane_id: 1 })
                && matches!(second.as_ref(), Node::Leaf { pane_id } if *pane_id == second_pane)
    ));

    let first = state
        .create_pane_at(
            HOST_A,
            state.revision(),
            second_pane,
            Axis::TopBottom,
            NewPanePosition::First,
            24,
            80,
        )
        .expect("first split");
    assert!(matches!(
        snapshot(&state).tabs[0].root,
        Node::Split { second: ref right_split, .. }
            if matches!(
                right_split.as_ref(),
                Node::Split { first: child_first, second: child_second, .. }
                    if matches!(child_first.as_ref(), Node::Leaf { pane_id } if *pane_id == first)
                        && matches!(child_second.as_ref(), Node::Leaf { pane_id } if *pane_id == second_pane)
            )
    ));
}

#[test]
fn deleting_a_pane_expands_its_sibling() {
    let mut state = state();
    let sibling = state
        .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("split");
    state
        .delete_pane(HOST_A, state.revision(), sibling)
        .expect("host can delete pane");

    let snapshot = snapshot(&state);
    assert_eq!(snapshot.tabs[0].root, Node::Leaf { pane_id: 1 });
    assert!(!snapshot.panes.contains_key(&sibling));
}

#[test]
fn membership_tab_pane_and_depth_limits_are_enforced() {
    let mut state = state();
    for member in 1..MAX_MEMBERS {
        state
            .add_member(
                state.revision(),
                format!("member-{member}").into_bytes(),
                format!("addr-{member}").into_bytes(),
            )
            .expect("within member limit");
    }
    let before = snapshot(&state);
    assert_eq!(
        state.add_member(
            state.revision(),
            b"one-too-many".to_vec(),
            b"address".to_vec()
        ),
        Err(LayoutError::MemberLimit)
    );
    assert_eq!(snapshot(&state), before);

    for _ in 1..MAX_TABS {
        state
            .create_tab(HOST_A, state.revision(), 24, 80)
            .expect("within tab limit");
    }
    assert_eq!(
        state.create_tab(HOST_A, state.revision(), 24, 80),
        Err(LayoutError::TabLimit)
    );

    let tab_id = snapshot(&state).tabs[0].tab_id;
    let mut leaves = vec![1];
    while leaves.len() < MAX_PANES_PER_TAB {
        let target = leaves.remove(0);
        let new_pane = state
            .create_pane(HOST_A, state.revision(), target, Axis::LeftRight, 24, 80)
            .expect("within pane limit");
        leaves.extend([target, new_pane]);
    }
    assert_eq!(state.pane_ids_in_tab(tab_id).len(), MAX_PANES_PER_TAB);
    assert_eq!(
        state.create_pane(HOST_A, state.revision(), leaves[0], Axis::LeftRight, 24, 80),
        Err(LayoutError::PaneLimit)
    );

    let mut depth_state =
        SessionState::new(HOST_A.to_vec(), ADDR_A.to_vec(), 24, 80).expect("valid initial layout");
    let mut leaf = 1;
    for _ in 0..4 {
        leaf = depth_state
            .create_pane_at(
                HOST_A,
                depth_state.revision(),
                leaf,
                Axis::TopBottom,
                NewPanePosition::Second,
                24,
                80,
            )
            .expect("within depth limit");
    }
    assert_eq!(
        depth_state.create_pane_at(
            HOST_A,
            depth_state.revision(),
            leaf,
            Axis::TopBottom,
            NewPanePosition::First,
            24,
            80
        ),
        Err(LayoutError::SplitDepthLimit)
    );
    assert_eq!(
        depth_state.create_pane_at(
            HOST_A,
            depth_state.revision(),
            leaf,
            Axis::TopBottom,
            NewPanePosition::Second,
            24,
            80
        ),
        Err(LayoutError::SplitDepthLimit)
    );
}

#[test]
fn only_hosts_can_delete_panes_and_mixed_tabs() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    assert_eq!(
        state.delete_pane(HOST_B, state.revision(), 1),
        Err(LayoutError::NotPaneHost { pane_id: 1 })
    );
    state
        .create_pane(HOST_B, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("B hosts new pane");
    state
        .create_tab(HOST_A, state.revision(), 24, 80)
        .expect("second tab");
    assert_eq!(
        state.delete_tab(HOST_A, state.revision(), 1),
        Err(LayoutError::NotTabHost { tab_id: 1 })
    );
}

#[test]
fn stale_requests_and_failed_grid_operations_do_not_change_the_snapshot() {
    let mut state = state();
    let before = snapshot(&state);
    assert_eq!(
        state.create_pane(HOST_A, 0, 1, Axis::LeftRight, 24, 80),
        Err(LayoutError::StaleRevision {
            expected: 1,
            got: 0,
        })
    );
    assert_eq!(
        state.create_tab(HOST_A, state.revision(), 0, 80),
        Err(LayoutError::InvalidGrid)
    );
    assert_eq!(snapshot(&state), before);
    assert_eq!(state.revision(), before.revision);
}

#[test]
fn last_leaf_and_last_tab_cannot_be_deleted_and_unknown_tab_wins_precedence() {
    let mut state = state();
    assert_eq!(
        state.delete_pane(HOST_A, state.revision(), 1),
        Err(LayoutError::LastPaneInTab { tab_id: 1 })
    );
    assert_eq!(
        state.delete_tab(HOST_A, state.revision(), 99),
        Err(LayoutError::UnknownTab { tab_id: 99 })
    );
    assert_eq!(
        state.delete_tab(HOST_A, state.revision(), 1),
        Err(LayoutError::LastTab)
    );
}

#[test]
fn ids_are_not_reused_after_deleted_tabs_or_panes() {
    let mut state = state();
    let pane = state
        .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("second pane");
    state
        .delete_pane(HOST_A, state.revision(), pane)
        .expect("delete second pane");
    assert_eq!(
        state
            .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
            .expect("third pane"),
        3
    );

    let first_new_tab = state
        .create_tab(HOST_A, state.revision(), 24, 80)
        .expect("first new tab");
    let second_new_tab = state
        .create_tab(HOST_A, state.revision(), 24, 80)
        .expect("second new tab");
    state
        .delete_tab(HOST_A, state.revision(), first_new_tab)
        .expect("delete first tab");
    assert_eq!(
        state
            .create_tab(HOST_A, state.revision(), 24, 80)
            .expect("replacement tab"),
        second_new_tab + 1
    );
}

#[test]
fn an_owner_can_delete_an_entire_tab_when_every_pane_is_local() {
    let mut state = state();
    let tab_id = state
        .create_tab(HOST_A, state.revision(), 24, 80)
        .expect("second tab");
    let only_pane = state.pane_ids_in_tab(tab_id)[0];
    let revision = state.revision();
    state
        .delete_tab(HOST_A, revision, tab_id)
        .expect("all-owned tab can be deleted");

    let snapshot = snapshot(&state);
    assert_eq!(state.revision(), revision + 1);
    assert_eq!(snapshot.tabs.len(), 1);
    assert!(!snapshot.panes.contains_key(&only_pane));
}

#[test]
fn successful_visible_mutations_advance_revision_once() {
    let mut state = state();
    let revision = state.revision();
    state
        .add_member(revision, HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member joins");
    assert_eq!(state.revision(), revision + 1);

    let revision = state.revision();
    let tab_id = state
        .create_tab(HOST_A, revision, 24, 80)
        .expect("create tab");
    assert_eq!(state.revision(), revision + 1);

    let pane_id = state
        .create_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("create pane");
    let revision = state.revision();
    state
        .delete_pane(HOST_A, revision, pane_id)
        .expect("delete pane");
    assert_eq!(state.revision(), revision + 1);

    let revision = state.revision();
    state
        .delete_tab(HOST_A, revision, tab_id)
        .expect("delete tab");
    assert_eq!(state.revision(), revision + 1);
}

#[test]
fn pane_reservation_is_invisible_until_the_creator_reports_ready() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    let before = snapshot(&state);
    let reservation = state
        .reserve_pane(HOST_B, state.revision(), 1, Axis::LeftRight, 30, 100)
        .expect("reserve B pane");

    assert_eq!(snapshot(&state), before);
    assert_eq!(state.revision(), before.revision);
    assert_eq!(
        state
            .pane_ready(HOST_B, state.revision(), reservation.reservation_id)
            .expect("creator reports ready"),
        ReservationCommit::Pane {
            pane_id: reservation.pane_id
        }
    );
    let snapshot = snapshot(&state);
    assert_eq!(snapshot.panes[&reservation.pane_id].host_peer_id, HOST_B);
    assert_eq!(snapshot.panes[&reservation.pane_id].grid_rows, 30);
    assert_eq!(state.revision(), before.revision + 1);
}

#[test]
fn tab_reservation_commits_its_reserved_tab_and_initial_pane() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    let reservation = state
        .reserve_tab(HOST_B, state.revision(), 33, 111)
        .expect("reserve B tab");
    let tab_id = reservation.tab_id.expect("tab reservation has tab ID");

    assert_eq!(
        state
            .pane_ready(HOST_B, state.revision(), reservation.reservation_id)
            .expect("commit tab"),
        ReservationCommit::Tab {
            tab_id,
            pane_id: reservation.pane_id,
        }
    );
    let snapshot = snapshot(&state);
    assert_eq!(
        tab(&snapshot, tab_id).root,
        Node::Leaf {
            pane_id: reservation.pane_id
        }
    );
    assert_eq!(snapshot.panes[&reservation.pane_id].host_peer_id, HOST_B);
}

#[test]
fn cancelling_or_expiring_a_reservation_keeps_layout_invisible_and_consumes_ids() {
    let mut state = state();
    let before = snapshot(&state);
    let first = state
        .reserve_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("reserve first pane");
    state
        .cancel_reservation(HOST_A, first.reservation_id)
        .expect("creator cancels");
    assert_eq!(snapshot(&state), before);
    assert_eq!(state.revision(), before.revision);

    let second = state
        .reserve_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("reserve second pane");
    state
        .expire_reservation(second.reservation_id)
        .expect("coordinator expires it");
    assert_eq!(snapshot(&state), before);
    let third = state
        .reserve_pane(HOST_A, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("reserve third pane");
    assert!(third.pane_id > second.pane_id);
    state
        .cancel_reservation(HOST_A, third.reservation_id)
        .expect("cleanup");

    let tab = state
        .reserve_tab(HOST_A, state.revision(), 24, 80)
        .expect("reserve a tab");
    state
        .expire_reservation(tab.reservation_id)
        .expect("expire tab reservation");
    assert_eq!(snapshot(&state), before);
}

#[test]
fn only_the_reservation_creator_can_commit_or_cancel_it() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    let reservation = state
        .reserve_pane(HOST_B, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("reserve B pane");
    let before = snapshot(&state);

    assert_eq!(
        state.pane_ready(HOST_A, state.revision(), reservation.reservation_id),
        Err(LayoutError::ReservationCreatorMismatch)
    );
    assert_eq!(
        state.cancel_reservation(HOST_A, reservation.reservation_id),
        Err(LayoutError::ReservationCreatorMismatch)
    );
    assert_eq!(snapshot(&state), before);
    state
        .cancel_reservation(HOST_B, reservation.reservation_id)
        .expect("creator can cancel");
}

#[test]
fn snapshots_associate_pane_hosts_with_bounded_member_addresses_and_reject_malformed_data() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    state
        .update_member_endpoint(state.revision(), HOST_B, b"updated-addr-b".to_vec())
        .expect("member B updates its endpoint");
    let pane_id = state
        .create_pane(HOST_B, state.revision(), 1, Axis::LeftRight, 24, 80)
        .expect("B pane");
    let valid_snapshot = snapshot(&state);
    assert_eq!(valid_snapshot.panes[&pane_id].host_peer_id, HOST_B);
    assert_eq!(
        valid_snapshot
            .members
            .iter()
            .find(|member| member.peer_id == HOST_B)
            .expect("B member")
            .endpoint_addr,
        b"updated-addr-b"
    );
    SessionState::validate_snapshot(&valid_snapshot).expect("valid snapshot");

    let mut malformed = valid_snapshot;
    malformed.tabs.clear();
    assert_eq!(
        SessionState::validate_snapshot(&malformed),
        Err(LayoutError::InvalidSnapshot)
    );

    let mut zero_tab_id = snapshot(&state);
    zero_tab_id.tabs[0].tab_id = 0;
    assert_eq!(
        SessionState::validate_snapshot(&zero_tab_id),
        Err(LayoutError::InvalidSnapshot)
    );

    let mut zero_pane_id = snapshot(&state);
    let pane = zero_pane_id
        .panes
        .remove(&pane_id)
        .expect("pane exists in snapshot");
    zero_pane_id
        .panes
        .insert(0, p2pmux::layout::Pane { pane_id: 0, ..pane });
    zero_pane_id.tabs[0].root = Node::Leaf { pane_id: 0 };
    assert_eq!(
        SessionState::validate_snapshot(&zero_pane_id),
        Err(LayoutError::InvalidSnapshot)
    );

    let mut oversized_peer = snapshot(&state);
    let original_peer = oversized_peer.members[0].peer_id.clone();
    let replacement_peer = vec![0; MAX_PEER_ID_BYTES + 1];
    oversized_peer.members[0].peer_id = replacement_peer.clone();
    for pane in oversized_peer.panes.values_mut() {
        if pane.host_peer_id == original_peer {
            pane.host_peer_id = replacement_peer.clone();
        }
    }
    assert_eq!(
        SessionState::validate_snapshot(&oversized_peer),
        Err(LayoutError::InvalidSnapshot)
    );
}

/// The whole of "open a terminal on that machine", at the layer that decides
/// which machine a pane belongs to.
///
/// The pane must commit against the machine that was asked, not the one that
/// asked — otherwise the pty runs in one place while the layout says another,
/// which is worse than the feature not existing.
#[test]
fn a_pane_asked_for_on_another_machine_is_hosted_by_that_machine() {
    let mut state = state();
    state
        .add_member(state.revision(), HOST_B.to_vec(), ADDR_B.to_vec())
        .expect("member B joins");
    let reservation = state
        .reserve_tab_on(HOST_A, HOST_B, state.revision(), 30, 100)
        .expect("A asks B for a terminal");

    assert_eq!(reservation.host_peer_id, HOST_B);
    assert_eq!(
        state.pane_ready(HOST_A, state.revision(), reservation.reservation_id),
        Err(LayoutError::ReservationCreatorMismatch),
        "the machine that asked cannot report a pane it is not hosting ready"
    );
    state
        .pane_ready(HOST_B, state.revision(), reservation.reservation_id)
        .expect("B reports the terminal it spawned");

    let snapshot = snapshot(&state);
    assert_eq!(snapshot.panes[&reservation.pane_id].host_peer_id, HOST_B);
}

/// Asking a machine that is not here is its own refusal, not a silence and not
/// a pane that quietly opens somewhere else.
#[test]
fn a_pane_asked_for_on_a_machine_that_is_not_here_is_refused() {
    let mut state = state();

    assert_eq!(
        state.reserve_tab_on(HOST_A, HOST_B, state.revision(), 24, 80),
        Err(LayoutError::UnknownTarget)
    );
    assert_eq!(
        state.reserve_pane_on(
            HOST_A,
            HOST_B,
            state.revision(),
            1,
            Axis::LeftRight,
            NewPanePosition::Second,
            24,
            80,
        ),
        Err(LayoutError::UnknownTarget)
    );
}

/// The default has to be exactly what it was, or every split anyone types
/// changes meaning.
#[test]
fn a_reservation_that_names_no_machine_is_hosted_by_the_one_that_asked() {
    let mut state = state();
    let reservation = state
        .reserve_tab(HOST_A, state.revision(), 24, 80)
        .expect("reserve");

    assert_eq!(reservation.host_peer_id, HOST_A);
}
