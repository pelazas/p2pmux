//! What a node hands back to a client: one screen per pane, the lease that
//! goes with it, and a scrollback window.

use std::collections::BTreeMap;

use crate::{layout::PaneId, screen::ScreenFrame};

pub(crate) enum NodeScreenSnapshot {
    Local {
        frame: ScreenFrame,
        history_len: u64,
        history_end: u64,
    },
    Remote {
        sequence: u64,
        kitty_keyboard_active: bool,
    },
}
pub(crate) type NodeScreenSnapshots = BTreeMap<PaneId, NodeScreenSnapshot>;
pub(crate) type NodeLeaseSnapshots = BTreeMap<PaneId, (bool, Option<Vec<u8>>, bool)>;
#[derive(Clone, Debug)]
pub(crate) struct LocalScrollbackWindow {
    pub total_rows: u64,
    pub screen: vt100::Screen,
}
