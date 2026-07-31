//! The value types the multi-pane UI keeps: chord and modal state, per-pane
//! view state, the intents a keystroke turns into, and text selection.

use std::collections::BTreeMap;

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::{
    layout::{Axis, NewPanePosition, PaneId, TabId},
    protocol::AgentRosterState,
};

/// The in-progress multi-pane command prefix, kept entirely local to one terminal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChordMode {
    #[default]
    None,
    Pane,
    Tab,
}
/// Metadata used to draw a pane before its runtime has delivered a screen and lease.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PaneViewState {
    pub ready: bool,
    pub controller_peer_id: Option<Vec<u8>>,
    pub controller_active: bool,
    pub(in crate::tui) scrollback: usize,
}
impl PaneViewState {
    pub fn from_chrome(
        ready: bool,
        controller_peer_id: Option<Vec<u8>>,
        controller_active: bool,
    ) -> Self {
        Self {
            ready,
            controller_peer_id,
            controller_active,
            scrollback: 0,
        }
    }
}
/// User operations emitted by the TUI. Session code owns all resulting mutations and PTYs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UiIntent {
    CreatePane {
        target_pane_id: PaneId,
        axis: Axis,
        position: NewPanePosition,
        grid_rows: u16,
        grid_cols: u16,
    },
    DeletePane {
        pane_id: PaneId,
    },
    CreateTab {
        grid_rows: u16,
        grid_cols: u16,
    },
    DeleteTab {
        tab_id: TabId,
    },
    FocusPane {
        pane_id: PaneId,
    },
    SwitchTab {
        tab_id: TabId,
    },
    SetSplitRatio {
        pane_id: PaneId,
        axis: Axis,
        first_share_bps: u16,
        base_revision: u64,
    },
    RenamePane {
        pane_id: PaneId,
        title: String,
    },
    RenameTab {
        tab_id: TabId,
        title: String,
    },
    SetPaneLock {
        pane_id: PaneId,
        locked: bool,
    },
    /// Close or reopen the whole session to peers that have never joined it.
    ///
    /// Unlike `SetPaneLock` this is not layout state and never travels as a layout
    /// request: only the coordinator answers joins, so only the coordinator can enforce
    /// it, and a guest pressing the key is told so rather than silently ignored.
    SetSessionLock {
        locked: bool,
    },
}
/// Whether a terminal key belongs to the mux or should later be offered to the focused pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyHandling {
    Forward,
    Consumed(Vec<UiIntent>),
    Quit,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MouseHandling {
    pub intents: Vec<UiIntent>,
    pub copy_selection_requested: bool,
    /// An xterm mouse report the focused pane's child asked to receive.
    pub forward_bytes: Option<Vec<u8>>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentOverlayRow {
    pub pane_id: PaneId,
    pub tab_ordinal: usize,
    pub pane_ordinal: usize,
    pub tab_label: String,
    pub pane_label: String,
    pub kind: String,
    pub cwd: String,
    pub state: AgentRosterState,
    pub working_since_unix_ms: u64,
    pub host: String,
    pub controller: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::tui) enum RenameTarget {
    Pane(PaneId),
    Tab(TabId),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::tui) struct RenamePrompt {
    pub(in crate::tui) target: RenameTarget,
    pub(in crate::tui) value: String,
    pub(in crate::tui) error: Option<String>,
}
/// Which piece of invite material the share modal asked the client to copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShareCopy {
    Ticket,
    Code,
}
/// The host-only invite material the share modal renders.
///
/// Only the coordinator's node holds a ticket, so a guest arrives here with empty fields and
/// gets told so rather than being offered an invite it cannot make. `code` is additionally
/// absent when the rendezvous service could not be reached, which is why the two are separate
/// options rather than one: a session with no code still has a working invite.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShareView<'a> {
    pub code: Option<&'a str>,
    pub ticket: Option<&'a str>,
    /// The result of the last copy, shown in the modal rather than the footer.
    pub notice: Option<&'a str>,
}
/// What a coordinator can hand out, owned by the runtime.
///
/// The two travel together everywhere and are absent together on a member, so they are one
/// value rather than a pair of options threaded through every constructor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::tui) struct Invite {
    pub ticket: Option<String>,
    pub code: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::tui) enum ModalState {
    #[default]
    None,
    Agents,
    Share,
    Rename(RenamePrompt),
    ConfirmDeleteTab {
        tab_id: TabId,
        pane_count: usize,
    },
}
/// Rectangles for one rendered terminal frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneGeometry {
    pub tab_bar: Rect,
    pub tab_labels: BTreeMap<TabId, Rect>,
    pub content: Rect,
    pub footer: Rect,
    pub panes: BTreeMap<PaneId, Rect>,
}
/// One cell in a pane's fixed VT grid.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::tui) struct ScreenCell {
    pub(in crate::tui) row: u16,
    pub(in crate::tui) col: u16,
}
/// A local, pane-scoped text selection. Both ends are inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) struct PaneTextSelection {
    pub(in crate::tui) pane_id: PaneId,
    pub(in crate::tui) anchor: ScreenCell,
    pub(in crate::tui) cursor: ScreenCell,
}
impl PaneTextSelection {
    pub(in crate::tui) fn is_empty(self) -> bool {
        self.anchor == self.cursor
    }

    pub(in crate::tui) fn contains(self, cell: ScreenCell) -> bool {
        let (start, end) = self.bounds();
        match cell.row {
            row if row == start.row && row == end.row => (start.col..=end.col).contains(&cell.col),
            row if row == start.row => cell.col >= start.col,
            row if row == end.row => cell.col <= end.col,
            row => (start.row..end.row).contains(&row),
        }
    }

    pub(in crate::tui) fn bounds(self) -> (ScreenCell, ScreenCell) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}
