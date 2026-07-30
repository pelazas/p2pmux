//! Fixtures shared by the tests of several `tui` submodules.

use std::collections::BTreeMap;

use crate::{
    layout::{Axis, LayoutSnapshot, Node, Pane, Tab},
    tui::PaneMouseProtocol,
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
