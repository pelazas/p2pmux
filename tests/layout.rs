use p2pmux::layout::{
    Axis, LayoutError, MAX_MEMBERS, MAX_PANES_PER_TAB, MAX_TABS, Node, SessionState,
};

const HOST_A: &[u8] = b"host-a";
const HOST_B: &[u8] = b"host-b";

fn state() -> SessionState {
    SessionState::new(HOST_A.to_vec(), 24, 80).expect("valid initial layout")
}

#[test]
fn initial_state_has_one_tab_and_one_hosted_pane() {
    let state = state();

    assert_eq!(state.revision, 1);
    assert_eq!(state.members.len(), 1);
    assert_eq!(state.tabs.len(), 1);
    assert_eq!(state.panes.len(), 1);
    assert_eq!(state.tabs[0].root, Node::Leaf { pane_id: 1 });
    assert_eq!(state.panes[&1].host_peer_id, HOST_A);
    assert_eq!(state.panes[&1].grid_rows, 24);
    assert_eq!(state.panes[&1].grid_cols, 80);
}

#[test]
fn creating_panes_uses_the_requested_split_axis() {
    let mut state = state();
    let first_revision = state.revision;
    let right = state
        .create_pane(HOST_A, first_revision, 1, Axis::LeftRight, 30, 100)
        .expect("right split");

    assert_eq!(right, 2);
    assert_eq!(state.revision, first_revision + 1);
    assert_eq!(
        state.tabs[0].root,
        Node::Split {
            axis: Axis::LeftRight,
            first: Box::new(Node::Leaf { pane_id: 1 }),
            second: Box::new(Node::Leaf { pane_id: right }),
        }
    );

    let down = state
        .create_pane(HOST_A, state.revision, right, Axis::TopBottom, 20, 70)
        .expect("down split");
    assert!(matches!(
        &state.tabs[0].root,
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
            } if matches!(first.as_ref(), Node::Leaf { pane_id } if *pane_id == right)
                && matches!(second.as_ref(), Node::Leaf { pane_id } if *pane_id == down)
        )
    ));
}

#[test]
fn deleting_a_pane_expands_its_sibling() {
    let mut state = state();
    let sibling = state
        .create_pane(HOST_A, state.revision, 1, Axis::LeftRight, 24, 80)
        .expect("split");

    state
        .delete_pane(HOST_A, state.revision, sibling)
        .expect("host can delete pane");

    assert_eq!(state.tabs[0].root, Node::Leaf { pane_id: 1 });
    assert!(!state.panes.contains_key(&sibling));
}

#[test]
fn membership_and_tab_and_pane_limits_are_enforced() {
    let mut state = state();
    for member in 1..MAX_MEMBERS {
        state
            .add_member(state.revision, format!("member-{member}").into_bytes())
            .expect("within member limit");
    }
    let revision = state.revision;
    assert_eq!(
        state.add_member(revision, b"one-too-many".to_vec()),
        Err(LayoutError::MemberLimit)
    );
    assert_eq!(state.revision, revision);

    for _ in 1..MAX_TABS {
        state
            .create_tab(HOST_A, state.revision, 24, 80)
            .expect("within tab limit");
    }
    let revision = state.revision;
    assert_eq!(
        state.create_tab(HOST_A, revision, 24, 80),
        Err(LayoutError::TabLimit)
    );
    assert_eq!(state.revision, revision);

    let tab_id = state.tabs[0].tab_id;
    let mut leaves = vec![1];
    while leaves.len() < MAX_PANES_PER_TAB {
        let target = leaves.remove(0);
        let new_pane = state
            .create_pane(HOST_A, state.revision, target, Axis::LeftRight, 24, 80)
            .expect("within pane limit");
        leaves.push(target);
        leaves.push(new_pane);
    }
    assert_eq!(state.pane_ids_in_tab(tab_id).len(), MAX_PANES_PER_TAB);
    let revision = state.revision;
    assert_eq!(
        state.create_pane(HOST_A, revision, leaves[0], Axis::LeftRight, 24, 80),
        Err(LayoutError::PaneLimit)
    );
    assert_eq!(state.revision, revision);
}

#[test]
fn splitting_beyond_depth_four_is_rejected() {
    let mut state = state();
    let mut leaf = 1;
    for _ in 0..4 {
        leaf = state
            .create_pane(HOST_A, state.revision, leaf, Axis::TopBottom, 24, 80)
            .expect("within depth limit");
    }

    let revision = state.revision;
    assert_eq!(
        state.create_pane(HOST_A, revision, leaf, Axis::TopBottom, 24, 80),
        Err(LayoutError::SplitDepthLimit)
    );
    assert_eq!(state.revision, revision);
}

#[test]
fn only_the_pane_host_can_delete_it() {
    let mut state = state();
    state
        .add_member(state.revision, HOST_B.to_vec())
        .expect("member B joins");
    let revision = state.revision;

    assert_eq!(
        state.delete_pane(HOST_B, revision, 1),
        Err(LayoutError::NotPaneHost { pane_id: 1 })
    );
    assert_eq!(state.revision, revision);
}

#[test]
fn deleting_a_tab_requires_ownership_of_every_pane() {
    let mut state = state();
    state
        .add_member(state.revision, HOST_B.to_vec())
        .expect("member B joins");
    state
        .create_pane(HOST_B, state.revision, 1, Axis::LeftRight, 24, 80)
        .expect("B hosts new pane");
    let revision = state.revision;

    state
        .create_tab(HOST_A, state.revision, 24, 80)
        .expect("a second tab allows deletion");
    assert_eq!(
        state.delete_tab(HOST_A, state.revision, 1),
        Err(LayoutError::NotTabHost { tab_id: 1 })
    );
    assert_eq!(state.revision, revision + 1);
}

#[test]
fn stale_requests_do_not_change_the_layout() {
    let mut state = state();
    let before = state.clone();

    assert_eq!(
        state.create_pane(HOST_A, 0, 1, Axis::LeftRight, 24, 80),
        Err(LayoutError::StaleRevision {
            expected: 1,
            got: 0,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn last_leaf_and_last_tab_cannot_be_deleted() {
    let mut state = state();
    let revision = state.revision;

    assert_eq!(
        state.delete_pane(HOST_A, revision, 1),
        Err(LayoutError::LastPaneInTab { tab_id: 1 })
    );
    assert_eq!(
        state.delete_tab(HOST_A, revision, 1),
        Err(LayoutError::LastTab)
    );
    assert_eq!(state.revision, revision);
}

#[test]
fn pane_ids_are_not_reused_after_deletion() {
    let mut state = state();
    let second = state
        .create_pane(HOST_A, state.revision, 1, Axis::LeftRight, 24, 80)
        .expect("second pane");
    state
        .delete_pane(HOST_A, state.revision, second)
        .expect("delete second pane");

    assert_eq!(
        state
            .create_pane(HOST_A, state.revision, 1, Axis::LeftRight, 24, 80)
            .expect("third pane"),
        3
    );
}
