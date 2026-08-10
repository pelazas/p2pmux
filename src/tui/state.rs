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
    /// The answer to a held remote-work request, travelling from the client a
    /// human is looking at to the node that is holding the request.
    AnswerRemoteWork {
        approved: bool,
    },
    /// A terminal on another machine in the session.
    ///
    /// Separate from [`Self::CreateTab`] rather than an option on it, because
    /// every caller of that one means "here" and should not have to say so.
    CreateTabOn {
        peer_id: Vec<u8>,
        /// What to run instead of a login shell. Empty for a shell.
        command: Vec<String>,
        /// The machine's name, carried so a refusal can name it. The layout
        /// only knows peer ids, and "1a2b3c4d refused" is not a sentence.
        name: String,
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
    /// The pane this agent runs in, or `0` for one running outside p2pmux.
    pub pane_id: PaneId,
    /// The agent's own process, for a row with no pane. `0` otherwise.
    pub process_pid: u32,
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

impl AgentOverlayRow {
    /// What identifies this row to the cursor.
    ///
    /// The inbox used to key its selection on a pane id, which was fine while
    /// every agent lived in a pane. An agent running under systemd has no pane,
    /// so identity has to be the pair that is unique either way: which machine,
    /// and which pane or process on it.
    pub(in crate::tui) fn row_id(&self) -> HomeRowId {
        HomeRowId {
            host: self.host.clone(),
            pane_id: self.pane_id,
            process_pid: self.process_pid,
        }
    }

    /// Whether this agent is reachable only by starting its own chat command,
    /// rather than by jumping to a pane p2pmux already owns.
    pub(in crate::tui) fn outside_p2pmux(&self) -> bool {
        self.pane_id == 0
    }
}

/// The identity of one inbox row. See [`AgentOverlayRow::row_id`].
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HomeRowId {
    pub host: String,
    pub pane_id: PaneId,
    pub process_pid: u32,
}
/// A machine this one is paired with.
///
/// Pairing is permanent and mutual, so this is a record of a decision rather
/// than a connection: a paired machine that is switched off is still paired,
/// and the inbox says `asleep` rather than forgetting it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairedMachine {
    pub name: String,
    /// The machine's own identity, hex-encoded, as proved when it was paired.
    ///
    /// This is what ownership is decided on. A name is chosen by the machine
    /// that carries it and two machines can pick the same one; a machine id is
    /// a public key whose holder signs the peer id the transport authenticated,
    /// so a stranger cannot present someone else's and a rename cannot cost a
    /// machine its place in the fleet.
    ///
    /// Deliberately *not* the node's peer id, which was the first thing tried
    /// and does not survive contact with a second machine: a peer id is
    /// generated per process, so the droplet that follows you into a new
    /// session arrives as a peer the fleet record has never seen. What is
    /// recognized has to outlive the process, and this does.
    ///
    /// `None` for records written before this field existed. Those fall back to
    /// matching on the name — no worse than what they had — and are upgraded in
    /// place the first time that machine proves which one it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
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
    /// Another machine of yours wants to start something here, and this
    /// machine is configured to be asked first.
    ///
    /// It goes up on the machine the terminal would run on, never on the one
    /// that asked, because that is the only machine whose owner's answer means
    /// anything. Ignoring it is a refusal: the request expires.
    ConfirmRemoteWork {
        /// What would be launched, as it would be typed. Empty is a shell,
        /// which the prompt says in those words.
        command: Vec<String>,
    },
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
/// One end of a selection, pinned to the text under it rather than to a place
/// on the screen.
///
/// A cell alone cannot say what was selected once the pane scrolls: row 3 is a
/// different line before and after a scroll, so an end recorded as row 3 slides
/// onto whatever moves under it. Recording the offset the end was placed at
/// fixes it to its line instead, and scrolling the pane then moves the *screen*
/// rather than the selection — which is what dragging past the edge has to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) struct SelectionPoint {
    /// The pane's scrollback offset when this end was placed.
    pub(in crate::tui) scrollback: usize,
    pub(in crate::tui) cell: ScreenCell,
}

impl SelectionPoint {
    /// This end's line, in a coordinate that no amount of scrolling changes.
    ///
    /// Scrolling back by one moves every line down one row, so `row -
    /// scrollback` is the same number before and after — smaller for older
    /// lines. Signed, because an end above the viewport has a negative one.
    pub(in crate::tui) fn line(self) -> i64 {
        i64::from(self.cell.row) - self.scrollback as i64
    }

    /// Which screen row this end sits on with the pane scrolled back
    /// `scrollback` rows. Off-screen ends are ordinary here: that is what a
    /// selection extending past the top of the viewport *is*.
    pub(in crate::tui) fn row_at(self, scrollback: usize) -> i64 {
        self.line() + scrollback as i64
    }

    fn order(self) -> (i64, u16) {
        (self.line(), self.cell.col)
    }
}

/// A local, pane-scoped text selection. Both ends are inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) struct PaneTextSelection {
    pub(in crate::tui) pane_id: PaneId,
    pub(in crate::tui) anchor: SelectionPoint,
    pub(in crate::tui) cursor: SelectionPoint,
}
impl PaneTextSelection {
    pub(in crate::tui) fn is_empty(self) -> bool {
        self.anchor.order() == self.cursor.order()
    }

    /// Whether `cell` is inside the selection, on a pane scrolled back
    /// `scrollback` rows.
    pub(in crate::tui) fn contains(self, cell: ScreenCell, scrollback: usize) -> bool {
        let (start, end) = self.bounds();
        let (start_row, end_row) = (start.row_at(scrollback), end.row_at(scrollback));
        let row = i64::from(cell.row);
        match row {
            row if row == start_row && row == end_row => {
                (start.cell.col..=end.cell.col).contains(&cell.col)
            }
            row if row == start_row => cell.col >= start.cell.col,
            row if row == end_row => cell.col <= end.cell.col,
            row => (start_row..end_row).contains(&row),
        }
    }

    pub(in crate::tui) fn bounds(self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor.order() <= self.cursor.order() {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}
