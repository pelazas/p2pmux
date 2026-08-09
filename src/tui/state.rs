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
    Quit(QuitAction),
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
    /// What the agent said it is doing. Populated only for panes hosted on this
    /// machine — a peer's node strips it before publishing, so a remote row's is
    /// always empty.
    pub message: String,
}
/// A machine this one is paired with.
///
/// Pairing is permanent and mutual, so this is a record of a decision rather
/// than a connection: a paired machine that is switched off is still paired,
/// and the inbox says `asleep` rather than forgetting it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairedMachine {
    pub name: String,
    /// The machine's peer identity, hex-encoded, as seen when it was paired.
    ///
    /// This is what ownership is actually decided on. A name is chosen by the
    /// machine that carries it and two machines can pick the same one; a peer
    /// id is the transport's own authenticated identity, so a stranger cannot
    /// present someone else's and a rename cannot cost a machine its place in
    /// the fleet.
    ///
    /// `None` for records written before this field existed. Those fall back to
    /// matching on the name — no worse than what they had — and are upgraded in
    /// place the first time the machine is seen in a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    /// Whether that machine accepts work from this one — or `None` when it has
    /// never said.
    ///
    /// The answer is given on the machine it is about, during its own pairing,
    /// and there is no channel back: the only thing that crosses machines is
    /// the shared layout, whose member list is signed and hash-chained, and the
    /// inbox is built on never touching that. So a machine knows its own answer
    /// and nothing about anyone else's, and the column says `—` rather than
    /// guessing `no` — which would read as a refusal that was never made.
    ///
    /// Nothing acts on it either way yet. It is the consent primitive that will
    /// later make starting a terminal on another machine legal without widening
    /// the trust model, and it means *accepts work from me*, never *from anyone
    /// in the session*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts_work: Option<bool>,
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
/// Which piece of invite material a panel asked the client to copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShareCopy {
    Ticket,
    Code,
    /// The same invite, as the `p2pmux pair` line the add-machine panel shows.
    /// Pairing and joining resolve the same code; what differs is what the
    /// machine at the other end does with it afterwards.
    Pair,
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
    Share,
    Rename(RenamePrompt),
    ConfirmDeleteTab {
        tab_id: TabId,
        pane_count: usize,
    },
    /// Ctrl+Q, asking which of the two leavings was meant.
    Quit,
    /// `a` on the inbox: the line to run on the machine being added.
    AddMachine(AddMachinePrompt),
}
/// The add-machine panel, and what it needs to notice the machine arriving.
///
/// Adding a machine used to mean leaving the UI: `p2pmux pair` on one machine,
/// carry the code to the other, and find out whether it worked by running
/// something else. The panel keeps both halves on the screen the fleet is
/// already on, and stays up to report the join rather than leaving the user to
/// go and check.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::tui) struct AddMachinePrompt {
    /// The machines that were already here when the panel went up. Anything
    /// that appears afterwards is the one being added, which is the whole
    /// difference between instructions and a report.
    pub(in crate::tui) known: Vec<String>,
}
/// The two ways out, which one keystroke used to conflate.
///
/// Detaching and ending a session look identical from the keyboard and could
/// not be less alike afterwards, so Ctrl+Q asks rather than picks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuitAction {
    /// Leave. Every pane keeps running and `p2pmux attach` comes back to it.
    Detach,
    /// Stop this machine's node. Its panes die with it, and a session with
    /// nobody left hosting a pane is over.
    Kill,
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
