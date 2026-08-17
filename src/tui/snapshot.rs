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

/// Why a node did or did not hand over a pane's history.
///
/// The three refusals are kept apart because each one has to be said to a
/// person, and they are not the same sentence. [`Self::Empty`] is not even a
/// refusal: a pane nothing has scrolled off yet has no history for the most
/// ordinary reason there is, and the honest report of that is silence.
///
/// The window is boxed so the three refusals do not each carry its size. This
/// is answered once per wheel notch and the window already owns a cloned
/// screen, so the indirection costs nothing worth measuring.
#[derive(Clone, Debug)]
pub(crate) enum LocalScrollback {
    Window(Box<LocalScrollbackWindow>),
    /// Nothing has scrolled off this pane yet.
    Empty,
    /// A full-screen program owns the pane, and its screen is not scrollback.
    AlternateScreen,
    /// This node does not host that pane, so it has no history to give.
    NotOurs,
}
