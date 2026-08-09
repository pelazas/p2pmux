//! Fixtures shared by the tests of several `tui` submodules.

use std::collections::BTreeMap;

use crate::{
    layout::{Axis, LayoutSnapshot, Node, Pane, Tab},
    tui::{AgentOverlayRow, MultiPaneTui, PaneMouseProtocol},
};

pub(in crate::tui) fn mouse_protocol(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> PaneMouseProtocol {
    PaneMouseProtocol { mode, encoding }
}

pub(in crate::tui) fn sgr_protocol(mode: vt100::MouseProtocolMode) -> PaneMouseProtocol {
    mouse_protocol(mode, vt100::MouseProtocolEncoding::Sgr)
}

pub(in crate::tui) fn layout(tabs: Vec<Tab>, panes: &[(u64, u16, u16)]) -> LayoutSnapshot {
    LayoutSnapshot {
        revision: 1,
        members: vec![crate::layout::Member {
            peer_id: b"host".to_vec(),
            endpoint_addr: b"endpoint".to_vec(),
            display_name: String::new(),
            kind: Default::default(),
        }],
        tabs,
        panes: panes
            .iter()
            .map(|(pane_id, rows, cols)| {
                (
                    *pane_id,
                    Pane {
                        pane_id: *pane_id,
                        host_peer_id: b"host".to_vec(),
                        locked: false,
                        exited: false,
                        grid_rows: *rows,
                        grid_cols: *cols,
                        title: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

pub(in crate::tui) fn split_layout() -> LayoutSnapshot {
    layout(
        vec![Tab {
            tab_id: 1,
            root: Node::Split {
                axis: Axis::LeftRight,
                first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                first: Box::new(Node::Leaf { pane_id: 1 }),
                second: Box::new(Node::Split {
                    axis: Axis::TopBottom,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 2 }),
                    second: Box::new(Node::Leaf { pane_id: 3 }),
                }),
            },

            title: None,
        }],
        &[(1, 4, 10), (2, 4, 10), (3, 4, 10)],
    )
}

pub(in crate::tui) fn agent_row(
    pane_id: u64,
    tab_ordinal: usize,
    pane_ordinal: usize,
) -> AgentOverlayRow {
    AgentOverlayRow {
        message: String::new(),
        pane_id,
        process_pid: 0,
        tab_ordinal,
        pane_ordinal,
        tab_label: format!("Tab #{tab_ordinal}"),
        pane_label: format!("Pane #{pane_ordinal}"),
        kind: String::from("codex"),
        cwd: String::from("/very/long/repository/path"),
        state: crate::protocol::AgentRosterState::Working,
        working_since_unix_ms: 1_725_000_000_123,
        host: String::from("Host"),
        controller: String::from("free"),
    }
}

/// A tui with one pane per agent, each on its own tab, and rows whose machine,
/// agent kind and state are set from the caller's table. Home is what the rows
/// are for, so the fixture speaks in the columns Home draws.
pub(in crate::tui) fn home_tui(
    rows: &[(&str, &str, crate::protocol::AgentRosterState)],
) -> MultiPaneTui {
    // A validated layout always has a tab, so an agentless fixture is one empty
    // terminal — exactly what a first run looks like.
    let count = rows.len().max(1) as u64;
    let tabs = (1..=count)
        .map(|pane_id| Tab {
            tab_id: pane_id,
            root: Node::Leaf { pane_id },
            title: None,
        })
        .collect();
    let panes = (1..=count)
        .map(|pane_id| (pane_id, 2, 8))
        .collect::<Vec<_>>();
    let mut tui = MultiPaneTui::new(layout(tabs, &panes)).expect("valid layout");
    // A real client always knows its own peer id, and the machines rail reads it
    // to mark which row is the machine being looked at.
    tui.local_peer_id = Some(b"host".to_vec());
    tui.set_agent_rows(
        rows.iter()
            .enumerate()
            .map(|(index, (host, kind, state))| AgentOverlayRow {
                host: (*host).to_owned(),
                kind: (*kind).to_owned(),
                state: *state,
                ..agent_row(index as u64 + 1, index + 1, 1)
            })
            .collect(),
    );
    tui.repair_home_selection();
    tui
}

pub(in crate::tui) fn two_tab_presence_tui() -> MultiPaneTui {
    let mut snapshot = layout(
        vec![
            Tab {
                tab_id: 10,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            },
            Tab {
                tab_id: 20,
                root: Node::Leaf { pane_id: 2 },
                title: None,
            },
        ],
        &[(1, 2, 2), (2, 2, 2)],
    );
    snapshot.members.push(crate::layout::Member {
        peer_id: b"tis".to_vec(),
        endpoint_addr: b"endpoint-tis".to_vec(),
        display_name: "tis".into(),
        kind: Default::default(),
    });
    MultiPaneTui::new(snapshot).expect("valid layout")
}

pub(in crate::tui) fn watcher(
    peer_id: &[u8],
    tab_id: u64,
    pane_id: u64,
) -> crate::local_ipc::PresenceRow {
    crate::local_ipc::PresenceRow {
        peer_id: peer_id.to_vec(),
        tab_id,
        pane_id,
    }
}

pub(in crate::tui) fn named_members() -> Vec<crate::layout::Member> {
    vec![
        crate::layout::Member {
            peer_id: b"host".to_vec(),
            endpoint_addr: b"endpoint".to_vec(),
            display_name: "pelazas".into(),
            kind: Default::default(),
        },
        crate::layout::Member {
            peer_id: b"tis".to_vec(),
            endpoint_addr: b"endpoint-tis".to_vec(),
            display_name: "tis".into(),
            kind: Default::default(),
        },
        crate::layout::Member {
            peer_id: b"ana".to_vec(),
            endpoint_addr: b"endpoint-ana".to_vec(),
            display_name: "ana".into(),
            kind: Default::default(),
        },
    ]
}

pub(in crate::tui) fn presence_members(count: usize) -> Vec<crate::layout::Member> {
    (0..count)
        .map(|index| crate::layout::Member {
            peer_id: vec![index as u8; 4],
            endpoint_addr: vec![index as u8],
            display_name: format!("member{index}"),
            kind: Default::default(),
        })
        .collect()
}
