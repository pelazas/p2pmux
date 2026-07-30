//! The fixed-grid local terminal renderer and input loop.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fmt,
    fs::OpenOptions,
    io,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self as sync_mpsc, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, watch};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use serde::{Deserialize, Serialize};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode,
        enable_raw_mode,
    },
};
use iroh::EndpointAddr;
use portable_pty::PtySize;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Widget},
};

use crate::{
    agent_detect::{
        PaneAgentTracker, ProcessSnapshot, SysinfoSampler, classify_pane_tree, cwd_for_pid,
        sample_global_snapshot,
    },
    config::UiTheme,
    kitty_keyboard::KittyKeyboardTracker,
    layout::{
        Axis, LayoutError, LayoutSnapshot, NewPanePosition, Node, PaneId, TabId, normalize_title,
    },
    lease::{IDLE_AFTER, LeaseDecision, LeaseManager, LeaseState},
    local_ipc::AgentOverlaySnapshotRow,
    protocol::{
        AgentRoster, AgentRosterEntry, AgentRosterState, CreatePane, CreateTab, DeletePane,
        DeleteTab, LayoutRequest, MarkPaneExited, NewPanePosition as ProtocolNewPanePosition,
        PaneDescriptor, PaneFailed, PaneReady, RenamePane, RenameTab, SetPaneLock, SplitAxis,
    },
    pty_host::PtyHost,
    screen::{GuestScreen, HostScreen, SCROLLBACK_LINES, ScreenFrame, SyncGate},
    session::{
        CoordinatorResponse, GuestEvent, GuestPane, HostControlEvent, HostPaneChannels,
        LayoutControlEvent, LayoutControlQueueError, PaneLayoutReconciler, PaneServer,
        SharedLayoutHost, SharedLayoutMember, layout_snapshot_from_state, pane_wire_id,
        subscribe_pane,
    },
    transport::Transport,
};

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

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

const TOP_BAR_BRAND: &str = "p2pmux";
const TOP_BAR_BRAND_SEPARATOR: &str = " │ ";
const TAB_BAR_SEPARATOR: &str = " · ";
const CONTROL_HELP: &str = "Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS";
const ESC_PREFIX_WINDOW: Duration = Duration::from_millis(50);
const CHORD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const PANE_SCROLL_WHEEL_STEP: usize = 3;

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
const AGENT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const AGENT_TOGGLE_WINDOW: Duration = Duration::from_millis(200);
pub(crate) const AGENT_OVERLAY_ANIMATION_INTERVAL: Duration = Duration::from_millis(100);
const AGENT_OVERLAY_CARD_LINES: usize = 3;

struct UiDebugLog {
    path: Option<PathBuf>,
    start: Instant,
    write_lock: Mutex<()>,
}

pub(crate) fn ui_debug_log(event: &str, fields: fmt::Arguments<'_>) {
    static UI_DEBUG_LOG: OnceLock<UiDebugLog> = OnceLock::new();
    let debug_log = UI_DEBUG_LOG.get_or_init(|| {
        let path = env::var("P2PMUX_DEBUG_UI").ok().and_then(|value| {
            if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("false") {
                None
            } else if value.contains('/') || value.starts_with('.') {
                Some(PathBuf::from(value))
            } else {
                Some(PathBuf::from("/tmp/p2pmux-ui.log"))
            }
        });
        UiDebugLog {
            path,
            start: Instant::now(),
            write_lock: Mutex::new(()),
        }
    });
    let Some(path) = &debug_log.path else {
        return;
    };
    let line = format!(
        "p2pmux ui: {} {} {}\n",
        debug_log.start.elapsed().as_millis(),
        event,
        fields
    );
    let Ok(_write_lock) = debug_log.write_lock.lock() else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}

enum FooterSegment {
    Text(&'static str),
    Key(&'static str),
    OrangeKey(&'static str),
}

const NORMAL_FOOTER: &[FooterSegment] = &[
    FooterSegment::OrangeKey("Ctrl"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("p"),
    FooterSegment::Text("> PANE   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TAB   <"),
    FooterSegment::Key("q"),
    FooterSegment::Text("> QUIT   "),
    FooterSegment::OrangeKey("Option"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("shift"),
    FooterSegment::Text("> + <"),
    FooterSegment::Key("↑↓←→"),
    FooterSegment::Text("> FOCUS"),
    FooterSegment::Text("   drag border RESIZE"),
];
const PANE_FOOTER: &[FooterSegment] = &[
    FooterSegment::Text("  <"),
    FooterSegment::Key("←↓↑→"),
    FooterSegment::Text("> FOCUS   <"),
    FooterSegment::Key("e"),
    FooterSegment::Text("> RENAME   <"),
    FooterSegment::Key("n"),
    FooterSegment::Text("> NEW   <"),
    FooterSegment::Key("r/l/d/u"),
    FooterSegment::Text("> SPLIT   <"),
    FooterSegment::Key("x"),
    FooterSegment::Text("> CLOSE   <"),
    FooterSegment::Key("k"),
    FooterSegment::Text("> LOCK   <"),
    FooterSegment::Key("i"),
    FooterSegment::Text("> INVITE   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> BACK"),
];
const TAB_FOOTER: &[FooterSegment] = &[
    FooterSegment::Text("  <"),
    FooterSegment::Key("←→"),
    FooterSegment::Text("> SWITCH   <"),
    FooterSegment::Key("e"),
    FooterSegment::Text("> RENAME   <"),
    FooterSegment::Key("n"),
    FooterSegment::Text("> NEW   <"),
    FooterSegment::Key("x"),
    FooterSegment::Text("> CLOSE   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> BACK"),
];
const AGENT_OVERLAY_HELP: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("↑↓"),
    FooterSegment::Text("> MOVE   <"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> FOCUS"),
];

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
    scrollback: usize,
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
    /// A runnable join command requested by a completed footer click.
    pub join_copy_command: Option<String>,
    /// An xterm mouse report the focused pane's child asked to receive.
    pub forward_bytes: Option<Vec<u8>>,
}

/// Footer content that determines the visible join command hit target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FooterMouseInput<'a> {
    pub status: &'a str,
    pub footer_notice: Option<&'a str>,
    pub copied_lines: Option<usize>,
    pub join_code: Option<&'a str>,
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
enum RenameTarget {
    Pane(PaneId),
    Tab(TabId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenamePrompt {
    target: RenameTarget,
    value: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum ModalState {
    #[default]
    None,
    Agents,
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
struct ScreenCell {
    row: u16,
    col: u16,
}

/// A local, pane-scoped text selection. Both ends are inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaneTextSelection {
    pane_id: PaneId,
    anchor: ScreenCell,
    cursor: ScreenCell,
}

#[derive(Clone, Copy, Debug)]
struct ResizeDrag {
    pane_id: PaneId,
    base_revision: u64,
    origin_column: u16,
    origin_row: u16,
    axis: Option<Axis>,
    horizontal: bool,
    vertical: bool,
    original_share_bps: u16,
    preview_first_share_bps: Option<u16>,
    span: u16,
    content: Rect,
}

#[derive(Clone, Copy, Debug)]
struct SplitTarget {
    first_share_bps: u16,
    span: u16,
}

#[derive(Clone, Copy, Debug)]
struct ResizePreview {
    pane_id: PaneId,
    axis: Axis,
    first_share_bps: u16,
}

impl PaneTextSelection {
    fn is_empty(self) -> bool {
        self.anchor == self.cursor
    }

    fn contains(self, cell: ScreenCell) -> bool {
        let (start, end) = self.bounds();
        match cell.row {
            row if row == start.row && row == end.row => (start.col..=end.col).contains(&cell.col),
            row if row == start.row => cell.col >= start.col,
            row if row == end.row => cell.col <= end.col,
            row => (start.row..end.row).contains(&row),
        }
    }

    fn bounds(self) -> (ScreenCell, ScreenCell) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}

/// Pure local rendering and selection state for a revisioned shared layout.
#[derive(Clone, Debug)]
pub struct MultiPaneTui {
    title: String,
    theme: UiTheme,
    snapshot: LayoutSnapshot,
    current_tab: TabId,
    focused_pane: PaneId,
    hovered_pane: Option<PaneId>,
    chord_mode: ChordMode,
    chord_last_activity: Option<Instant>,
    pane_views: BTreeMap<PaneId, PaneViewState>,
    pending_created_tab: Option<TabId>,
    pending_created_pane: Option<PaneId>,
    selection: Option<PaneTextSelection>,
    selection_dragging: bool,
    agent_rows: Vec<AgentOverlayRow>,
    prior_agent_states: BTreeMap<PaneId, AgentRosterState>,
    /// Start of the working interval last seen for a pane. An idle row carries the `0`
    /// sentinel, so the episode a completion refers to has to be remembered while it runs.
    prior_agent_episodes: BTreeMap<PaneId, u64>,
    /// Working interval a pane was last announced for. Keyed separately from
    /// `unread_agent_panes` because that set is cleared by focusing the pane.
    notified_agent_episodes: BTreeMap<PaneId, u64>,
    unread_agent_panes: BTreeSet<PaneId>,
    modal: ModalState,
    agent_selected_pane: Option<PaneId>,
    /// Terminal-line offset into the cards (two card lines plus one spacer each).
    agent_overlay_scroll_line: usize,
    agent_overlay_viewport_lines: u16,
    pending_agent_toggle: Option<Instant>,
    resize_drag: Option<ResizeDrag>,
    pending_join_click: bool,
    /// Set while a press forwarded to a child owns the drag and release that follow.
    mouse_forwarding: bool,
    /// A pane-mode invite request. The ticket lives in the node's rendezvous record rather
    /// than in the layout, so the attached client resolves and copies it.
    pending_ticket_copy: bool,
    /// Whether the coordinator is refusing new peers. Mirrored from the node rather than
    /// derived, because only the coordinator knows it and any peer may be drawing it.
    session_locked: bool,
}

impl MultiPaneTui {
    pub fn new(snapshot: LayoutSnapshot) -> Result<Self, LayoutError> {
        Self::with_theme(snapshot, UiTheme::default())
    }

    pub fn with_theme(snapshot: LayoutSnapshot, theme: UiTheme) -> Result<Self, LayoutError> {
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let current_tab = snapshot.tabs[0].tab_id;
        let focused_pane = first_leaf(&snapshot.tabs[0].root).expect("validated layout has a leaf");
        let pane_views = snapshot
            .panes
            .keys()
            .map(|pane_id| (*pane_id, PaneViewState::default()))
            .collect();
        Ok(Self {
            title: TOP_BAR_BRAND.into(),
            theme,
            snapshot,
            current_tab,
            focused_pane,
            hovered_pane: None,
            chord_mode: ChordMode::None,
            chord_last_activity: None,
            pane_views,
            pending_created_tab: None,
            pending_created_pane: None,
            selection: None,
            selection_dragging: false,
            agent_rows: Vec::new(),
            prior_agent_states: BTreeMap::new(),
            prior_agent_episodes: BTreeMap::new(),
            notified_agent_episodes: BTreeMap::new(),
            unread_agent_panes: BTreeSet::new(),
            modal: ModalState::None,
            agent_selected_pane: None,
            agent_overlay_scroll_line: 0,
            agent_overlay_viewport_lines: 0,
            pending_agent_toggle: None,
            resize_drag: None,
            pending_join_click: false,
            session_locked: false,
            mouse_forwarding: false,
            pending_ticket_copy: false,
        })
    }

    pub fn snapshot(&self) -> &LayoutSnapshot {
        &self.snapshot
    }

    /// Local clients may decorate the shared chrome without changing layout state.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn current_tab(&self) -> TabId {
        self.current_tab
    }

    pub fn focused_pane(&self) -> PaneId {
        self.focused_pane
    }

    pub fn chord_mode(&self) -> ChordMode {
        self.chord_mode
    }

    pub fn overlay_open(&self) -> bool {
        matches!(self.modal, ModalState::Agents)
    }

    /// Whether a blocking dialog is open, excluding the interactive agents overlay.
    pub fn modal_open(&self) -> bool {
        matches!(
            self.modal,
            ModalState::Rename(_) | ModalState::ConfirmDeleteTab { .. }
        )
    }

    pub fn set_agent_rows(&mut self, mut rows: Vec<AgentOverlayRow>) -> bool {
        rows.retain_mut(|row| {
            let Some((tab_ordinal, pane_ordinal)) = self.pane_location(row.pane_id) else {
                return false;
            };
            row.tab_ordinal = tab_ordinal;
            row.pane_ordinal = pane_ordinal;
            row.tab_label = self
                .snapshot
                .tabs
                .iter()
                .find(|tab| contains_leaf(&tab.root, row.pane_id))
                .and_then(|tab| tab.title.clone())
                .unwrap_or_else(|| format!("Tab #{tab_ordinal}"));
            row.pane_label = self
                .snapshot
                .panes
                .get(&row.pane_id)
                .and_then(|pane| pane.title.clone())
                .unwrap_or_else(|| format!("Pane #{pane_ordinal}"));
            true
        });
        rows.sort_by_key(|row| (row.tab_ordinal, row.pane_ordinal, row.pane_id));
        if self.agent_rows == rows {
            return false;
        }
        self.agent_rows = rows;
        if self
            .agent_selected_pane
            .is_some_and(|pane_id| !self.agent_rows.iter().any(|row| row.pane_id == pane_id))
        {
            self.agent_selected_pane = self.agent_rows.first().map(|row| row.pane_id);
        }
        if self.agent_selected_pane.is_none() {
            self.agent_selected_pane = self.agent_rows.first().map(|row| row.pane_id);
        }
        self.clamp_agent_overlay_scroll();
        self.ensure_agent_selection_visible();
        true
    }

    /// Updates attached-client agent rows and reports newly unread completions.
    ///
    /// The first observed roster only establishes the local baseline. Roster rows that disappear
    /// intentionally retain their previous state and unread marker until their pane is deleted.
    /// Apply the latest roster and return the panes whose agent just finished a work episode
    /// the user has not been told about. Announcing is keyed on the work episode rather than on
    /// the unread marker: focusing a pane clears the marker, and that must not re-arm the
    /// completion sound for work the user has already been notified about.
    pub fn update_attached_agent_rows(&mut self, rows: Vec<AgentOverlayRow>) -> Vec<PaneId> {
        self.set_agent_rows(rows);
        let mut newly_unread = Vec::new();
        for row in &self.agent_rows {
            if row.state == AgentRosterState::Working {
                self.prior_agent_episodes
                    .insert(row.pane_id, row.working_since_unix_ms);
            }
            let previous = self.prior_agent_states.insert(row.pane_id, row.state);
            if previous != Some(AgentRosterState::Working)
                || row.state != AgentRosterState::Idle
                || row.pane_id == self.focused_pane
            {
                continue;
            }
            let episode = self
                .prior_agent_episodes
                .get(&row.pane_id)
                .copied()
                .unwrap_or_default();
            if self.notified_agent_episodes.get(&row.pane_id) == Some(&episode) {
                continue;
            }
            self.notified_agent_episodes.insert(row.pane_id, episode);
            self.unread_agent_panes.insert(row.pane_id);
            newly_unread.push(row.pane_id);
        }
        newly_unread
    }

    pub(crate) fn set_agent_overlay_viewport(&mut self, area: Rect) {
        self.agent_overlay_viewport_lines = agents_overlay_content(area).height;
        self.clamp_agent_overlay_scroll();
    }

    fn agent_overlay_total_lines(&self) -> usize {
        self.agent_rows
            .len()
            .saturating_mul(AGENT_OVERLAY_CARD_LINES)
            .saturating_sub(1)
    }

    fn agent_overlay_max_scroll(&self) -> usize {
        self.agent_overlay_total_lines()
            .saturating_sub(usize::from(self.agent_overlay_viewport_lines.max(1)))
    }

    fn clamp_agent_overlay_scroll(&mut self) {
        if self.agent_overlay_viewport_lines == 0 {
            self.agent_overlay_scroll_line = 0;
            return;
        }
        self.agent_overlay_scroll_line = self
            .agent_overlay_scroll_line
            .min(self.agent_overlay_max_scroll());
    }

    fn ensure_agent_selection_visible(&mut self) {
        if self.agent_overlay_viewport_lines == 0 {
            return;
        }
        let Some(index) = self
            .agent_selected_pane
            .and_then(|pane| self.agent_rows.iter().position(|row| row.pane_id == pane))
        else {
            return;
        };
        let viewport_lines = usize::from(self.agent_overlay_viewport_lines.max(1));
        let card_start = index.saturating_mul(AGENT_OVERLAY_CARD_LINES);
        let card_end = card_start.saturating_add(1);
        if card_start < self.agent_overlay_scroll_line {
            self.agent_overlay_scroll_line = card_start;
        } else if card_end
            >= self
                .agent_overlay_scroll_line
                .saturating_add(viewport_lines)
        {
            self.agent_overlay_scroll_line =
                card_end.saturating_add(1).saturating_sub(viewport_lines);
        }
        self.clamp_agent_overlay_scroll();
    }

    pub(crate) fn scroll_agent_overlay(&mut self, area: Rect, up: bool) -> bool {
        self.set_agent_overlay_viewport(area);
        let previous = self.agent_overlay_scroll_line;
        if up {
            self.agent_overlay_scroll_line = self
                .agent_overlay_scroll_line
                .saturating_sub(AGENT_OVERLAY_CARD_LINES);
        } else {
            self.agent_overlay_scroll_line = self
                .agent_overlay_scroll_line
                .saturating_add(AGENT_OVERLAY_CARD_LINES);
        }
        self.clamp_agent_overlay_scroll();
        self.agent_overlay_scroll_line != previous
    }

    pub(crate) fn agent_overlay_has_working_rows(&self) -> bool {
        self.overlay_open()
            && self
                .agent_rows
                .iter()
                .any(|row| row.state == AgentRosterState::Working)
    }

    fn pane_location(&self, pane_id: PaneId) -> Option<(usize, usize)> {
        self.snapshot
            .tabs
            .iter()
            .enumerate()
            .find_map(|(tab_index, tab)| {
                visible_leaf_panes(&tab.root)
                    .iter()
                    .position(|id| *id == pane_id)
                    .map(|pane_index| (tab_index + 1, pane_index + 1))
            })
    }

    pub(crate) fn expire_agent_toggle(&mut self, now: Instant) -> bool {
        if self
            .pending_agent_toggle
            .is_some_and(|then| now.duration_since(then) >= AGENT_TOGGLE_WINDOW)
        {
            self.pending_agent_toggle = None;
        }
        false
    }

    fn exit_chord_mode(&mut self) {
        self.chord_mode = ChordMode::None;
        self.chord_last_activity = None;
    }

    fn enter_chord_mode(&mut self, mode: ChordMode) {
        self.chord_mode = mode;
        self.chord_last_activity = Some(Instant::now());
    }

    fn touch_chord_activity(&mut self) {
        if self.chord_mode != ChordMode::None {
            self.chord_last_activity = Some(Instant::now());
        }
    }

    /// Clears sticky pane/tab mode after [`CHORD_IDLE_TIMEOUT`] without a key.
    pub(crate) fn expire_chord_mode(&mut self, now: Instant) -> bool {
        let Some(last) = self.chord_last_activity else {
            return false;
        };
        if self.chord_mode == ChordMode::None
            || now.saturating_duration_since(last) < CHORD_IDLE_TIMEOUT
        {
            return false;
        }
        self.exit_chord_mode();
        true
    }

    fn clear_selection(&mut self) -> bool {
        let changed = self.selection.take().is_some();
        self.selection_dragging = false;
        changed
    }

    fn begin_selection_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        self.selection = Some(PaneTextSelection {
            pane_id,
            anchor: cell,
            cursor: cell,
        });
        self.selection_dragging = true;
        true
    }

    fn extend_selection_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        if !self.selection_dragging {
            return false;
        }
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        if selection.pane_id != pane_id || selection.cursor == cell {
            return false;
        }
        selection.cursor = cell;
        true
    }

    fn end_selection_drag(&mut self) -> bool {
        std::mem::replace(&mut self.selection_dragging, false)
    }

    fn begin_resize_drag(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let geometry = self.geometry(area);
        let Some((pane_id, horizontal, vertical)) = resize_border_hit(&geometry.panes, column, row)
        else {
            return false;
        };
        self.resize_drag = Some(ResizeDrag {
            pane_id,
            base_revision: self.snapshot.revision,
            origin_column: column,
            origin_row: row,
            axis: None,
            horizontal,
            vertical,
            original_share_bps: 0,
            preview_first_share_bps: None,
            span: 1,
            content: geometry.content,
        });
        if horizontal != vertical {
            self.lock_resize_drag(if horizontal {
                Axis::LeftRight
            } else {
                Axis::TopBottom
            });
        }
        true
    }

    fn extend_resize_drag(&mut self, column: u16, row: u16) -> bool {
        let Some(drag) = self.resize_drag else {
            return false;
        };
        if drag.axis.is_none() {
            let horizontal = i32::from(column) - i32::from(drag.origin_column);
            let vertical = i32::from(row) - i32::from(drag.origin_row);
            if horizontal.unsigned_abs().max(vertical.unsigned_abs()) < 2 {
                return true;
            }
            let axis = match (drag.horizontal, drag.vertical) {
                (true, false) => Axis::LeftRight,
                (false, true) => Axis::TopBottom,
                (true, true) if horizontal.unsigned_abs() >= vertical.unsigned_abs() => {
                    Axis::LeftRight
                }
                (true, true) => Axis::TopBottom,
                (false, false) => {
                    self.resize_drag = None;
                    return true;
                }
            };
            self.lock_resize_drag(axis);
        }
        if let Some(drag) = self.resize_drag.as_mut()
            && drag.axis.is_some()
        {
            drag.preview_first_share_bps = Some(resize_proposed_share(*drag, column, row));
        }
        true
    }

    fn lock_resize_drag(&mut self, axis: Axis) {
        let Some(drag) = self.resize_drag else {
            return;
        };
        let Some(tab) = self.current_tab_layout() else {
            self.resize_drag = None;
            return;
        };
        let Some(target) = nearest_split_for_pane(&tab.root, drag.pane_id, axis, drag.content)
        else {
            self.resize_drag = None;
            return;
        };
        let drag = self.resize_drag.as_mut().expect("drag remains active");
        drag.axis = Some(axis);
        drag.original_share_bps = target.first_share_bps;
        drag.span = target.span;
    }

    fn end_resize_drag(&mut self, column: u16, row: u16) -> Option<UiIntent> {
        let drag = self.resize_drag.take()?;
        let axis = drag.axis?;
        let proposed = resize_proposed_share(drag, column, row);
        (proposed != drag.original_share_bps).then_some(UiIntent::SetSplitRatio {
            pane_id: drag.pane_id,
            axis,
            first_share_bps: proposed,
            base_revision: drag.base_revision,
        })
    }

    fn cancel_resize_drag(&mut self) -> bool {
        self.resize_drag.take().is_some()
    }

    fn selection(&self) -> Option<PaneTextSelection> {
        self.selection.filter(|selection| !selection.is_empty())
    }

    pub(crate) fn selection_pane(&self) -> Option<PaneId> {
        self.selection().map(|selection| selection.pane_id)
    }

    pub(crate) fn selected_text(&self, screen: &vt100::Screen) -> Option<String> {
        self.selection()
            .and_then(|selection| selection_text(screen, selection))
    }

    fn screen_cell_at(&self, column: u16, row: u16, area: Rect) -> Option<(PaneId, ScreenCell)> {
        let geometry = self.geometry(area);
        let (pane_id, pane_rect) = geometry.panes.iter().find_map(|(pane_id, rect)| {
            rect_contains(*rect, column, row).then_some((*pane_id, *rect))
        })?;
        let pane = self.snapshot.panes.get(&pane_id)?;
        let viewport =
            fixed_grid_viewport(pane_content_rect(pane_rect), pane.grid_rows, pane.grid_cols);
        mouse_to_screen_cell(viewport, column, row).map(|cell| (pane_id, cell))
    }

    pub fn pane_view(&self, pane_id: PaneId) -> Option<&PaneViewState> {
        self.pane_views.get(&pane_id)
    }

    fn scrollback_offset(&self, pane_id: PaneId) -> usize {
        self.pane_views
            .get(&pane_id)
            .map_or(0, |view| view.scrollback)
    }

    pub(crate) fn pane_scrollback_offset(&self, pane_id: PaneId) -> usize {
        self.scrollback_offset(pane_id)
    }

    pub(crate) fn set_pane_scrollback_offset(&mut self, pane_id: PaneId, offset: usize) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == offset {
            return false;
        }
        view.scrollback = offset;
        true
    }

    fn pane_at_or_focused(&self, column: u16, row: u16, area: Rect) -> PaneId {
        pane_at(&self.geometry(area).panes, column, row).unwrap_or(self.focused_pane)
    }

    pub fn pane_at_or_focused_for_mouse(&self, column: u16, row: u16, area: Rect) -> PaneId {
        self.pane_at_or_focused(column, row, area)
    }

    pub fn scroll_mouse_pane(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
        scrollback_len: usize,
        up: bool,
    ) -> bool {
        if self.modal_open() {
            return false;
        }
        let pane_id = self.pane_at_or_focused(column, row, area);
        self.scroll_pane(pane_id, scrollback_len, up)
    }

    fn scroll_pane(&mut self, pane_id: PaneId, scrollback_len: usize, up: bool) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        let scrollback = if up {
            view.scrollback
                .saturating_add(PANE_SCROLL_WHEEL_STEP)
                .min(scrollback_len)
        } else {
            view.scrollback.saturating_sub(PANE_SCROLL_WHEEL_STEP)
        };
        if view.scrollback == scrollback {
            return false;
        }
        view.scrollback = scrollback;
        true
    }

    fn reset_scrollback(&mut self, pane_id: PaneId) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == 0 {
            return false;
        }
        view.scrollback = 0;
        true
    }

    /// Keeps a scrolled-back local viewport pinned while the host appends visual rows.
    pub fn pin_scrollback_after_output(
        &mut self,
        pane_id: PaneId,
        appended_rows: usize,
        scrollback_len: usize,
    ) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == 0 {
            return false;
        }
        let scrollback = view
            .scrollback
            .saturating_add(appended_rows)
            .min(scrollback_len);
        if view.scrollback == scrollback {
            return false;
        }
        view.scrollback = scrollback;
        true
    }

    pub fn set_pane_view(&mut self, pane_id: PaneId, mut state: PaneViewState) -> bool {
        if self.snapshot.panes.contains_key(&pane_id) {
            state.scrollback = self.scrollback_offset(pane_id);
            if self.pane_views.get(&pane_id) == Some(&state) {
                return false;
            }
            self.pane_views.insert(pane_id, state);
            return true;
        }
        false
    }

    /// Select a tab only when the coordinator publishes the exact ID reserved for this member.
    pub fn select_created_tab(&mut self, tab_id: TabId) {
        self.pending_created_tab = Some(tab_id);
        self.repair_selection();
    }

    /// Select a pane only once an authoritative snapshot includes the reservation's pane ID.
    pub fn select_created_pane(&mut self, pane_id: PaneId) {
        self.pending_created_pane = Some(pane_id);
        self.repair_selection();
    }

    pub fn apply_snapshot(&mut self, snapshot: LayoutSnapshot) -> Result<(), LayoutError> {
        if snapshot.revision < self.snapshot.revision {
            return Err(LayoutError::StaleRevision {
                expected: self.snapshot.revision,
                got: snapshot.revision,
            });
        }
        if snapshot.revision == self.snapshot.revision {
            return if snapshot == self.snapshot {
                Ok(())
            } else {
                Err(LayoutError::ConflictingSnapshotRevision {
                    revision: snapshot.revision,
                })
            };
        }
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let old_views = std::mem::take(&mut self.pane_views);
        self.pane_views = snapshot
            .panes
            .values()
            .map(|pane| {
                let state = old_views.get(&pane.pane_id).cloned().unwrap_or_default();
                (pane.pane_id, state)
            })
            .collect();
        self.snapshot = snapshot;
        self.prior_agent_states
            .retain(|pane_id, _| self.snapshot.panes.contains_key(pane_id));
        self.prior_agent_episodes
            .retain(|pane_id, _| self.snapshot.panes.contains_key(pane_id));
        self.notified_agent_episodes
            .retain(|pane_id, _| self.snapshot.panes.contains_key(pane_id));
        self.unread_agent_panes
            .retain(|pane_id| self.snapshot.panes.contains_key(pane_id));
        self.cancel_resize_drag();
        self.clear_selection();
        self.repair_selection();
        Ok(())
    }

    pub fn select_tab(&mut self, tab_id: TabId) -> Result<(), LayoutError> {
        let tab = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        let pane_id = first_leaf(&tab.root).expect("validated layout has a leaf");
        self.select_pane(tab_id, pane_id, "tab_switch");
        Ok(())
    }

    fn focus_pane_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some(pane_id) = pane_at(&self.geometry(area).panes, column, row) else {
            return false;
        };
        self.select_pane(self.current_tab, pane_id, "mouse")
    }

    fn hover_pane_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let hovered_pane = pane_at(&self.geometry(area).panes, column, row);
        if self.hovered_pane == hovered_pane {
            return false;
        }
        self.hovered_pane = hovered_pane;
        true
    }

    fn switch_tab_at(&mut self, column: u16, row: u16, area: Rect) -> Option<UiIntent> {
        let tab_id = self
            .geometry(area)
            .tab_labels
            .iter()
            .find_map(|(tab_id, rect)| rect_contains(*rect, column, row).then_some(*tab_id))?;
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(UiIntent::SwitchTab { tab_id })
    }

    pub fn geometry(&self, area: Rect) -> PaneGeometry {
        let tab_bar = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let footer_height = area.height.saturating_sub(tab_bar.height).min(1);
        let footer = Rect::new(
            area.x,
            area.y
                .saturating_add(area.height.saturating_sub(footer_height)),
            area.width,
            footer_height,
        );
        let tab_bar_gap = 0;
        let content = Rect::new(
            area.x,
            area.y.saturating_add(tab_bar.height + tab_bar_gap),
            area.width,
            area.height
                .saturating_sub(tab_bar.height + tab_bar_gap + footer_height),
        );
        let mut panes = BTreeMap::new();
        if let Some(tab) = self.current_tab_layout() {
            allocate_node_with_preview(
                &tab.root,
                content,
                &mut panes,
                self.resize_drag.and_then(|drag| {
                    drag.axis
                        .zip(drag.preview_first_share_bps)
                        .map(|(axis, first_share_bps)| ResizePreview {
                            pane_id: drag.pane_id,
                            axis,
                            first_share_bps,
                        })
                }),
            );
        }
        PaneGeometry {
            tab_bar,
            tab_labels: self.tab_label_rects(tab_bar),
            content,
            footer,
            panes,
        }
    }

    fn tab_label_rects(&self, tab_bar: Rect) -> BTreeMap<TabId, Rect> {
        let mut x = tab_bar
            .x
            .saturating_add(text_width(&self.title))
            .saturating_add(text_width(TOP_BAR_BRAND_SEPARATOR));
        let right = tab_bar.x.saturating_add(tab_bar.width);
        self.snapshot
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                if index > 0 {
                    x = x.saturating_add(text_width(TAB_BAR_SEPARATOR));
                }
                let label_width = text_width(&tab_label(
                    tab.title.as_deref(),
                    index + 1,
                    tab.tab_id == self.current_tab,
                    self.tab_has_unread_agent_pane(tab),
                ));
                let width = right.saturating_sub(x).min(label_width);
                let rect = Rect::new(x, tab_bar.y, width, tab_bar.height);
                x = x.saturating_add(label_width);
                (tab.tab_id, rect)
            })
            .collect()
    }

    fn tab_has_unread_agent_pane(&self, tab: &crate::layout::Tab) -> bool {
        self.unread_agent_panes
            .iter()
            .any(|pane_id| contains_leaf(&tab.root, *pane_id))
    }

    pub fn handle_key(&mut self, key: KeyEvent, area: Rect) -> KeyHandling {
        if is_quit(key) {
            self.modal = ModalState::None;
            self.pending_agent_toggle = None;
            self.exit_chord_mode();
            return KeyHandling::Quit;
        }
        if matches!(self.modal, ModalState::Rename(_)) {
            return self.handle_rename_key(key);
        }
        if matches!(self.modal, ModalState::ConfirmDeleteTab { .. }) {
            return self.handle_confirm_delete_tab_key(key);
        }
        if key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL {
            if self.overlay_open() {
                let forward = self
                    .pending_agent_toggle
                    .is_some_and(|then| then.elapsed() <= AGENT_TOGGLE_WINDOW);
                self.modal = ModalState::None;
                self.pending_agent_toggle = None;
                return if forward {
                    ui_debug_log(
                        "agents_toggle_forward",
                        format_args!("window_ms={}", AGENT_TOGGLE_WINDOW.as_millis()),
                    );
                    KeyHandling::Forward
                } else {
                    ui_debug_log("agents_overlay_close", format_args!("reason=ctrl_a"));
                    KeyHandling::Consumed(vec![])
                };
            }
            self.modal = ModalState::Agents;
            self.pending_agent_toggle = Some(Instant::now());
            self.exit_chord_mode();
            ui_debug_log(
                "agents_overlay_open",
                format_args!("window_ms={}", AGENT_TOGGLE_WINDOW.as_millis()),
            );
            return KeyHandling::Consumed(vec![]);
        }
        if self.overlay_open() {
            self.set_agent_overlay_viewport(area);
            return self.handle_agent_overlay_key(key);
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.chord_mode != ChordMode::None
        {
            self.exit_chord_mode();
            return KeyHandling::Consumed(vec![]);
        }
        if matches!(self.chord_mode, ChordMode::None | ChordMode::Pane) && is_option_arrow(key) {
            self.touch_chord_activity();
            return KeyHandling::Consumed(self.move_focus(key.code, area).into_iter().collect());
        }
        if self.chord_mode == ChordMode::None {
            if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
                self.enter_chord_mode(ChordMode::Pane);
                return KeyHandling::Consumed(vec![]);
            }
            if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
                self.enter_chord_mode(ChordMode::Tab);
                return KeyHandling::Consumed(vec![]);
            }
            return KeyHandling::Forward;
        }

        let chord = self.chord_mode;
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.enter_chord_mode(ChordMode::Pane);
            return KeyHandling::Consumed(vec![]);
        }
        if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            self.enter_chord_mode(ChordMode::Tab);
            return KeyHandling::Consumed(vec![]);
        }
        let intent = match chord {
            ChordMode::Pane => self.handle_pane_chord(key, area),
            ChordMode::Tab => self.handle_tab_chord(key, area),
            ChordMode::None => None,
        };
        if is_chord_command(chord, key) {
            if is_chord_navigation(chord, key) {
                self.touch_chord_activity();
            } else {
                self.exit_chord_mode();
            }
            KeyHandling::Consumed(intent.into_iter().collect())
        } else {
            self.exit_chord_mode();
            KeyHandling::Forward
        }
    }

    /// Applies the local half of mouse interaction and returns mutations for the node.
    pub fn handle_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        area: Rect,
        footer: FooterMouseInput<'_>,
        protocol: PaneMouseProtocol,
    ) -> MouseHandling {
        if self.modal_open() {
            self.pending_join_click = false;
            self.mouse_forwarding = false;
            return MouseHandling::default();
        }
        if let Some(handling) = self.report_mouse_to_child(mouse, area, protocol) {
            return handling;
        }
        match mouse.kind {
            MouseEventKind::Moved if !self.overlay_open() => {
                self.hover_pane_at(mouse.column, mouse.row, area);
                MouseHandling::default()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.overlay_open() {
                    return MouseHandling {
                        intents: self.handle_agent_overlay_click(mouse.column, mouse.row, area),
                        ..MouseHandling::default()
                    };
                }
                self.pending_join_click = self
                    .footer_join_rect(area, footer)
                    .is_some_and(|rect| rect_contains_mouse(rect, mouse.column, mouse.row));
                if self.pending_join_click {
                    return MouseHandling::default();
                }
                self.clear_selection();
                if self.begin_resize_drag(mouse.column, mouse.row, area) {
                    return MouseHandling::default();
                }
                if let Some(intent) = self.switch_tab_at(mouse.column, mouse.row, area) {
                    return MouseHandling {
                        intents: vec![intent],
                        ..MouseHandling::default()
                    };
                }
                let changed = self.focus_pane_at(mouse.column, mouse.row, area);
                self.begin_selection_at(mouse.column, mouse.row, area);
                MouseHandling {
                    intents: changed
                        .then_some(UiIntent::FocusPane {
                            pane_id: self.focused_pane,
                        })
                        .into_iter()
                        .collect(),
                    ..MouseHandling::default()
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.pending_join_click {
                    self.pending_join_click = false;
                    return MouseHandling::default();
                }
                if !self.extend_resize_drag(mouse.column, mouse.row) {
                    self.extend_selection_at(mouse.column, mouse.row, area);
                }
                MouseHandling::default()
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if std::mem::take(&mut self.pending_join_click) {
                    let join_copy_command = self
                        .footer_join_rect(area, footer)
                        .filter(|rect| rect_contains_mouse(*rect, mouse.column, mouse.row))
                        .zip(footer.join_code)
                        .map(|(_, code)| format!("p2pmux join {code}"));
                    return MouseHandling {
                        join_copy_command,
                        ..MouseHandling::default()
                    };
                }
                self.end_selection_drag();
                let resize_intent = self.end_resize_drag(mouse.column, mouse.row);
                MouseHandling {
                    copy_selection_requested: resize_intent.is_none() && self.selection().is_some(),
                    intents: resize_intent.into_iter().collect(),
                    ..MouseHandling::default()
                }
            }
            _ => MouseHandling::default(),
        }
    }

    /// Hands a mouse event to the focused pane's child when that child asked for
    /// mouse reports, so a click lands as a caret move instead of a mux selection.
    ///
    /// Returns `None` when the mux keeps the event for its own focus, selection,
    /// resize, tab, and footer handling.
    fn report_mouse_to_child(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        area: Rect,
        protocol: PaneMouseProtocol,
    ) -> Option<MouseHandling> {
        // Shift is the standing escape hatch: it always selects, never forwards.
        if self.overlay_open()
            || !protocol.reports_mouse()
            || mouse.modifiers.contains(KeyModifiers::SHIFT)
            || self.scrollback_offset(self.focused_pane) != 0
        {
            self.mouse_forwarding = false;
            return None;
        }
        let viewport = self.focused_pane_viewport(area)?;
        let cell = match mouse.kind {
            // A press outside the focused pane still belongs to the mux: it moves
            // focus, drags a border, or switches a tab.
            MouseEventKind::Down(_) => {
                let cell = mouse_to_screen_cell(viewport, mouse.column, mouse.row)?;
                self.clear_selection();
                self.mouse_forwarding = true;
                cell
            }
            // Once a press is forwarded the child owns the rest of the gesture, even
            // if the pointer leaves the pane mid-drag.
            MouseEventKind::Drag(_) => {
                if !self.mouse_forwarding {
                    return None;
                }
                clamp_to_viewport(viewport, mouse.column, mouse.row)?
            }
            MouseEventKind::Up(_) => {
                if !std::mem::take(&mut self.mouse_forwarding) {
                    return None;
                }
                clamp_to_viewport(viewport, mouse.column, mouse.row)?
            }
            MouseEventKind::Moved => {
                // Hover chrome still tracks the pointer while motion is forwarded.
                self.hover_pane_at(mouse.column, mouse.row, area);
                mouse_to_screen_cell(viewport, mouse.column, mouse.row)?
            }
            _ => mouse_to_screen_cell(viewport, mouse.column, mouse.row)?,
        };
        Some(MouseHandling {
            forward_bytes: encode_mouse(mouse.kind, mouse.modifiers, cell, protocol),
            ..MouseHandling::default()
        })
    }

    fn focused_pane_viewport(&self, area: Rect) -> Option<Rect> {
        let geometry = self.geometry(area);
        let pane_rect = geometry.panes.get(&self.focused_pane)?;
        let pane = self.snapshot.panes.get(&self.focused_pane)?;
        Some(fixed_grid_viewport(
            pane_content_rect(*pane_rect),
            pane.grid_rows,
            pane.grid_cols,
        ))
    }

    fn footer_join_rect(&self, area: Rect, footer: FooterMouseInput<'_>) -> Option<Rect> {
        footer_layout(
            self.geometry(area).footer,
            self.chord_mode,
            footer.status,
            footer.footer_notice,
            footer.copied_lines,
            footer.join_code,
        )
        .join_rect
    }

    /// Aligns a reattached client's local selection with the node-owned focus.
    pub fn set_focus(&mut self, tab_id: TabId, pane_id: PaneId) -> Result<(), LayoutError> {
        let tab = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        if !contains_leaf(&tab.root, pane_id) {
            return Err(LayoutError::UnknownPane { pane_id });
        }
        self.select_pane(tab_id, pane_id, "node_sync");
        Ok(())
    }

    fn handle_agent_overlay_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Esc => {
                self.modal = ModalState::None;
                self.pending_agent_toggle = None;
                ui_debug_log("agents_overlay_close", format_args!("reason=esc"));
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_agent_selection(false);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_agent_selection(true);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter => {
                let Some(pane_id) = self.agent_selected_pane else {
                    return KeyHandling::Consumed(vec![]);
                };
                KeyHandling::Consumed(self.jump_to_agent_pane(pane_id, "enter"))
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    fn open_rename(&mut self, target: RenameTarget) {
        let value = match target {
            RenameTarget::Pane(pane_id) => self
                .snapshot
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.title.clone()),
            RenameTarget::Tab(tab_id) => self
                .snapshot
                .tabs
                .iter()
                .find(|tab| tab.tab_id == tab_id)
                .and_then(|tab| tab.title.clone()),
        }
        .unwrap_or_default();
        self.modal = ModalState::Rename(RenamePrompt {
            target,
            value,
            error: None,
        });
        self.exit_chord_mode();
        self.clear_selection();
        self.cancel_resize_drag();
    }

    fn handle_rename_key(&mut self, key: KeyEvent) -> KeyHandling {
        let ModalState::Rename(prompt) = &mut self.modal else {
            unreachable!("rename handler requires an active rename prompt");
        };
        match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                prompt.value.pop();
                prompt.error = None;
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter if key.modifiers.is_empty() => match normalize_title(&prompt.value) {
                Ok(normalized) => {
                    let title = normalized.unwrap_or_default();
                    let intent = match prompt.target {
                        RenameTarget::Pane(pane_id) => UiIntent::RenamePane { pane_id, title },
                        RenameTarget::Tab(tab_id) => UiIntent::RenameTab { tab_id, title },
                    };
                    self.modal = ModalState::None;
                    KeyHandling::Consumed(vec![intent])
                }
                Err(_) => {
                    prompt.error = Some(String::from("Max 32 characters; no controls"));
                    KeyHandling::Consumed(vec![])
                }
            },
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                prompt.value.push(character);
                prompt.error = None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    fn confirm_delete_tab(&mut self, tab_id: TabId, pane_count: usize) {
        self.modal = ModalState::ConfirmDeleteTab { tab_id, pane_count };
        self.exit_chord_mode();
        self.clear_selection();
        self.cancel_resize_drag();
    }

    fn handle_confirm_delete_tab_key(&mut self, key: KeyEvent) -> KeyHandling {
        let tab_id = match &self.modal {
            ModalState::ConfirmDeleteTab { tab_id, .. } => *tab_id,
            _ => unreachable!("delete confirmation handler requires an active confirmation"),
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id }])
            }
            KeyCode::Esc | KeyCode::Char('n') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    fn handle_agent_overlay_click(&mut self, column: u16, row: u16, area: Rect) -> Vec<UiIntent> {
        let Some(pane_id) = self.agent_overlay_row_at(column, row, area) else {
            return Vec::new();
        };
        self.agent_selected_pane = Some(pane_id);
        self.jump_to_agent_pane(pane_id, "mouse")
    }

    fn agent_overlay_row_at(&self, column: u16, row: u16, area: Rect) -> Option<PaneId> {
        let content = agents_overlay_content(area);
        if !rect_contains(content, column, row) {
            return None;
        }
        let line = usize::from(row.saturating_sub(content.y))
            .saturating_add(self.agent_overlay_scroll_line);
        if line % AGENT_OVERLAY_CARD_LINES == 2 {
            return None;
        }
        self.agent_rows
            .get(line / AGENT_OVERLAY_CARD_LINES)
            .map(|agent| agent.pane_id)
    }

    fn jump_to_agent_pane(&mut self, pane_id: PaneId, close_reason: &str) -> Vec<UiIntent> {
        let Some(tab) = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| contains_leaf(&tab.root, pane_id))
        else {
            return Vec::new();
        };
        self.select_pane(tab.tab_id, pane_id, "overlay_jump");
        self.modal = ModalState::None;
        self.pending_agent_toggle = None;
        ui_debug_log(
            "agents_overlay_close",
            format_args!("reason={close_reason} pane_id={pane_id}"),
        );
        vec![UiIntent::FocusPane { pane_id }]
    }

    fn move_agent_selection(&mut self, forward: bool) {
        if self.agent_rows.is_empty() {
            self.agent_selected_pane = None;
            return;
        }
        let current = self
            .agent_selected_pane
            .and_then(|pane| self.agent_rows.iter().position(|row| row.pane_id == pane))
            .unwrap_or(0);
        let len = self.agent_rows.len();
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.agent_selected_pane = Some(self.agent_rows[next].pane_id);
        self.ensure_agent_selection_visible();
    }

    fn handle_pane_chord(&mut self, key: KeyEvent, area: Rect) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_rename(RenameTarget::Pane(self.focused_pane));
                None
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
                self.create_pane(
                    if rect.width > rect.height {
                        Axis::LeftRight
                    } else {
                        Axis::TopBottom
                    },
                    NewPanePosition::Second,
                    area,
                )
            }
            KeyCode::Char('r') if key.modifiers.is_empty() => {
                self.create_pane(Axis::LeftRight, NewPanePosition::Second, area)
            }
            KeyCode::Char('l') if key.modifiers.is_empty() => {
                self.create_pane(Axis::LeftRight, NewPanePosition::First, area)
            }
            KeyCode::Char('d') if key.modifiers.is_empty() => {
                self.create_pane(Axis::TopBottom, NewPanePosition::Second, area)
            }
            KeyCode::Char('u') if key.modifiers.is_empty() => {
                self.create_pane(Axis::TopBottom, NewPanePosition::First, area)
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => Some(UiIntent::DeletePane {
                pane_id: self.focused_pane,
            }),
            KeyCode::Char('i') if key.modifiers.is_empty() => {
                self.pending_ticket_copy = true;
                None
            }
            KeyCode::Char('k') if key.modifiers.is_empty() => self
                .snapshot
                .panes
                .get(&self.focused_pane)
                .map(|pane| UiIntent::SetPaneLock {
                    pane_id: self.focused_pane,
                    locked: !pane.locked,
                }),
            // Shift+L, deliberately a different key from the lowercase `k` pane lock:
            // locking one pane and locking the front door are very different acts to
            // perform by accident.
            KeyCode::Char('L') => Some(UiIntent::SetSessionLock {
                locked: !self.session_locked,
            }),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.is_empty() =>
            {
                self.move_focus(key.code, area)
            }
            _ => None,
        }
    }

    /// Mirror the coordinator's current session lock, for display and for the toggle.
    pub fn set_session_locked(&mut self, locked: bool) {
        self.session_locked = locked;
    }

    pub fn session_locked(&self) -> bool {
        self.session_locked
    }

    /// Claim a pending invite request, if the last key asked for one.
    pub fn take_ticket_copy_request(&mut self) -> bool {
        std::mem::take(&mut self.pending_ticket_copy)
    }

    fn create_pane(&self, axis: Axis, position: NewPanePosition, area: Rect) -> Option<UiIntent> {
        let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
        let (grid_rows, grid_cols) = grid_for_pane(rect);
        Some(UiIntent::CreatePane {
            target_pane_id: self.focused_pane,
            axis,
            position,
            grid_rows,
            grid_cols,
        })
    }

    fn handle_tab_chord(&mut self, key: KeyEvent, area: Rect) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_rename(RenameTarget::Tab(self.current_tab));
                None
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let (grid_rows, grid_cols) = grid_for_pane(self.geometry(area).content);
                Some(UiIntent::CreateTab {
                    grid_rows,
                    grid_cols,
                })
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => {
                let tab = self
                    .snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == self.current_tab)?;
                let pane_count = visible_leaf_panes(&tab.root).len();
                if pane_count > 1 {
                    self.confirm_delete_tab(tab.tab_id, pane_count);
                    None
                } else {
                    Some(UiIntent::DeleteTab { tab_id: tab.tab_id })
                }
            }
            KeyCode::Left if key.modifiers.is_empty() => self.switch_tab(false),
            KeyCode::Right if key.modifiers.is_empty() => self.switch_tab(true),
            _ => None,
        }
    }

    fn move_focus(&mut self, direction: KeyCode, area: Rect) -> Option<UiIntent> {
        let geometry = self.geometry(area);
        let source = *geometry.panes.get(&self.focused_pane)?;
        let source_center = rect_center(source);
        let candidates = geometry
            .panes
            .iter()
            .filter(|(pane_id, _)| **pane_id != self.focused_pane)
            .map(|(pane_id, rect)| (*pane_id, *rect))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let pane_id = candidates
            .iter()
            .copied()
            .filter(|(_, rect)| is_in_direction(source_center, rect_center(*rect), direction))
            .min_by_key(|(pane_id, rect)| {
                direction_distance(source_center, rect_center(*rect), direction, *pane_id)
            })?
            .0;
        self.select_pane(self.current_tab, pane_id, "key");
        Some(UiIntent::FocusPane { pane_id })
    }

    fn switch_tab(&mut self, forward: bool) -> Option<UiIntent> {
        let index = self
            .snapshot
            .tabs
            .iter()
            .position(|tab| tab.tab_id == self.current_tab)?;
        let len = self.snapshot.tabs.len();
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        let tab_id = self.snapshot.tabs[next].tab_id;
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(UiIntent::SwitchTab { tab_id })
    }

    fn current_tab_layout(&self) -> Option<&crate::layout::Tab> {
        self.snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == self.current_tab)
    }

    fn repair_selection(&mut self) {
        if let Some(tab_id) = self.pending_created_tab
            && let Some(tab) = self.snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id)
        {
            let pane_id = first_leaf(&tab.root).expect("validated layout has a leaf");
            self.pending_created_tab = None;
            self.select_pane(tab_id, pane_id, "snapshot_repair");
            return;
        }
        let current_tab = if let Some(tab) = self.current_tab_layout() {
            tab
        } else {
            &self.snapshot.tabs[0]
        };
        let (tab_id, pane_id) = {
            let root = &current_tab.root;
            let pane_id = self
                .pending_created_pane
                .filter(|pane_id| contains_leaf(root, *pane_id))
                .or_else(|| contains_leaf(root, self.focused_pane).then_some(self.focused_pane))
                .unwrap_or_else(|| first_leaf(root).expect("validated layout has a leaf"));
            (current_tab.tab_id, pane_id)
        };
        if self.pending_created_pane == Some(pane_id) {
            self.pending_created_pane = None;
        }
        self.select_pane(tab_id, pane_id, "snapshot_repair");
    }

    fn select_pane(&mut self, tab_id: TabId, pane_id: PaneId, reason: &str) -> bool {
        let old_tab = self.current_tab;
        let old_pane = self.focused_pane;
        self.current_tab = tab_id;
        self.focused_pane = pane_id;
        let unread_cleared = self.unread_agent_panes.remove(&pane_id);
        self.log_selection_change(reason, old_tab, old_pane, tab_id, pane_id);
        old_tab != tab_id || old_pane != pane_id || unread_cleared
    }

    fn log_selection_change(
        &self,
        reason: &str,
        old_tab: TabId,
        old_pane: PaneId,
        new_tab: TabId,
        new_pane: PaneId,
    ) {
        if old_tab != new_tab || old_pane != new_pane {
            ui_debug_log(
                "selection_change",
                format_args!(
                    "reason={reason} old_tab={old_tab} old_pane={old_pane} new_tab={new_tab} new_pane={new_pane}"
                ),
            );
        }
    }
}

fn is_option_arrow(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
        )
}

#[derive(Default)]
struct PendingEscape {
    started: Option<Instant>,
}

impl PendingEscape {
    fn start(&mut self, now: Instant) {
        self.started = Some(now);
    }

    fn take_if_expired(&mut self, now: Instant) -> bool {
        if !self
            .started
            .is_some_and(|started| now.saturating_duration_since(started) >= ESC_PREFIX_WINDOW)
        {
            return false;
        }
        self.started = None;
        true
    }

    fn take_option_arrow(&mut self, key: KeyEvent) -> Option<KeyEvent> {
        (self.started.is_some() && key.modifiers.is_empty())
            .then_some(key.code)
            .filter(|code| {
                matches!(
                    code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                )
            })
            .map(|code| {
                self.started = None;
                KeyEvent::new(code, KeyModifiers::ALT)
            })
    }

    fn take(&mut self) -> bool {
        self.started.take().is_some()
    }
}

/// Upper bound on terminal events applied per loop cycle so one burst cannot
/// starve PTY draining forever.
const MAX_EVENTS_PER_CYCLE: usize = 256;

/// Frames are presented at most once per interval (~60 fps) so parse and input
/// work is never crowded out by redraws faster than anyone can perceive.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

fn frame_due(last_draw: Option<Instant>) -> bool {
    last_draw.is_none_or(|drawn| drawn.elapsed() >= FRAME_INTERVAL)
}

/// Asks the outer terminal to present the writes up to the matching end marker
/// atomically (DEC 2026). Terminals without support ignore both markers.
fn begin_synchronized_output() -> io::Result<()> {
    io::stdout().write_all(b"\x1b[?2026h")
}

fn end_synchronized_output() -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1b[?2026l")?;
    stdout.flush()
}

/// Poll timeout for the run loops: until the next frame deadline while a dirty
/// frame is waiting, one full interval when idle.
fn event_poll_timeout(dirty: bool, last_draw: Option<Instant>) -> Duration {
    if !dirty {
        return FRAME_INTERVAL;
    }
    last_draw
        .map(|drawn| FRAME_INTERVAL.saturating_sub(drawn.elapsed()))
        .unwrap_or(Duration::ZERO)
        .max(Duration::from_millis(1))
}

fn is_mouse_moved(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Moved)
    )
}

/// Reads every queued terminal event without blocking. Hover only depends on the
/// final pointer position, so all mouse-moves but the last are dropped; a burst
/// of moves then costs one update instead of one redraw cycle each.
fn collect_pending_events(max: usize) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    while events.len() < max && event::poll(Duration::ZERO)? {
        events.push(event::read()?);
    }
    if let Some(last_moved) = events.iter().rposition(is_mouse_moved) {
        let mut index = 0;
        events.retain(|event| {
            let keep = index == last_moved || !is_mouse_moved(event);
            index += 1;
            keep
        });
    }
    Ok(events)
}

fn is_chord_command(mode: ChordMode, key: KeyEvent) -> bool {
    // An uppercase chord letter arrives with SHIFT held, which is not a modifier the user
    // added on purpose -- it is how the letter is typed. Rejecting it outright would make
    // any uppercase chord permanently unreachable.
    let uppercase = matches!(key.code, KeyCode::Char(letter) if letter.is_ascii_uppercase());
    let permitted = if uppercase {
        (key.modifiers - KeyModifiers::SHIFT).is_empty()
    } else {
        key.modifiers.is_empty()
    };
    if !permitted {
        return false;
    }
    match mode {
        ChordMode::Pane => matches!(
            key.code,
            KeyCode::Char('e')
                | KeyCode::Char('n')
                | KeyCode::Char('r')
                | KeyCode::Char('l')
                | KeyCode::Char('d')
                | KeyCode::Char('u')
                | KeyCode::Char('x')
                | KeyCode::Char('k')
                | KeyCode::Char('L')
                | KeyCode::Char('i')
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
        ),
        ChordMode::Tab => matches!(
            key.code,
            KeyCode::Char('e')
                | KeyCode::Char('n')
                | KeyCode::Char('x')
                | KeyCode::Left
                | KeyCode::Right
        ),
        ChordMode::None => false,
    }
}

fn is_chord_navigation(mode: ChordMode, key: KeyEvent) -> bool {
    if !key.modifiers.is_empty() {
        return false;
    }
    match mode {
        ChordMode::Pane => matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
        ),
        ChordMode::Tab => matches!(key.code, KeyCode::Left | KeyCode::Right),
        ChordMode::None => false,
    }
}

fn first_leaf(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf { pane_id } => Some(*pane_id),
        Node::Split { first, .. } => first_leaf(first),
    }
}

fn visible_leaf_panes(node: &Node) -> Vec<PaneId> {
    match node {
        Node::Leaf { pane_id } => vec![*pane_id],
        Node::Split { first, second, .. } => {
            let mut panes = visible_leaf_panes(first);
            panes.extend(visible_leaf_panes(second));
            panes
        }
    }
}

fn pane_at(panes: &BTreeMap<PaneId, Rect>, column: u16, row: u16) -> Option<PaneId> {
    panes
        .iter()
        .find_map(|(pane_id, rect)| rect_contains(*rect, column, row).then_some(*pane_id))
}

fn resize_border_hit(
    panes: &BTreeMap<PaneId, Rect>,
    column: u16,
    row: u16,
) -> Option<(PaneId, bool, bool)> {
    panes.iter().find_map(|(pane_id, rect)| {
        if !rect_contains(*rect, column, row)
            || rect_contains(pane_content_rect(*rect), column, row)
        {
            return None;
        }
        let vertical = (rect.width > 0
            && column == rect.x
            && panes.iter().any(|(other_id, other)| {
                other_id != pane_id
                    && other.right() == rect.x
                    && rect_contains(*other, other.x, row)
            }))
            || (rect.width > 0
                && column == rect.right().saturating_sub(1)
                && panes.iter().any(|(other_id, other)| {
                    other_id != pane_id
                        && other.x == rect.right()
                        && rect_contains(*other, other.x, row)
                }));
        let horizontal = (rect.height > 0
            && row == rect.y
            && panes.iter().any(|(other_id, other)| {
                other_id != pane_id
                    && other.bottom() == rect.y
                    && rect_contains(*other, column, other.y)
            }))
            || (rect.height > 0
                && row == rect.bottom().saturating_sub(1)
                && panes.iter().any(|(other_id, other)| {
                    other_id != pane_id
                        && other.y == rect.bottom()
                        && rect_contains(*other, column, other.y)
                }));
        (vertical || horizontal).then_some((*pane_id, vertical, horizontal))
    })
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    u32::from(column) >= u32::from(rect.x)
        && u32::from(column) < u32::from(rect.x) + u32::from(rect.width)
        && u32::from(row) >= u32::from(rect.y)
        && u32::from(row) < u32::from(rect.y) + u32::from(rect.height)
}

fn text_width(text: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
}

fn truncate_trailing(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut result = String::new();
    for character in value.chars() {
        if UnicodeWidthStr::width(result.as_str()).saturating_add(character.width().unwrap_or(0))
            > width - 1
        {
            break;
        }
        result.push(character);
    }
    result.push('…');
    result
}

fn tab_label(title: Option<&str>, index: usize, active: bool, unread: bool) -> String {
    let label = title.map_or_else(|| format!("Tab #{index}"), str::to_owned);
    let label = if unread { format!("* {label}") } else { label };
    if active { format!(" {label} ") } else { label }
}

#[allow(clippy::too_many_arguments)]
fn pane_title(
    custom_title: Option<&str>,
    index: usize,
    host_peer_id: &[u8],
    controller_peer_id: Option<&[u8]>,
    locked: bool,
    exited: bool,
    members: &[crate::layout::Member],
    available_width: usize,
) -> Line<'static> {
    let control = match controller_peer_id {
        Some([]) => "free".to_owned(),
        Some(peer_id) => member_label(peer_id, members),
        None => "…".to_owned(),
    };
    let label = truncate_trailing(
        &custom_title.map_or_else(|| format!("Pane #{index}"), str::to_owned),
        available_width,
    );
    let control = if exited {
        "exited"
    } else if locked {
        "host-only"
    } else {
        &control
    };
    let metadata = format!(
        " host: {} control: {control}",
        member_label(host_peer_id, members)
    );
    let metadata_width = available_width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
    Line::from(vec![
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(truncate_trailing(&metadata, metadata_width)),
    ])
}

fn pane_border_color(
    theme: &UiTheme,
    exited: bool,
    controller_peer_id: Option<&[u8]>,
    _controller_active: bool,
    focused: bool,
    hovered: bool,
    chord_mode: ChordMode,
) -> Color {
    if exited {
        return theme.pane_border_idle;
    }
    if focused && chord_mode == ChordMode::Pane {
        return theme.pane_border_chord_focused;
    }

    match controller_peer_id {
        Some([]) if focused => theme.pane_border_free_focused,
        None if focused => theme.pane_border_unknown_focused,
        Some([]) | None if hovered => theme.pane_border_hovered,
        Some([]) | None => theme.pane_border_idle,
        Some(_) => theme.pane_border_remote_control,
    }
}

fn resize_proposed_share(drag: ResizeDrag, column: u16, row: u16) -> u16 {
    let delta = match drag.axis.expect("locked resize drag has an axis") {
        Axis::LeftRight => i32::from(column) - i32::from(drag.origin_column),
        Axis::TopBottom => i32::from(row) - i32::from(drag.origin_row),
    };
    (i32::from(drag.original_share_bps) + delta * 10_000 / i32::from(drag.span.max(1)))
        .clamp(1, 9_999) as u16
}

fn contains_leaf(node: &Node, pane_id: PaneId) -> bool {
    match node {
        Node::Leaf { pane_id: candidate } => *candidate == pane_id,
        Node::Split { first, second, .. } => {
            contains_leaf(first, pane_id) || contains_leaf(second, pane_id)
        }
    }
}

fn nearest_split_for_pane(
    node: &Node,
    pane_id: PaneId,
    axis: Axis,
    area: Rect,
) -> Option<SplitTarget> {
    let Node::Split {
        axis: split_axis,
        first_share_bps,
        first,
        second,
    } = node
    else {
        return None;
    };
    let (first_area, second_area) = split_areas(*split_axis, *first_share_bps, first, second, area);
    if contains_leaf(first, pane_id) {
        nearest_split_for_pane(first, pane_id, axis, first_area).or_else(|| {
            (*split_axis == axis).then_some(SplitTarget {
                first_share_bps: *first_share_bps,
                span: match axis {
                    Axis::LeftRight => area.width,
                    Axis::TopBottom => area.height,
                },
            })
        })
    } else if contains_leaf(second, pane_id) {
        nearest_split_for_pane(second, pane_id, axis, second_area).or_else(|| {
            (*split_axis == axis).then_some(SplitTarget {
                first_share_bps: *first_share_bps,
                span: match axis {
                    Axis::LeftRight => area.width,
                    Axis::TopBottom => area.height,
                },
            })
        })
    } else {
        None
    }
}

fn node_minimum(node: &Node) -> (u16, u16) {
    match node {
        Node::Leaf { .. } => (3, 3),
        Node::Split {
            axis,
            first,
            second,
            ..
        } => {
            let (first_width, first_height) = node_minimum(first);
            let (second_width, second_height) = node_minimum(second);
            match axis {
                Axis::LeftRight => (
                    first_width.saturating_add(second_width),
                    first_height.max(second_height),
                ),
                Axis::TopBottom => (
                    first_width.max(second_width),
                    first_height.saturating_add(second_height),
                ),
            }
        }
    }
}

fn allocated_first_span(span: u16, share_bps: u16, first_min: u16, second_min: u16) -> u16 {
    let unconstrained = ((u32::from(span) * u32::from(share_bps)) / 10_000) as u16;
    if span >= first_min.saturating_add(second_min) {
        unconstrained.clamp(first_min, span.saturating_sub(second_min))
    } else {
        unconstrained.min(span)
    }
}

fn split_areas(
    axis: Axis,
    first_share_bps: u16,
    first: &Node,
    second: &Node,
    area: Rect,
) -> (Rect, Rect) {
    match axis {
        Axis::LeftRight => {
            let (first_min, _) = node_minimum(first);
            let (second_min, _) = node_minimum(second);
            let first_width =
                allocated_first_span(area.width, first_share_bps, first_min, second_min);
            (
                Rect::new(area.x, area.y, first_width, area.height),
                Rect::new(
                    area.x.saturating_add(first_width),
                    area.y,
                    area.width - first_width,
                    area.height,
                ),
            )
        }
        Axis::TopBottom => {
            let (_, first_min) = node_minimum(first);
            let (_, second_min) = node_minimum(second);
            let first_height =
                allocated_first_span(area.height, first_share_bps, first_min, second_min);
            (
                Rect::new(area.x, area.y, area.width, first_height),
                Rect::new(
                    area.x,
                    area.y.saturating_add(first_height),
                    area.width,
                    area.height - first_height,
                ),
            )
        }
    }
}

fn allocate_node_with_preview(
    node: &Node,
    area: Rect,
    panes: &mut BTreeMap<PaneId, Rect>,
    preview: Option<ResizePreview>,
) {
    match node {
        Node::Leaf { pane_id } => {
            panes.insert(*pane_id, area);
        }
        Node::Split {
            axis,
            first_share_bps,
            first,
            second,
        } => {
            let share = preview
                .filter(|preview| {
                    preview.axis == *axis
                        && split_is_nearest_for_pane(node, preview.pane_id, preview.axis)
                })
                .map_or(*first_share_bps, |preview| preview.first_share_bps);
            let (first_area, second_area) = split_areas(*axis, share, first, second, area);
            allocate_node_with_preview(first, first_area, panes, preview);
            allocate_node_with_preview(second, second_area, panes, preview);
        }
    }
}

fn split_is_nearest_for_pane(node: &Node, pane_id: PaneId, axis: Axis) -> bool {
    let Node::Split {
        axis: split_axis,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    if *split_axis != axis {
        return false;
    }
    let child = if contains_leaf(first, pane_id) {
        first
    } else if contains_leaf(second, pane_id) {
        second
    } else {
        return false;
    };
    !contains_split_for_pane(child, pane_id, axis)
}

fn contains_split_for_pane(node: &Node, pane_id: PaneId, axis: Axis) -> bool {
    let Node::Split {
        axis: split_axis,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    if contains_leaf(first, pane_id) {
        *split_axis == axis || contains_split_for_pane(first, pane_id, axis)
    } else if contains_leaf(second, pane_id) {
        *split_axis == axis || contains_split_for_pane(second, pane_id, axis)
    } else {
        false
    }
}

pub(crate) fn grid_for_pane(rect: Rect) -> (u16, u16) {
    let content = pane_content_rect(rect);
    (content.height.max(1), content.width.max(1))
}

pub(crate) fn initial_root_pane_grid(cols: u16, rows: u16) -> (u16, u16) {
    grid_for_pane(Rect::new(0, 0, cols, rows.saturating_sub(2)))
}

fn rect_center(rect: Rect) -> (u32, u32) {
    (
        u32::from(rect.x) * 2 + u32::from(rect.width),
        u32::from(rect.y) * 2 + u32::from(rect.height),
    )
}

fn is_in_direction(source: (u32, u32), target: (u32, u32), direction: KeyCode) -> bool {
    match direction {
        KeyCode::Left => target.0 < source.0,
        KeyCode::Right => target.0 > source.0,
        KeyCode::Up => target.1 < source.1,
        KeyCode::Down => target.1 > source.1,
        _ => false,
    }
}

fn direction_distance(
    source: (u32, u32),
    target: (u32, u32),
    direction: KeyCode,
    pane_id: PaneId,
) -> (u32, u32, u32, PaneId) {
    match direction {
        KeyCode::Left | KeyCode::Right => {
            let primary = source.0.abs_diff(target.0);
            let secondary = source.1.abs_diff(target.1);
            (primary + secondary, secondary, primary, pane_id)
        }
        KeyCode::Up | KeyCode::Down => {
            let primary = source.1.abs_diff(target.1);
            let secondary = source.0.abs_diff(target.0);
            (primary + secondary, secondary, primary, pane_id)
        }
        _ => (u32::MAX, u32::MAX, u32::MAX, pane_id),
    }
}

fn fixed_grid_viewport(inner: Rect, rows: u16, cols: u16) -> Rect {
    let width = inner.width.min(cols);
    let height = inner.height.min(rows);
    Rect::new(inner.x, inner.y, width, height)
}

fn pane_content_rect(pane_rect: Rect) -> Rect {
    Block::bordered().inner(pane_rect)
}

/// Pins a pointer that wandered off the pane to the nearest cell inside it.
fn clamp_to_viewport(viewport: Rect, column: u16, row: u16) -> Option<ScreenCell> {
    if viewport.width == 0 || viewport.height == 0 {
        return None;
    }
    let last_column = viewport.x + viewport.width - 1;
    let last_row = viewport.y + viewport.height - 1;
    Some(ScreenCell {
        row: row.clamp(viewport.y, last_row) - viewport.y,
        col: column.clamp(viewport.x, last_column) - viewport.x,
    })
}

fn mouse_to_screen_cell(viewport: Rect, column: u16, row: u16) -> Option<ScreenCell> {
    rect_contains(viewport, column, row).then_some(ScreenCell {
        row: row.saturating_sub(viewport.y),
        col: column.saturating_sub(viewport.x),
    })
}

fn selection_text(screen: &vt100::Screen, selection: PaneTextSelection) -> Option<String> {
    if selection.is_empty() {
        return None;
    }
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }
    let (start, end) = selection.bounds();
    let start = ScreenCell {
        row: start.row.min(rows.saturating_sub(1)),
        col: start.col.min(cols.saturating_sub(1)),
    };
    let end = ScreenCell {
        row: end.row.min(rows.saturating_sub(1)),
        col: end.col.min(cols.saturating_sub(1)),
    };

    let lines = (start.row..=end.row)
        .map(|row| {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row { end.col } else { cols - 1 };
            let mut line = String::new();
            for col in first_col..=last_col {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if !cell.is_wide_continuation() {
                    let contents = cell.contents();
                    line.push_str(if contents.is_empty() { " " } else { contents });
                }
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>();
    Some(lines.join("\n"))
}

pub(crate) fn copy_selection_to_clipboard(text: &str) -> io::Result<usize> {
    copy_to_macos_clipboard(text)?;
    Ok(copied_line_count(text))
}

fn copy_to_macos_clipboard(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("pbcopy stdin unavailable"))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    if child.wait()?.success() {
        Ok(())
    } else {
        Err(io::Error::other("pbcopy failed"))
    }
}

fn copied_line_count(text: &str) -> usize {
    text.split('\n').count().max(1)
}

/// Renders layout chrome plus any currently available fixed-size VT screens.
pub fn render_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
) {
    render_shared_multi_pane(frame, tui, screens, "", None, None, None, None);
}

/// Renders the local attachment footer with its own copy feedback.
#[allow(clippy::too_many_arguments)]
pub fn render_multi_pane_with_copy_feedback(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
    copied_lines: Option<usize>,
    footer_notice: Option<&str>,
    join_code: Option<&str>,
    local_peer_id: Option<&[u8]>,
    link: Option<&str>,
) {
    let exited_notice = tui
        .snapshot()
        .panes
        .get(&tui.focused_pane())
        .filter(|pane| pane.exited)
        .map(|pane| {
            if local_peer_id == Some(pane.host_peer_id.as_slice()) {
                "exited — close with Ctrl+P, X"
            } else {
                "exited — input disabled; pane host can close with Ctrl+P, X"
            }
        });
    render_shared_multi_pane(
        frame,
        tui,
        screens,
        "",
        copied_lines,
        footer_notice.or(exited_notice),
        join_code,
        link,
    );
}

fn contextual_footer(chord_mode: ChordMode) -> (&'static str, &'static [FooterSegment]) {
    match chord_mode {
        ChordMode::None => (CONTROL_HELP, NORMAL_FOOTER),
        ChordMode::Pane => (
            "  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <k> LOCK   <L> LOCK SESSION   <i> INVITE   <Esc> BACK",
            PANE_FOOTER,
        ),
        ChordMode::Tab => (
            "  <←→> SWITCH   <e> RENAME   <n> NEW   <x> CLOSE   <Esc> BACK",
            TAB_FOOTER,
        ),
    }
}

fn render_footer_segments(
    buffer: &mut Buffer,
    theme: &UiTheme,
    mut x: u16,
    y: u16,
    end_x: u16,
    segments: &[FooterSegment],
) -> u16 {
    for segment in segments {
        let (text, style) = match segment {
            FooterSegment::Text(text) => (
                *text,
                Style::default()
                    .fg(theme.footer_muted)
                    .bg(theme.footer_background),
            ),
            FooterSegment::Key(text) => (
                *text,
                Style::default()
                    .fg(theme.footer_accent)
                    .bg(theme.footer_background)
                    .add_modifier(Modifier::BOLD),
            ),
            FooterSegment::OrangeKey(text) => (
                *text,
                Style::default()
                    .fg(theme.footer_orange)
                    .bg(theme.footer_background)
                    .add_modifier(Modifier::BOLD),
            ),
        };
        x = buffer
            .set_stringn(x, y, text, usize::from(end_x.saturating_sub(x)), style)
            .0;
    }
    x
}

fn footer_segments_width(segments: &[FooterSegment]) -> u16 {
    segments
        .iter()
        .map(|segment| match segment {
            FooterSegment::Text(text)
            | FooterSegment::Key(text)
            | FooterSegment::OrangeKey(text) => text_width(text),
        })
        .sum()
}

fn chord_footer_badge(chord_mode: ChordMode, width: u16) -> Option<&'static str> {
    let (full, compact) = match chord_mode {
        ChordMode::Pane => ("PANE MODE", "PANE"),
        ChordMode::Tab => ("TAB MODE", "TAB"),
        ChordMode::None => return None,
    };

    if width >= text_width(full) {
        Some(full)
    } else if width >= text_width(compact) {
        Some(compact)
    } else {
        None
    }
}

fn render_copy_feedback(
    buffer: &mut Buffer,
    theme: &UiTheme,
    mut x: u16,
    y: u16,
    end_x: u16,
    lines: usize,
) {
    x = buffer
        .set_stringn(
            x,
            y,
            "  copied ",
            usize::from(end_x.saturating_sub(x)),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background),
        )
        .0;
    buffer.set_stringn(
        x,
        y,
        format!("{lines} line{}", if lines == 1 { "" } else { "s" }),
        usize::from(end_x.saturating_sub(x)),
        Style::default()
            .fg(theme.copy_feedback_accent)
            .bg(theme.footer_background),
    );
}

fn footer_suffix(text: &str, width: usize) -> &str {
    if width == 0 {
        return "";
    }
    let start = text
        .char_indices()
        .nth(text.chars().count().saturating_sub(width))
        .map_or(0, |(index, _)| index);
    &text[start..]
}

fn mid_footer_flash_width(copied_lines: Option<usize>, footer_notice: Option<&str>) -> u16 {
    if let Some(notice) = footer_notice {
        return u16::try_from(notice.chars().count().saturating_add(2)).unwrap_or(u16::MAX);
    }
    if let Some(lines) = copied_lines {
        let count = format!("{lines} line{}", if lines == 1 { "" } else { "s" });
        return u16::try_from(
            count
                .chars()
                .count()
                .saturating_add("  copied ".chars().count()),
        )
        .unwrap_or(u16::MAX);
    }
    0
}

struct FooterText {
    text: String,
    rect: Rect,
}

enum FooterFlash {
    Notice(FooterText),
    CopyFeedback { lines: usize, rect: Rect },
}

/// The shared placement authority for footer content, including the join command.
struct FooterLayout {
    badge: Option<&'static str>,
    help_start: u16,
    help_end: u16,
    status: Option<FooterText>,
    flash: Option<FooterFlash>,
    join_x: u16,
    join_text: Option<String>,
    join_rect: Option<Rect>,
}

fn footer_layout(
    area: Rect,
    chord_mode: ChordMode,
    status: &str,
    footer_notice: Option<&str>,
    copied_lines: Option<usize>,
    join_code: Option<&str>,
) -> FooterLayout {
    let end_x = area.right();
    let join = join_code.map(|code| format!("join: p2pmux join {code}"));

    if chord_mode != ChordMode::None {
        let badge = chord_footer_badge(chord_mode, area.width);
        let mut x = area.x.saturating_add(badge.map_or(0, text_width));
        let (_, segments) = contextual_footer(chord_mode);
        if badge.is_none() || end_x.saturating_sub(x) < footer_segments_width(segments) {
            return FooterLayout {
                badge,
                help_start: x,
                help_end: end_x,
                status: None,
                flash: None,
                join_x: end_x,
                join_text: None,
                join_rect: None,
            };
        }

        x = x.saturating_add(footer_segments_width(segments));
        let help_end = x;
        let status = (!status.is_empty()).then(|| {
            let text = format!("{status}  ");
            let width = text_width(&text);
            (text, width)
        });
        let status = status.and_then(|(text, width)| {
            (end_x.saturating_sub(x) >= width).then(|| {
                let rect = Rect::new(x, area.y, width, 1);
                x = x.saturating_add(width);
                FooterText { text, rect }
            })
        });
        let flash = footer_notice.and_then(|notice| {
            let text = format!("  {notice}");
            let width = text_width(&text);
            (end_x.saturating_sub(x) >= width).then(|| {
                let rect = Rect::new(x, area.y, width, 1);
                x = x.saturating_add(width);
                FooterFlash::Notice(FooterText { text, rect })
            })
        });
        let join_text = join.filter(|text| end_x.saturating_sub(x) >= text_width(text));
        let join_rect = join_text
            .as_ref()
            .map(|text| Rect::new(x, area.y, text_width(text), 1));

        return FooterLayout {
            badge,
            help_start: area.x.saturating_add(badge.map_or(0, text_width)),
            help_end,
            status,
            flash,
            join_x: join_rect.map_or(end_x, |rect| rect.x),
            join_text,
            join_rect,
        };
    }

    let mut x = area.x;
    let status = (!status.is_empty()).then(|| {
        let text = format!("{status}  ");
        let width = text_width(&text).min(end_x.saturating_sub(x));
        let rect = Rect::new(x, area.y, width, 1);
        x = x.saturating_add(width);
        FooterText { text, rect }
    });
    let join_text = join.as_deref().and_then(|text| {
        let text = footer_suffix(text, usize::from(end_x.saturating_sub(x)));
        (!text.is_empty()).then(|| text.to_owned())
    });
    let join_x = join_text.as_ref().map_or(end_x, |text| {
        end_x.saturating_sub(u16::try_from(text.chars().count()).unwrap_or(u16::MAX))
    });
    let join_rect = join_text.as_ref().map(|text| {
        Rect::new(
            join_x,
            area.y,
            u16::try_from(text.chars().count()).unwrap_or(u16::MAX),
            1,
        )
    });
    let flash_width = if footer_notice.is_some() {
        mid_footer_flash_width(None, footer_notice)
    } else if status.is_none() {
        mid_footer_flash_width(copied_lines, None)
    } else {
        0
    };
    let help_end = join_x.saturating_sub(flash_width);
    let flash_start = x.max(help_end);
    let flash_rect = Rect::new(flash_start, area.y, join_x.saturating_sub(flash_start), 1);
    let flash = footer_notice
        .map(|notice| {
            FooterFlash::Notice(FooterText {
                text: format!("  {notice}"),
                rect: flash_rect,
            })
        })
        .or_else(|| {
            status
                .is_none()
                .then_some(copied_lines)
                .flatten()
                .map(|lines| FooterFlash::CopyFeedback {
                    lines,
                    rect: flash_rect,
                })
        });

    FooterLayout {
        badge: None,
        help_start: x,
        help_end,
        status,
        flash,
        join_x,
        join_text,
        join_rect,
    }
}

fn rect_contains_mouse(rect: Rect, column: u16, row: u16) -> bool {
    (rect.x..rect.right()).contains(&column) && (rect.y..rect.bottom()).contains(&row)
}

fn render_chord_footer(
    buffer: &mut Buffer,
    theme: &UiTheme,
    area: Rect,
    chord_mode: ChordMode,
    layout: &FooterLayout,
) {
    let Some(badge) = layout.badge else {
        return;
    };
    buffer.set_stringn(
        area.x,
        area.y,
        badge,
        usize::from(area.right().saturating_sub(area.x)),
        Style::default()
            .fg(theme.footer_orange)
            .bg(theme.footer_background)
            .add_modifier(Modifier::BOLD),
    );
    let (_, segments) = contextual_footer(chord_mode);
    render_footer_segments(
        buffer,
        theme,
        layout.help_start,
        area.y,
        layout.help_end,
        segments,
    );

    if let Some(status) = &layout.status {
        buffer.set_stringn(
            status.rect.x,
            area.y,
            &status.text,
            usize::from(status.rect.width),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background),
        );
    }
    if let Some(FooterFlash::Notice(notice)) = &layout.flash {
        buffer.set_stringn(
            notice.rect.x,
            area.y,
            &notice.text,
            usize::from(notice.rect.width),
            Style::default()
                .fg(theme.footer_orange)
                .bg(theme.footer_background)
                .add_modifier(Modifier::BOLD),
        );
    }
    if let (Some(join), Some(join_rect)) = (&layout.join_text, layout.join_rect) {
        buffer.set_stringn(
            layout.join_x,
            area.y,
            join,
            usize::from(join_rect.width),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background)
                .add_modifier(Modifier::UNDERLINED),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_contextual_footer(
    buffer: &mut Buffer,
    theme: &UiTheme,
    area: Rect,
    status: &str,
    copied_lines: Option<usize>,
    footer_notice: Option<&str>,
    join_code: Option<&str>,
    chord_mode: ChordMode,
) {
    let layout = footer_layout(
        area,
        chord_mode,
        status,
        footer_notice,
        copied_lines,
        join_code,
    );
    buffer.set_stringn(
        area.x,
        area.y,
        " ".repeat(usize::from(area.width)),
        usize::from(area.width),
        Style::default().bg(theme.footer_background),
    );

    if chord_mode != ChordMode::None {
        render_chord_footer(buffer, theme, area, chord_mode, &layout);
        return;
    }

    if let Some(status) = &layout.status {
        buffer.set_stringn(
            status.rect.x,
            area.y,
            &status.text,
            usize::from(status.rect.width),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background),
        );
    }
    let (_, segments) = contextual_footer(chord_mode);
    render_footer_segments(
        buffer,
        theme,
        layout.help_start,
        area.y,
        layout.help_end,
        segments,
    );
    // Mid-bar flash messages sit after chord help and before the join code.
    match &layout.flash {
        Some(FooterFlash::Notice(notice)) => {
            buffer.set_stringn(
                notice.rect.x,
                area.y,
                &notice.text,
                usize::from(notice.rect.width),
                Style::default()
                    .fg(theme.footer_orange)
                    .bg(theme.footer_background)
                    .add_modifier(Modifier::BOLD),
            );
        }
        Some(FooterFlash::CopyFeedback { lines, rect }) => {
            render_copy_feedback(buffer, theme, rect.x, area.y, rect.right(), *lines);
        }
        None => {}
    }
    if let (Some(text), Some(join_rect)) = (&layout.join_text, layout.join_rect) {
        buffer.set_stringn(
            layout.join_x,
            area.y,
            text,
            usize::from(join_rect.width),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background)
                .add_modifier(Modifier::UNDERLINED),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_shared_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
    status: &str,
    copied_lines: Option<usize>,
    footer_notice: Option<&str>,
    join_code: Option<&str>,
    link: Option<&str>,
) {
    let theme = &tui.theme;
    let geometry = tui.geometry(frame.area());
    if geometry.tab_bar.width > 0 && geometry.tab_bar.height > 0 {
        let buffer = frame.buffer_mut();
        buffer.set_stringn(
            geometry.tab_bar.x,
            geometry.tab_bar.y,
            " ".repeat(usize::from(geometry.tab_bar.width)),
            usize::from(geometry.tab_bar.width),
            Style::default().bg(theme.footer_background),
        );
        let mut x = buffer
            .set_stringn(
                geometry.tab_bar.x,
                geometry.tab_bar.y,
                tui.title(),
                usize::from(geometry.tab_bar.width),
                Style::default()
                    .fg(theme.tab_foreground)
                    .bg(theme.footer_background),
            )
            .0;
        x = buffer
            .set_stringn(
                x,
                geometry.tab_bar.y,
                TOP_BAR_BRAND_SEPARATOR,
                usize::from(geometry.tab_bar.right().saturating_sub(x)),
                Style::default()
                    .fg(theme.tab_separator)
                    .bg(theme.footer_background),
            )
            .0;
        for (index, tab) in tui.snapshot.tabs.iter().enumerate() {
            if index > 0 {
                x = buffer
                    .set_stringn(
                        x,
                        geometry.tab_bar.y,
                        TAB_BAR_SEPARATOR,
                        usize::from(geometry.tab_bar.right().saturating_sub(x)),
                        Style::default()
                            .fg(theme.tab_separator)
                            .bg(theme.footer_background),
                    )
                    .0;
            }
            let active = tab.tab_id == tui.current_tab;
            let style = if active {
                Style::default()
                    .fg(theme.tab_foreground)
                    .bg(theme.tab_active_background)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(if tui.chord_mode == ChordMode::Tab {
                        theme.tab_separator
                    } else {
                        theme.tab_foreground
                    })
                    .bg(theme.footer_background)
            };
            let label = truncate_trailing(
                &tab_label(
                    tab.title.as_deref(),
                    index + 1,
                    active,
                    tui.tab_has_unread_agent_pane(tab),
                ),
                usize::from(geometry.tab_labels[&tab.tab_id].width),
            );
            let label_rect = geometry.tab_labels[&tab.tab_id];
            if label_rect.width == 0 {
                continue;
            }
            buffer.set_stringn(
                label_rect.x,
                label_rect.y,
                &label,
                usize::from(label_rect.width),
                style,
            );
            x = x.saturating_add(text_width(&label));
        }
        // `direct 35ms` / `relayed 120ms ×3`, right-aligned. Lives in the tab bar rather
        // than the footer because the footer is transient -- chords, copy feedback and
        // status notices all take it over -- and connectivity is exactly the fact you
        // want on screen at the moment something else has gone wrong.
        let badge = match (tui.session_locked, link.filter(|link| !link.is_empty())) {
            (true, Some(link)) => Some(format!("locked · {link}")),
            (true, None) => Some(String::from("locked")),
            (false, link) => link.map(str::to_owned),
        };
        if let Some(link) = badge.as_deref() {
            let width = text_width(link);
            let start = geometry.tab_bar.right().saturating_sub(width);
            // Never overwrite a tab label: if the tabs already reach that far, the tab
            // names are the more important thing and the badge simply stands down.
            if width > 0 && start > x {
                buffer.set_stringn(
                    start,
                    geometry.tab_bar.y,
                    link,
                    usize::from(width),
                    Style::default()
                        .fg(theme.tab_separator)
                        .bg(theme.footer_background),
                );
            }
        }
    }
    if geometry.footer.width > 0 && geometry.footer.height > 0 {
        render_contextual_footer(
            frame.buffer_mut(),
            theme,
            geometry.footer,
            status,
            copied_lines,
            footer_notice,
            join_code,
            tui.chord_mode,
        );
    }

    let pane_ids = tui
        .current_tab_layout()
        .map(|tab| visible_leaf_panes(&tab.root))
        .unwrap_or_default();
    for (index, pane_id) in pane_ids.into_iter().enumerate() {
        let Some(rect) = geometry.panes.get(&pane_id).copied() else {
            continue;
        };
        let pane = &tui.snapshot.panes[&pane_id];
        let view = tui.pane_views.get(&pane_id).cloned().unwrap_or_default();
        let focused = pane_id == tui.focused_pane;
        let title_width = usize::from(rect.width.saturating_sub(2));
        let lock_badge = (!pane.exited && pane.locked).then(|| {
            format!(
                "(locked by {})",
                member_label(&pane.host_peer_id, &tui.snapshot.members)
            )
        });
        let badge_width = lock_badge
            .as_deref()
            .map_or(0, |badge| UnicodeWidthStr::width(badge).min(title_width));
        let mut title = pane_title(
            pane.title.as_deref(),
            index + 1,
            &pane.host_peer_id,
            view.controller_peer_id.as_deref(),
            pane.locked,
            pane.exited,
            &tui.snapshot.members,
            title_width.saturating_sub(badge_width).saturating_sub(2),
        );
        title.spans.insert(
            0,
            Span::raw(if tui.unread_agent_panes.contains(&pane_id) {
                " * "
            } else {
                " "
            }),
        );
        title.spans.push(Span::raw(" "));
        let border_color = pane_border_color(
            theme,
            pane.exited,
            view.controller_peer_id.as_deref(),
            view.controller_active,
            focused,
            tui.hovered_pane == Some(pane_id),
            tui.chord_mode,
        );
        let mut block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(border_color));
        if let Some(badge) = lock_badge {
            block = block.title(
                Line::from(truncate_trailing(&badge, badge_width)).alignment(Alignment::Right),
            );
        }
        let content = pane_content_rect(rect);
        frame.render_widget(block, rect);
        // The fixed VT grid may be smaller than this pane after layout reflow. Clear the full
        // interior before drawing it so letterbox margins cannot retain cells from an older pane.
        frame.render_widget(Clear, content);
        if let Some(screen) = screens.get(&pane_id) {
            let viewport = fixed_grid_viewport(content, pane.grid_rows, pane.grid_cols);
            let screen = viewed_screen(screen, tui.scrollback_offset(pane_id));
            frame.render_widget(
                VtScreen::new(&screen).with_selection(
                    tui.selection()
                        .filter(|selection| selection.pane_id == pane_id),
                ),
                viewport,
            );
            let (row, col) = screen.cursor_position();
            if focused
                && view.ready
                && screen.scrollback() == 0
                && !screen.hide_cursor()
                && !pane.exited
                && row < viewport.height
                && col < viewport.width
            {
                frame.set_cursor_position((
                    viewport.x.saturating_add(col),
                    viewport.y.saturating_add(row),
                ));
            }
        } else if !view.ready {
            frame.render_widget(Paragraph::new("waiting for pane snapshot/lease"), content);
        }
    }
    if tui.overlay_open() {
        render_agents_overlay(frame, tui, unix_ms_now());
    }
    if let ModalState::Rename(prompt) = &tui.modal {
        render_rename_prompt(frame, prompt, &tui.theme);
    }
    if let ModalState::ConfirmDeleteTab { pane_count, .. } = &tui.modal {
        render_delete_tab_confirmation(frame, *pane_count, &tui.theme);
    }
}

fn render_agents_overlay(frame: &mut Frame<'_>, tui: &MultiPaneTui, now_unix_ms: u64) {
    let area = frame.area();
    let panel = agents_overlay_panel(area);
    frame.render_widget(Clear, panel);
    let needs_you = tui.agent_rows.iter().any(|row| row.state.needs_you());
    let title_color = if needs_you {
        tui.theme.agent_overlay_attention
    } else {
        tui.theme.agent_overlay_chrome
    };
    let block = Block::bordered()
        .title(Line::styled(
            agents_overlay_title(&tui.agent_rows),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(tui.theme.agent_overlay_chrome));
    let content = agents_overlay_content(area);
    frame.render_widget(block, panel);
    if tui.agent_rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No agents running",
                Style::default().fg(tui.theme.agent_overlay_muted),
            )),
            content,
        );
    } else {
        let mut lines = Vec::with_capacity(tui.agent_rows.len().saturating_mul(3));
        let animation_phase = agent_overlay_animation_phase(now_unix_ms);
        for (index, row) in tui.agent_rows.iter().enumerate() {
            lines.extend(format_agent_overlay_card(
                row,
                tui.agent_selected_pane == Some(row.pane_id),
                content.width,
                now_unix_ms,
                animation_phase,
                &tui.theme,
            ));
            if index + 1 < tui.agent_rows.len() {
                lines.push(Line::raw(""));
            }
        }
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(tui.agent_overlay_scroll_line)
                    .collect::<Vec<_>>(),
            ),
            content,
        );
    }
    render_agents_overlay_help(frame.buffer_mut(), &tui.theme, agents_overlay_help(area));
}

fn agents_overlay_panel(area: Rect) -> Rect {
    let width = area.width.saturating_sub(2).clamp(24, 96);
    let height = area.height.saturating_sub(2).clamp(5, 28);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.min(area.width),
        height.min(area.height),
    )
}

fn agents_overlay_inner(area: Rect) -> Rect {
    Block::bordered().inner(agents_overlay_panel(area))
}

fn agents_overlay_help(area: Rect) -> Option<Rect> {
    let inner = agents_overlay_inner(area);
    (inner.height >= 4).then(|| {
        Rect::new(
            inner.x,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
            inner.width,
            1,
        )
    })
}

fn agents_overlay_content(area: Rect) -> Rect {
    let inner = agents_overlay_inner(area);
    let y = inner.y.saturating_add(1);
    let height = agents_overlay_help(area)
        .map(|help| help.y.saturating_sub(y))
        .unwrap_or_else(|| inner.height.saturating_sub(1));
    Rect::new(
        inner.x.saturating_add(1),
        y,
        inner.width.saturating_sub(2),
        height,
    )
}

fn render_agents_overlay_help(buffer: &mut Buffer, theme: &UiTheme, help: Option<Rect>) {
    let Some(help) = help else {
        return;
    };
    buffer.set_stringn(
        help.x,
        help.y,
        " ".repeat(usize::from(help.width)),
        usize::from(help.width),
        Style::default().bg(theme.footer_background),
    );
    render_footer_segments(
        buffer,
        theme,
        help.x.saturating_add(1),
        help.y,
        help.right(),
        AGENT_OVERLAY_HELP,
    );
}

fn render_rename_prompt(frame: &mut Frame<'_>, prompt: &RenamePrompt, theme: &UiTheme) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(28, 64).min(area.width);
    let height = 7_u16.min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let title = match prompt.target {
        RenameTarget::Pane(_) => " Rename pane ",
        RenameTarget::Tab(_) => " Rename tab ",
    };
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::bordered()
            .title(Line::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(theme.footer_accent)),
        panel,
    );
    let inner = Block::bordered().inner(panel);
    let field = truncate_trailing(&prompt.value, usize::from(inner.width));
    let mut lines = vec![Line::raw(field)];
    if let Some(error) = &prompt.error {
        lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
    } else {
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        "Enter save · Esc cancel",
        Style::default().fg(theme.footer_muted),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_delete_tab_confirmation(frame: &mut Frame<'_>, pane_count: usize, theme: &UiTheme) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(28, 64).min(area.width);
    let height = 7_u16.min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::bordered().border_style(Style::default().fg(theme.footer_accent)),
        panel,
    );
    let inner = Block::bordered().inner(panel);
    let pane_label = if pane_count == 1 { "pane" } else { "panes" };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("Delete tab?", Style::default().add_modifier(Modifier::BOLD)),
            Line::raw(format!("{pane_count} {pane_label}")),
            Line::raw(""),
            Line::styled(
                "Enter/y yes · Esc/n no",
                Style::default().fg(theme.footer_muted),
            ),
        ]),
        inner,
    );
}

/// Colour for a state's glyph and label. `Working` keeps the chrome accent it
/// has always had; the two states that want a human get their own roles so a
/// blocked or failed agent never reads as ordinary muted text.
fn agent_overlay_state_color(state: AgentRosterState, theme: &UiTheme) -> Color {
    match state {
        AgentRosterState::Working => theme.agent_overlay_chrome,
        AgentRosterState::Pending => theme.agent_overlay_attention,
        AgentRosterState::Error => theme.agent_overlay_error,
        AgentRosterState::Idle | AgentRosterState::Done => theme.agent_overlay_muted,
    }
}

/// Overlay title, carrying a count of agents blocked on a human when there are
/// any. The whole point of opening the overlay is finding those, so the count
/// belongs where it is readable before the eye reaches the cards.
fn agents_overlay_title(rows: &[AgentOverlayRow]) -> String {
    let needs_you = rows.iter().filter(|row| row.state.needs_you()).count();
    match needs_you {
        0 => String::from(" Agents "),
        1 => String::from(" Agents · 1 needs you "),
        count => format!(" Agents · {count} need you "),
    }
}

fn format_agent_overlay_card(
    row: &AgentOverlayRow,
    selected: bool,
    width: u16,
    now_unix_ms: u64,
    animation_phase: usize,
    theme: &UiTheme,
) -> [Line<'static>; 2] {
    let marker = if selected { "›" } else { " " };
    let kind = overlay_kind_label(&row.kind);
    let first_prefix = format!("{marker} {kind} · ");
    let cwd = truncate_leading(
        &row.cwd,
        usize::from(width.saturating_sub(text_width(&first_prefix))),
    );
    let card_style = if selected {
        Style::default().bg(theme.agent_overlay_selected_background)
    } else {
        Style::default()
    };
    let first_line = agent_overlay_line(
        vec![
            Span::styled(marker, Style::default().fg(theme.agent_overlay_chrome)),
            Span::raw(" "),
            Span::styled(
                kind,
                Style::default()
                    .fg(theme.agent_overlay_foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(theme.agent_overlay_muted)),
            Span::styled(cwd, Style::default().fg(theme.agent_overlay_secondary)),
        ],
        card_style,
        selected,
        width,
    );

    let (glyph, state, elapsed) = match row.state {
        AgentRosterState::Working => (
            agent_overlay_working_glyph(animation_phase),
            "working",
            Some(format_agent_elapsed(row.working_since_unix_ms, now_unix_ms)),
        ),
        AgentRosterState::Idle => ("○", "idle", None),
        AgentRosterState::Done => ("✓", "done", None),
        AgentRosterState::Pending => ("◆", "needs you", None),
        AgentRosterState::Error => ("✗", "error", None),
    };
    let state_color = agent_overlay_state_color(row.state, theme);
    let location = format!(
        " · {} · {} · host: {}",
        truncate_trailing(&row.tab_label, usize::from(width / 3)),
        truncate_trailing(&row.pane_label, usize::from(width / 3)),
        row.host
    );
    let state_prefix = match elapsed.as_deref() {
        Some(elapsed) => format!("{glyph} {state} {elapsed}"),
        None => format!("{glyph} {state}"),
    };
    let controller_prefix = " · control: ";
    let controller_width = width.saturating_sub(
        text_width(&state_prefix)
            .saturating_add(text_width(&location))
            .saturating_add(text_width(controller_prefix)),
    );
    let controller = (controller_width > 0)
        .then(|| truncate_trailing(&row.controller, usize::from(controller_width)));
    let mut state_style = Style::default().fg(state_color);
    if row.state.needs_you() {
        // A blocked agent is the one row in the overlay that is costing someone
        // time right now, so it gets the only weight in the state column.
        state_style = state_style.add_modifier(Modifier::BOLD);
    }
    let mut second_spans = vec![
        Span::styled(glyph, Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(state, state_style),
    ];
    if let Some(elapsed) = elapsed {
        second_spans.push(Span::raw(" "));
        second_spans.push(Span::styled(
            elapsed,
            Style::default().fg(theme.agent_overlay_warm),
        ));
    }
    second_spans.push(Span::styled(
        location,
        Style::default().fg(theme.agent_overlay_muted),
    ));
    if let Some(controller) = controller {
        second_spans.push(Span::styled(
            controller_prefix,
            Style::default().fg(theme.agent_overlay_muted),
        ));
        second_spans.push(Span::styled(
            controller,
            Style::default().fg(theme.agent_overlay_secondary),
        ));
    }
    [
        first_line,
        agent_overlay_line(second_spans, card_style, selected, width),
    ]
}

fn agent_overlay_animation_phase(now_unix_ms: u64) -> usize {
    now_unix_ms.saturating_div(AGENT_OVERLAY_ANIMATION_INTERVAL.as_millis() as u64) as usize
}

fn agent_overlay_working_glyph(animation_phase: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    FRAMES[animation_phase % FRAMES.len()]
}

fn agent_overlay_line(
    mut spans: Vec<Span<'static>>,
    card_style: Style,
    selected: bool,
    width: u16,
) -> Line<'static> {
    if selected {
        let content_width = spans.iter().fold(0_u16, |total, span| {
            total.saturating_add(text_width(&span.content))
        });
        spans.push(Span::styled(
            " ".repeat(usize::from(width.saturating_sub(content_width))),
            card_style,
        ));
    }
    Line::from(spans).style(card_style)
}

fn overlay_kind_label(kind: &str) -> &'static str {
    match kind {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor Agent",
        "pi" => "Pi",
        "opencode" => "OpenCode",
        _ => "Unknown",
    }
}

fn format_agent_elapsed(working_since_unix_ms: u64, now_unix_ms: u64) -> String {
    let elapsed_seconds = now_unix_ms
        .saturating_sub(working_since_unix_ms)
        .saturating_div(1_000);
    if elapsed_seconds >= 3_600 {
        format!(
            "{}h {}m",
            elapsed_seconds / 3_600,
            (elapsed_seconds % 3_600) / 60
        )
    } else if elapsed_seconds >= 60 {
        format!("{}m {}s", elapsed_seconds / 60, elapsed_seconds % 60)
    } else {
        format!("{elapsed_seconds}s")
    }
}

fn truncate_leading(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut result = String::new();
    for character in value.chars().rev() {
        if UnicodeWidthStr::width(result.as_str()).saturating_add(character.width().unwrap_or(0))
            > width - 1
        {
            break;
        }
        result.insert(0, character);
    }
    format!("…{result}")
}

fn sanitize_single_line(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// One fixed-grid PTY owned by this process. Its watch channels are registered with the pane
/// service before the layout coordinator is told that the pane is ready.
pub struct SharedLocalPane {
    pane_id: PaneId,
    host: PtyHost,
    screen: HostScreen,
    lease: LeaseManager,
    host_peer_id: Vec<u8>,
    locked: bool,
    exited: bool,
    exit_report_pending: bool,
    screen_tx: watch::Sender<ScreenFrame>,
    lease_tx: watch::Sender<LeaseState>,
    control_tx: mpsc::Sender<HostControlEvent>,
    control_rx: mpsc::Receiver<HostControlEvent>,
    agent_tracker: PaneAgentTracker,
    sync_gate: SyncGate,
}

impl SharedLocalPane {
    pub fn spawn(
        pane_id: PaneId,
        grid_rows: u16,
        grid_cols: u16,
        host_peer_id: Vec<u8>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_cwd(pane_id, grid_rows, grid_cols, host_peer_id, None)
    }

    pub(crate) fn spawn_with_cwd(
        pane_id: PaneId,
        grid_rows: u16,
        grid_cols: u16,
        host_peer_id: Vec<u8>,
        cwd: Option<&Path>,
    ) -> Result<Self, Box<dyn Error>> {
        let size = PtySize {
            rows: grid_rows,
            cols: grid_cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cwd = cwd.filter(|path| path.is_dir());
        let host = match PtyHost::spawn_default_shell_with_cwd(size, cwd) {
            Ok(host) => host,
            Err(_) if cwd.is_some_and(|path| !path.is_dir()) => PtyHost::spawn_default_shell(size)?,
            Err(error) => return Err(error),
        };
        let screen = HostScreen::new(grid_rows, grid_cols)?;
        let (screen_tx, _) = watch::channel(screen.current_frame().clone());
        let lease = LeaseManager::new(Vec::new(), Instant::now());
        let (lease_tx, _) = watch::channel(lease.state().clone());
        let (control_tx, control_rx) = mpsc::channel(256);
        Ok(Self {
            pane_id,
            host,
            screen,
            lease,
            host_peer_id,
            locked: false,
            exited: false,
            exit_report_pending: false,
            screen_tx,
            lease_tx,
            control_tx,
            control_rx,
            agent_tracker: PaneAgentTracker::default(),
            sync_gate: SyncGate::default(),
        })
    }

    pub fn channels(&self) -> HostPaneChannels {
        HostPaneChannels {
            pane_id: pane_wire_id(self.pane_id),
            host_peer_id: self.host_peer_id.clone(),
            screen_rx: self.screen_tx.subscribe(),
            lease_rx: self.lease_tx.subscribe(),
            control_tx: self.control_tx.clone(),
        }
    }

    fn view_state(&self) -> PaneViewState {
        PaneViewState {
            ready: true,
            controller_peer_id: Some(self.lease.state().controller_peer_id.clone()),
            controller_active: !self.lease.state().is_idle_at(Instant::now()),
            scrollback: 0,
        }
    }

    fn drain(&mut self) -> Result<LocalPaneDrain, Box<dyn Error>> {
        let mut changed = false;
        if !self.exited && self.host.try_wait()? {
            changed |= self.transition_exited()?;
        }
        if let Some(state) = self.lease.clear_if_idle(Instant::now())? {
            self.lease_tx.send_replace(state);
            changed = true;
        }
        while let Ok(event) = self.control_rx.try_recv() {
            if self.exited {
                continue;
            }
            match event {
                HostControlEvent::Input { peer_id, input } => {
                    if self.locked && peer_id != self.host_peer_id {
                        continue;
                    }
                    if let LeaseDecision::AcceptInput(bytes) =
                        self.lease
                            .input(&peer_id, input.lease_epoch, input.data, Instant::now())
                    {
                        if self.host.write_input(&bytes).is_err() {
                            changed |= self.transition_exited()?;
                            continue;
                        }
                        self.lease_tx.send_replace(self.lease.state().clone());
                        changed = true;
                    }
                }
                HostControlEvent::TakeControl { peer_id, request } => {
                    if self.locked && peer_id != self.host_peer_id {
                        continue;
                    }
                    let decision = self.lease.take_control(
                        peer_id,
                        request.known_lease_epoch,
                        Instant::now(),
                    )?;
                    match decision {
                        LeaseDecision::Publish(state) => {
                            self.lease_tx.send_replace(state);
                            changed = true;
                        }
                        // A normal request while the holder is active does not change the lease,
                        // but the requester needs an authoritative re-publication to clear its
                        // pending claim and try again after the idle timeout.
                        LeaseDecision::RejectActiveController => {
                            self.lease_tx.send_replace(self.lease.state().clone());
                            changed = true;
                        }
                        LeaseDecision::AcceptInput(_)
                        | LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest => {}
                    }
                }
                HostControlEvent::ReleaseControl { peer_id } => {
                    if self.lease.state().controller_peer_id == peer_id
                        && let Some(state) = self.lease.clear_controller(Instant::now())?
                    {
                        self.lease_tx.send_replace(state);
                        changed = true;
                    }
                }
            }
        }
        let started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = self.host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        if !pending.is_empty() {
            self.agent_tracker
                .record_output(Instant::now(), unix_ms_now());
        }
        let ready = if pending.is_empty() {
            self.sync_gate.flush_stale(Instant::now())
        } else {
            self.sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            // One parse/snapshot/diff for the whole batch: process_pty clones the
            // screen per call, so per-chunk calls dominated CPU under output floods.
            let frame = self.screen.process_pty(&ready)?;
            // Read after parsing: the bell is a parser callback, so the count only reflects
            // this batch once the batch has been fed through.
            if self.screen.take_bell_count() > 0 {
                self.agent_tracker.record_completion_signal(Instant::now());
            }
            if let Some(reply) = self.screen.take_kitty_keyboard_query_reply()
                && self.host.write_input(&reply).is_err()
            {
                changed |= self.transition_exited()?;
            } else {
                self.screen_tx.send_replace(frame);
                changed = true;
            }
        }
        if !self.exited && self.host.output_closed() {
            changed |= self.transition_exited()?;
        }
        Ok(LocalPaneDrain {
            changed,
            newly_exited: std::mem::take(&mut self.exit_report_pending),
        })
    }

    fn apply_agent_snapshot(&mut self, processes: &[ProcessSnapshot], now: Instant) -> bool {
        let unix_ms_now = unix_ms_now();
        let before = self.agent_tracker.listed_agent(now, unix_ms_now);
        let detected = self
            .host
            .process_id()
            .and_then(|pid| classify_pane_tree(pid, processes));
        self.agent_tracker.update(detected, now, unix_ms_now);
        self.agent_tracker.listed_agent(now, unix_ms_now) != before
    }

    fn agent_roster_entry(&mut self, now: Instant) -> Option<AgentRosterEntry> {
        let (agent, state) = self.agent_tracker.listed_agent(now, unix_ms_now())?;
        Some(AgentRosterEntry {
            pane_id: self.pane_id,
            agent_kind: agent.kind.wire_value().into(),
            cwd: agent.cwd,
            state: match state {
                crate::agent_detect::AgentState::Idle => AgentRosterState::Idle as i32,
                crate::agent_detect::AgentState::Working => AgentRosterState::Working as i32,
                crate::agent_detect::AgentState::Done => AgentRosterState::Done as i32,
            },
            working_since_unix_ms: if state == crate::agent_detect::AgentState::Working {
                self.agent_tracker.working_since_unix_ms
            } else {
                0
            },
        })
    }

    fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        if self.exited {
            return Ok(());
        }
        let epoch = self.lease.state().epoch;
        let decision = self
            .lease
            .input(&self.host_peer_id, epoch, bytes, Instant::now());
        match decision {
            LeaseDecision::AcceptInput(bytes) => {
                if self.host.write_input(&bytes).is_err() {
                    let _ = self.transition_exited()?;
                }
                self.lease_tx.send_replace(self.lease.state().clone());
            }
            LeaseDecision::Publish(_) => {}
            LeaseDecision::RejectStaleInput
            | LeaseDecision::RejectStaleRequest
            | LeaseDecision::RejectActiveController => {}
        }
        Ok(())
    }

    fn release_controller(&mut self, peer_id: &[u8]) -> Result<bool, Box<dyn Error>> {
        if self.lease.state().controller_peer_id != peer_id {
            return Ok(false);
        }
        let Some(state) = self.lease.clear_controller(Instant::now())? else {
            return Ok(false);
        };
        self.lease_tx.send_replace(state);
        Ok(true)
    }

    fn set_locked(&mut self, locked: bool) -> Result<bool, Box<dyn Error>> {
        if self.locked == locked {
            return Ok(false);
        }
        self.locked = locked;
        if locked
            && self.lease.state().controller_peer_id != self.host_peer_id
            && let Some(state) = self.lease.clear_controller(Instant::now())?
        {
            self.lease_tx.send_replace(state);
        }
        Ok(true)
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Box<dyn Error>> {
        if self.exited {
            return Ok(());
        }
        if self.screen.screen().size() == (rows, cols) {
            return Ok(());
        }
        self.host.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let frame = self.screen.resize(rows, cols)?;
        self.screen_tx.send_replace(frame);
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), Box<dyn Error>> {
        self.host.shutdown()
    }

    fn transition_exited(&mut self) -> Result<bool, Box<dyn Error>> {
        if self.exited {
            return Ok(false);
        }
        self.exited = true;
        self.exit_report_pending = true;
        while self.control_rx.try_recv().is_ok() {}
        if let Some(state) = self.lease.clear_controller(Instant::now())? {
            self.lease_tx.send_replace(state);
        }
        Ok(true)
    }

    fn mark_exited(&mut self) -> Result<(), Box<dyn Error>> {
        self.transition_exited()?;
        self.exit_report_pending = false;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LocalPaneDrain {
    changed: bool,
    newly_exited: bool,
}

struct AgentSamplingWorker {
    snapshots: Receiver<Vec<ProcessSnapshot>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AgentSamplingWorker {
    fn spawn() -> Self {
        let (snapshot_tx, snapshots) = sync_mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let join = thread::spawn(move || {
            let mut sampler = SysinfoSampler::default();
            while !worker_stop.load(Ordering::Relaxed) {
                if snapshot_tx
                    .send(sample_global_snapshot(&mut sampler))
                    .is_err()
                {
                    break;
                }
                thread::sleep(AGENT_SAMPLE_INTERVAL);
            }
        });
        Self {
            snapshots,
            stop,
            join: Some(join),
        }
    }

    fn latest_snapshot(&self) -> Option<Vec<ProcessSnapshot>> {
        let mut latest = None;
        while let Ok(snapshot) = self.snapshots.try_recv() {
            latest = Some(snapshot);
        }
        latest
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct SharedRemotePane {
    pane: GuestPane,
    screen: GuestScreen,
    lease: Option<LeaseState>,
    last_lease: Instant,
    pending_control: bool,
    held_input: Vec<u8>,
    exited: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemotePaneDrain {
    Unchanged,
    Changed,
    Disconnected,
}

fn lease_allows_held_input(controller_peer_id: &[u8], peer_id: &[u8]) -> bool {
    controller_peer_id.is_empty() || controller_peer_id == peer_id
}

fn reconcile_remote_control_attempt(
    pending_control: &mut bool,
    held_input: &mut Vec<u8>,
    controller_peer_id: &[u8],
    peer_id: &[u8],
) {
    *pending_control = false;
    if !lease_allows_held_input(controller_peer_id, peer_id) {
        held_input.clear();
    }
}

impl SharedRemotePane {
    fn new(pane: GuestPane) -> Self {
        Self {
            pane,
            screen: GuestScreen::new(),
            lease: None,
            last_lease: Instant::now(),
            pending_control: false,
            held_input: Vec::new(),
            exited: false,
        }
    }

    fn view_state(&self) -> PaneViewState {
        PaneViewState {
            ready: self.screen.screen().is_some() && self.lease.is_some(),
            controller_peer_id: self
                .lease
                .as_ref()
                .map(|lease| lease.controller_peer_id.clone()),
            controller_active: self
                .lease
                .as_ref()
                .is_some_and(|lease| !lease.is_idle_at(Instant::now())),
            scrollback: 0,
        }
    }

    fn drain(&mut self) -> RemotePaneDrain {
        if self.exited {
            while self.pane.events.try_recv().is_ok() {}
            return RemotePaneDrain::Unchanged;
        }
        let mut changed = false;
        let mut received_lease = false;
        loop {
            match self.pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    if self
                        .screen
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok()
                    {
                        self.screen
                            .set_kitty_keyboard_active(snapshot.kitty_keyboard_active);
                        changed = true;
                    }
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    if self
                        .screen
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok()
                    {
                        self.screen
                            .set_kitty_keyboard_active(delta.kitty_keyboard_active);
                        changed = true;
                    }
                }
                Ok(GuestEvent::Lease(lease)) => {
                    received_lease = true;
                    self.lease = Some(LeaseState {
                        controller_peer_id: lease.controller_peer_id,
                        epoch: lease.lease_epoch,
                        last_activity: Instant::now(),
                    });
                    self.last_lease = Instant::now();
                    self.pending_control = false;
                    changed = true;
                }
                Ok(GuestEvent::ScreenGap { .. }) => {}
                Ok(GuestEvent::Disconnected)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return RemotePaneDrain::Disconnected;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            }
        }
        if received_lease && let Some(lease) = self.lease.as_ref() {
            reconcile_remote_control_attempt(
                &mut self.pending_control,
                &mut self.held_input,
                &lease.controller_peer_id,
                self.pane.controls.peer_id(),
            );
        }
        if !self.pending_control
            && !self.held_input.is_empty()
            && self.lease.as_ref().is_some_and(|lease| {
                lease_allows_held_input(&lease.controller_peer_id, self.pane.controls.peer_id())
            })
        {
            let bytes = std::mem::take(&mut self.held_input);
            if self
                .pane
                .controls
                .try_input(
                    self.lease.as_ref().expect("checked lease").epoch,
                    bytes.clone(),
                )
                .is_err()
            {
                self.held_input = bytes;
            }
        }
        if changed {
            RemotePaneDrain::Changed
        } else {
            RemotePaneDrain::Unchanged
        }
    }

    fn input(&mut self, bytes: Vec<u8>) {
        if self.exited {
            return;
        }
        let Some(lease) = self.lease.as_ref() else {
            return;
        };
        if lease.controller_peer_id == self.pane.controls.peer_id() {
            if self.held_input.is_empty() {
                let _ = self.pane.controls.try_input(lease.epoch, bytes);
            } else {
                self.held_input.extend_from_slice(&bytes);
            }
        } else if lease.controller_peer_id.is_empty() {
            let _ = self.pane.controls.try_input(lease.epoch, bytes);
        } else if self.last_lease.elapsed() >= IDLE_AFTER && !self.pending_control {
            self.held_input.extend_from_slice(&bytes);
            self.pending_control = self.pane.controls.try_take_control(lease.epoch).is_ok();
            if !self.pending_control {
                self.held_input.clear();
            }
        }
    }

    fn release_controller(&mut self) -> bool {
        let Some(lease) = self.lease.as_mut() else {
            return false;
        };
        if lease.controller_peer_id != self.pane.controls.peer_id()
            || self.pane.controls.try_release_control().is_err()
        {
            return false;
        }
        lease.controller_peer_id.clear();
        self.pending_control = false;
        self.held_input.clear();
        true
    }

    fn mark_exited(&mut self) {
        self.exited = true;
        self.pending_control = false;
        self.held_input.clear();
    }
}

enum SharedControl {
    Host(SharedLayoutHost),
    Member(SharedLayoutMember),
}

impl SharedControl {
    fn peer_id(&self) -> Vec<u8> {
        match self {
            Self::Host(host) => host.ticket().endpoint_addr().id.as_bytes().to_vec(),
            Self::Member(member) => member.peer_id.clone(),
        }
    }

    fn try_request(&self, request: LayoutRequest) -> Result<Option<CoordinatorResponse>, String> {
        match self {
            Self::Host(host) => host
                .handle_local_request(request)
                .map(Some)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_request(request)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    fn try_ready(&self, ready: PaneReady) -> Result<Option<CoordinatorResponse>, String> {
        match self {
            Self::Host(host) => host
                .handle_local_ready(ready)
                .map(Some)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_ready(ready)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    fn try_failed(&self, failed: PaneFailed) -> Result<(), String> {
        match self {
            Self::Host(host) => host
                .handle_local_failed(failed)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Self::Member(member) => member.try_failed(failed).map_err(layout_queue_message),
        }
    }

    fn try_agent_roster(&self, roster: AgentRoster) -> Result<(), String> {
        match self {
            Self::Host(host) => host
                .publish_local_agent_roster(roster)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_agent_roster(roster)
                .map_err(layout_queue_message),
        }
    }

    fn try_event(&mut self, current_revision: u64) -> Option<LayoutControlEvent> {
        match self {
            // The coordinator is the authority, so it does not receive its own broadcasts. Poll
            // its tiny in-memory snapshot to observe joins, departures, and member-originated
            // commits without adding a second internal control stream.
            Self::Host(host) => host
                .session_snapshot()
                .ok()
                .filter(|snapshot| {
                    snapshot
                        .state
                        .as_ref()
                        .is_some_and(|state| state.revision > current_revision)
                })
                .map(LayoutControlEvent::Snapshot),
            Self::Member(member) => member.events.try_recv().ok(),
        }
    }

    async fn shutdown(self) {
        match self {
            Self::Host(host) => host.close().await,
            Self::Member(member) => member.shutdown().await,
        }
    }
}

fn layout_queue_message(error: LayoutControlQueueError) -> String {
    match error {
        LayoutControlQueueError::Full => String::from("layout request queue is busy"),
        LayoutControlQueueError::Closed => String::from("layout coordinator disconnected"),
    }
}

struct PendingCreate {
    request_id: u64,
    base_revision: u64,
    grid_rows: u16,
    grid_cols: u16,
    cwd: Option<PathBuf>,
}

/// Pure retry state for direct remote-pane subscriptions. Ticks are supplied by the UI drain
/// loop, which keeps retry behavior deterministic in tests and prevents a failed connect from
/// either becoming permanent or spinning a busy loop.
#[derive(Default)]
struct RemoteSubscriptionState {
    in_flight: BTreeSet<PaneId>,
    retry_at: BTreeMap<PaneId, u64>,
    failures: BTreeMap<PaneId, u8>,
}

impl RemoteSubscriptionState {
    const MAX_BACKOFF_TICKS: u64 = 32;

    fn start(&mut self, pane_id: PaneId, tick: u64) -> bool {
        if self.in_flight.contains(&pane_id)
            || self
                .retry_at
                .get(&pane_id)
                .is_some_and(|retry_at| *retry_at > tick)
        {
            return false;
        }
        self.in_flight.insert(pane_id);
        true
    }

    fn succeeded(&mut self, pane_id: PaneId) {
        self.in_flight.remove(&pane_id);
        self.retry_at.remove(&pane_id);
        self.failures.remove(&pane_id);
    }

    fn failed(&mut self, pane_id: PaneId, tick: u64) {
        self.in_flight.remove(&pane_id);
        let failures = self.failures.entry(pane_id).or_default();
        *failures = failures.saturating_add(1);
        let delay = 1_u64
            .checked_shl(u32::from((*failures).saturating_sub(1)))
            .unwrap_or(Self::MAX_BACKOFF_TICKS)
            .min(Self::MAX_BACKOFF_TICKS);
        self.retry_at.insert(pane_id, tick.saturating_add(delay));
    }

    fn nudge(&mut self) {
        for retry_at in self.retry_at.values_mut() {
            *retry_at = 0;
        }
    }

    fn retain(&mut self, pane_ids: &BTreeSet<PaneId>) {
        self.in_flight.retain(|pane_id| pane_ids.contains(pane_id));
        self.retry_at
            .retain(|pane_id, _| pane_ids.contains(pane_id));
        self.failures
            .retain(|pane_id, _| pane_ids.contains(pane_id));
    }

    #[cfg(test)]
    fn next_retry_at(&self, pane_id: PaneId) -> Option<u64> {
        self.retry_at.get(&pane_id).copied()
    }
}

/// Blocking terminal runtime for the shared layout. Network tasks keep streams independent while
/// this loop only drains ready channels and renders the current fixed grids.
pub struct SharedLayoutRuntime {
    tui: MultiPaneTui,
    control: SharedControl,
    panes: PaneServer,
    reconciler: PaneLayoutReconciler,
    transport: Transport,
    session_id: Vec<u8>,
    runtime: tokio::runtime::Handle,
    local: BTreeMap<PaneId, SharedLocalPane>,
    remote: BTreeMap<PaneId, SharedRemotePane>,
    remote_descriptors: BTreeMap<PaneId, (EndpointAddr, PaneDescriptor)>,
    subscriptions: RemoteSubscriptionState,
    retry_tick: u64,
    subscription_tx: tokio::sync::mpsc::UnboundedSender<(PaneId, Result<GuestPane, String>)>,
    subscription_rx: tokio::sync::mpsc::UnboundedReceiver<(PaneId, Result<GuestPane, String>)>,
    pending_create: Option<PendingCreate>,
    provisional: BTreeMap<u64, PaneId>,
    pending_locks: BTreeMap<u64, (PaneId, bool)>,
    pending_exits: BTreeMap<PaneId, u64>,
    next_request_id: u64,
    status: String,
    copied_lines: Option<usize>,
    footer_notice: Option<String>,
    join_code: Option<String>,
    agent_sampler: AgentSamplingWorker,
    agent_rosters: BTreeMap<Vec<u8>, AgentRoster>,
    agent_roster_generation: u64,
    last_local_agent_entries: Vec<AgentRosterEntry>,
    next_agent_roster_heartbeat: Instant,
    last_agent_overlay_animation: Instant,
}

impl SharedLayoutRuntime {
    pub fn host(
        host: SharedLayoutHost,
        panes: PaneServer,
        snapshot: LayoutSnapshot,
        initial: SharedLocalPane,
        join_code: String,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = host.transport();
        Self::new(
            SharedControl::Host(host),
            panes,
            transport,
            snapshot,
            Some(initial),
            Some(join_code),
            runtime,
        )
    }

    pub fn member(
        member: SharedLayoutMember,
        panes: PaneServer,
        session_id: Vec<u8>,
        snapshot: LayoutSnapshot,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = member.transport();
        let mut value = Self::new(
            SharedControl::Member(member),
            panes,
            transport,
            snapshot,
            None,
            None,
            runtime,
        )?;
        value.session_id = session_id;
        Ok(value)
    }

    /// Builds a member runtime from the first authoritative snapshot. Applying the state before
    /// entering raw-terminal mode both establishes the direct-pane admission roster and starts
    /// nonblocking subscriptions for panes hosted by other members.
    pub fn member_from_state(
        member: SharedLayoutMember,
        panes: PaneServer,
        session_id: Vec<u8>,
        state: crate::protocol::LayoutState,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let snapshot = layout_snapshot_from_state(&state)
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let mut value = Self::member(member, panes, session_id, snapshot, runtime)?;
        value.apply_layout_state(&state)?;
        Ok(value)
    }

    fn new(
        control: SharedControl,
        panes: PaneServer,
        transport: Transport,
        snapshot: LayoutSnapshot,
        initial: Option<SharedLocalPane>,
        join_code: Option<String>,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let (subscription_tx, subscription_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut local = BTreeMap::new();
        if let Some(initial) = initial {
            local.insert(initial.pane_id, initial);
        }
        let session_id = Vec::new();
        let reconciler = PaneLayoutReconciler::new(panes.clone());
        let mut value = Self {
            tui: MultiPaneTui::new(snapshot)
                .map_err(|error| io::Error::other(format!("invalid layout: {error:?}")))?,
            control,
            panes,
            reconciler,
            transport,
            session_id,
            runtime,
            local,
            remote: BTreeMap::new(),
            remote_descriptors: BTreeMap::new(),
            subscriptions: RemoteSubscriptionState::default(),
            retry_tick: 0,
            subscription_tx,
            subscription_rx,
            pending_create: None,
            provisional: BTreeMap::new(),
            pending_locks: BTreeMap::new(),
            pending_exits: BTreeMap::new(),
            next_request_id: 1,
            status: String::new(),
            copied_lines: None,
            footer_notice: None,
            join_code,
            agent_sampler: AgentSamplingWorker::spawn(),
            agent_rosters: BTreeMap::new(),
            agent_roster_generation: 0,
            last_local_agent_entries: Vec::new(),
            next_agent_roster_heartbeat: Instant::now(),
            last_agent_overlay_animation: Instant::now(),
        };
        value.refresh_local_views();
        Ok(value)
    }

    pub fn set_session_id(&mut self, session_id: Vec<u8>) {
        self.session_id = session_id;
    }

    /// Node-facing non-terminal operations. Kept small while the old foreground adapter is
    /// retired so pane/Iroh ownership has exactly one home.
    pub fn drain_node(&mut self) -> Result<bool, Box<dyn Error>> {
        self.drain()
    }

    pub fn node_input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(bytes.clone())?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id) {
            pane.input(bytes);
        }
        self.tui.reset_scrollback(pane_id);
        Ok(())
    }

    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> {
        let peer_id = self.control.peer_id();
        for pane in self.local.values_mut() {
            pane.release_controller(&peer_id)?;
        }
        for pane in self.remote.values_mut() {
            pane.release_controller();
        }
        Ok(())
    }

    pub fn local_focus(&self) -> (u64, u64) {
        (self.tui.current_tab(), self.tui.focused_pane())
    }

    pub fn local_peer_id(&self) -> Vec<u8> {
        self.control.peer_id()
    }

    pub(crate) fn join_code(&self) -> Option<&str> {
        self.join_code.as_deref()
    }

    /// Current operator-facing status, empty when there is nothing to report.
    ///
    /// When the runtime drives its own terminal this is drawn directly, but under the
    /// node+client split the runtime is headless, so the node has to forward this to the
    /// attached client or the user never learns about a lost coordinator or a retrying
    /// pane.
    pub(crate) fn status(&self) -> &str {
        &self.status
    }

    /// Which network path each connected peer is actually using, and how far away it is.
    ///
    /// Travels the same headless route as `status`: the runtime holds the transport, so
    /// only it can see this, and only the client can draw it.
    pub(crate) fn peer_paths(&self) -> Vec<crate::transport::PeerPath> {
        self.transport.paths()
    }

    /// Whether this session is currently refusing new peers.
    ///
    /// Only the coordinator holds the answer, so a guest reports `false`: its own client
    /// learns the real state from the layout it is shown, not from here.
    pub(crate) fn session_locked(&self) -> bool {
        match &self.control {
            SharedControl::Host(host) => host.is_session_locked().unwrap_or(false),
            SharedControl::Member(_) => self.tui.session_locked(),
        }
    }

    fn exited_footer_notice(&self) -> Option<&'static str> {
        let pane = self.tui.snapshot().panes.get(&self.tui.focused_pane())?;
        if !pane.exited {
            return None;
        }
        if pane.host_peer_id == self.control.peer_id() {
            Some("exited — close with Ctrl+P, X")
        } else {
            Some("exited — input disabled; pane host can close with Ctrl+P, X")
        }
    }

    fn input_allowed(&self, pane_id: PaneId) -> bool {
        let peer_id = self.control.peer_id();
        self.tui
            .snapshot()
            .panes
            .get(&pane_id)
            .is_none_or(|pane| !pane.exited && (!pane.locked || pane.host_peer_id == peer_id))
    }

    /// A complete node-owned view for a newly attached local renderer.
    pub(crate) fn node_snapshot(
        &self,
    ) -> (
        LayoutSnapshot,
        NodeScreenSnapshots,
        NodeLeaseSnapshots,
        Vec<AgentOverlaySnapshotRow>,
    ) {
        let mut screens = BTreeMap::new();
        let mut chrome = BTreeMap::new();
        for (pane_id, pane) in &self.local {
            screens.insert(
                *pane_id,
                NodeScreenSnapshot::Local {
                    frame: pane.screen.current_frame().clone(),
                    history_len: pane.screen.history_metadata().0,
                    history_end: pane.screen.history_metadata().1,
                },
            );
            let view = pane.view_state();
            chrome.insert(
                *pane_id,
                (view.ready, view.controller_peer_id, view.controller_active),
            );
        }
        for (pane_id, pane) in &self.remote {
            if pane.screen.screen().is_some() {
                screens.insert(
                    *pane_id,
                    NodeScreenSnapshot::Remote {
                        sequence: pane.screen.sequence().unwrap_or(1),
                        kitty_keyboard_active: pane.screen.kitty_keyboard_active(),
                    },
                );
            }
            let view = pane.view_state();
            chrome.insert(
                *pane_id,
                (view.ready, view.controller_peer_id, view.controller_active),
            );
        }
        (
            self.tui.snapshot().clone(),
            screens,
            chrome,
            self.agent_overlay_rows()
                .iter()
                .map(AgentOverlaySnapshotRow::from)
                .collect(),
        )
    }

    pub(crate) fn node_local_scrollback(&self, pane_id: PaneId) -> Option<LocalScrollbackWindow> {
        let pane = self.local.get(&pane_id)?;
        let (total_rows, _) = pane.screen.history_metadata();
        if total_rows == 0 || pane.screen.screen().alternate_screen() {
            return None;
        }
        Some(LocalScrollbackWindow {
            total_rows,
            screen: pane.screen.screen().clone(),
        })
    }

    pub(crate) fn node_remote_snapshot(&self, pane_id: PaneId) -> Option<Vec<u8>> {
        let screen = self.remote.get(&pane_id)?.screen.screen()?;
        crate::screen::snapshot_payload(screen)
            .ok()
            .map(|snapshot| snapshot.as_ref().to_vec())
    }

    pub fn node_resize(&mut self, cols: u16, rows: u16) -> Result<(), Box<dyn Error>> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        self.reflow_local_panes(Rect::new(0, 0, cols, rows))
    }

    pub fn node_focus(&mut self, tab_id: TabId, pane_id: PaneId) -> Result<(), Box<dyn Error>> {
        let previous = self.tui.focused_pane();
        self.tui
            .set_focus(tab_id, pane_id)
            .map_err(|error| io::Error::other(format!("invalid node focus: {error:?}")))?;
        self.release_blurred_pane(previous)
    }

    pub fn node_intent(&mut self, intent: UiIntent) -> Result<(), Box<dyn Error>> {
        self.handle_intent(intent)
    }

    pub fn shutdown_node(self) {
        self.shutdown();
    }

    fn handle_key(&mut self, key: KeyEvent, area: Rect) -> Result<bool, Box<dyn Error>> {
        let previously_focused = self.tui.focused_pane();
        let quit = match self.tui.handle_key(key, area) {
            KeyHandling::Quit => Ok::<bool, Box<dyn Error>>(true),
            KeyHandling::Consumed(intents) => {
                for intent in intents {
                    self.handle_intent(intent)?;
                }
                Ok(false)
            }
            KeyHandling::Forward => {
                self.forward_key(key)?;
                Ok(false)
            }
        }?;
        self.release_blurred_pane(previously_focused)?;
        Ok(quit)
    }

    fn release_blurred_pane(&mut self, previously_focused: PaneId) -> Result<(), Box<dyn Error>> {
        if self.tui.focused_pane() == previously_focused {
            return Ok(());
        }
        self.footer_notice = None;
        let peer_id = self.control.peer_id();
        if let Some(pane) = self.local.get_mut(&previously_focused) {
            pane.release_controller(&peer_id)?;
        }
        if let Some(pane) = self.remote.get_mut(&previously_focused) {
            pane.release_controller();
        }
        Ok(())
    }

    pub fn run(mut self) -> Result<(), Box<dyn Error>> {
        let (mut cols, mut rows) = terminal::size()?;
        let mut guard = TerminalGuard::new();
        enable_raw_mode()?;
        guard.raw_mode = true;
        execute!(io::stdout(), SetTitle("p2pmux"))?;
        guard.alternate_screen = true;
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.bracketed_paste = true;
        execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
        guard.mouse_capture = true;
        execute!(io::stdout(), EnableMouseCapture)?;
        guard.keyboard_enhancement = enable_keyboard_enhancement()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;
        self.tui
            .set_agent_overlay_viewport(Rect::new(0, 0, cols, rows));
        let mut dirty = true;
        let mut last_draw: Option<Instant> = None;
        let mut pending_escape = PendingEscape::default();
        loop {
            dirty |= self.drain()?;
            if self.tui.expire_chord_mode(Instant::now()) {
                dirty = true;
            }
            if self.tui.expire_agent_toggle(Instant::now()) {
                dirty = true;
            }
            let now = Instant::now();
            if self.tui.agent_overlay_has_working_rows()
                && now.duration_since(self.last_agent_overlay_animation)
                    >= AGENT_OVERLAY_ANIMATION_INTERVAL
            {
                self.last_agent_overlay_animation = now;
                dirty = true;
            }
            if pending_escape.take_if_expired(Instant::now()) {
                if self.handle_key(
                    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                    Rect::new(0, 0, cols, rows),
                )? {
                    break;
                }
                dirty = true;
            }
            if dirty && frame_due(last_draw) {
                let mut screens = BTreeMap::new();
                for (pane_id, pane) in &self.local {
                    screens.insert(*pane_id, pane.screen.screen());
                }
                for (pane_id, pane) in &self.remote {
                    if let Some(screen) = pane.screen.screen() {
                        screens.insert(*pane_id, screen);
                    }
                }
                begin_synchronized_output()?;
                // The legacy foreground path owns the transport directly, so unlike the
                // node+client split it can read the link state without an IPC hop.
                let link = crate::transport::link_summary(&self.peer_paths());
                terminal.draw(|frame| {
                    render_shared_multi_pane(
                        frame,
                        &self.tui,
                        &screens,
                        &self.status,
                        self.copied_lines,
                        self.footer_notice
                            .as_deref()
                            .or_else(|| self.exited_footer_notice()),
                        self.join_code.as_deref(),
                        link.as_deref(),
                    );
                })?;
                end_synchronized_output()?;
                dirty = false;
                last_draw = Some(Instant::now());
            }
            if !event::poll(event_poll_timeout(dirty, last_draw))? {
                continue;
            }
            let mut quit = false;
            for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
                if self.handle_terminal_event(
                    event,
                    &mut cols,
                    &mut rows,
                    &mut pending_escape,
                    &mut dirty,
                )? {
                    quit = true;
                    break;
                }
            }
            if quit {
                break;
            }
        }
        self.shutdown();
        Ok(())
    }

    /// Applies one terminal event to the runtime. Returns true when the user quit.
    fn handle_terminal_event(
        &mut self,
        event: Event,
        cols: &mut u16,
        rows: &mut u16,
        pending_escape: &mut PendingEscape,
        dirty: &mut bool,
    ) -> Result<bool, Box<dyn Error>> {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let area = Rect::new(0, 0, *cols, *rows);
                if let Some(option_arrow) = pending_escape.take_option_arrow(key) {
                    if self.handle_key(option_arrow, area)? {
                        return Ok(true);
                    }
                } else {
                    if pending_escape.take()
                        && self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area)?
                    {
                        return Ok(true);
                    }
                    if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                        pending_escape.start(Instant::now());
                    } else if self.handle_key(key, area)? {
                        return Ok(true);
                    }
                }
                *dirty = true;
            }
            Event::Paste(text) => {
                if !self.tui.overlay_open() && !self.tui.modal_open() {
                    self.tui.exit_chord_mode();
                    self.forward_paste(&text)?;
                }
                *dirty = true;
            }
            Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Moved) => {
                if !self.tui.overlay_open() && !self.tui.modal_open() {
                    let area = Rect::new(0, 0, *cols, *rows);
                    let protocol = self.focused_pane_mouse_protocol();
                    if protocol.reports_mouse() {
                        let handling = self.tui.handle_mouse(
                            mouse,
                            area,
                            FooterMouseInput {
                                status: &self.status,
                                footer_notice: self.footer_notice.as_deref(),
                                copied_lines: self.copied_lines,
                                join_code: self.join_code.as_deref(),
                            },
                            protocol,
                        );
                        if let Some(bytes) = handling.forward_bytes {
                            self.forward_mouse(bytes)?;
                        }
                    }
                    *dirty |= self.tui.hover_pane_at(mouse.column, mouse.row, area);
                }
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_)
                ) =>
            {
                if self.tui.modal_open() {
                    return Ok(false);
                }
                let area = Rect::new(0, 0, *cols, *rows);
                let previously_focused = self.tui.focused_pane();
                let protocol = self.focused_pane_mouse_protocol();
                let handling = self.tui.handle_mouse(
                    mouse,
                    area,
                    FooterMouseInput {
                        status: &self.status,
                        footer_notice: self.footer_notice.as_deref(),
                        copied_lines: self.copied_lines,
                        join_code: self.join_code.as_deref(),
                    },
                    protocol,
                );
                if let Some(bytes) = handling.forward_bytes {
                    self.forward_mouse(bytes)?;
                }
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.copied_lines = None;
                    if self.footer_notice.as_deref() == Some("copied join command") {
                        self.footer_notice = None;
                    }
                }
                for intent in handling.intents {
                    self.handle_intent(intent)?;
                }
                if handling.copy_selection_requested {
                    self.copy_selection_to_clipboard();
                }
                if let Some(command) = handling.join_copy_command {
                    match copy_selection_to_clipboard(&command) {
                        Ok(_) => {
                            self.status.clear();
                            self.copied_lines = None;
                            self.footer_notice = Some(String::from("copied join command"));
                        }
                        Err(error) => {
                            self.status = format!("clipboard copy failed: {error}");
                        }
                    }
                }
                self.release_blurred_pane(previously_focused)?;
                *dirty = true;
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                let area = Rect::new(0, 0, *cols, *rows);
                if self.tui.modal_open() {
                    return Ok(false);
                }
                if self.tui.overlay_open() {
                    *dirty |= self
                        .tui
                        .scroll_agent_overlay(area, matches!(mouse.kind, MouseEventKind::ScrollUp));
                    return Ok(false);
                }
                // A child that reports mouse scrolls its own buffer; local scrollback
                // would otherwise hide the wheel from it.
                let protocol = self.focused_pane_mouse_protocol();
                if protocol.reports_mouse()
                    && let Some(bytes) = self
                        .tui
                        .handle_mouse(mouse, area, FooterMouseInput::default(), protocol)
                        .forward_bytes
                {
                    self.forward_mouse(bytes)?;
                    *dirty = true;
                    return Ok(false);
                }
                let pane_id = self.tui.pane_at_or_focused(mouse.column, mouse.row, area);
                let scrollback_len = self
                    .local
                    .get(&pane_id)
                    .map(|pane| available_scrollback(pane.screen.screen()))
                    .or_else(|| {
                        self.remote
                            .get(&pane_id)
                            .and_then(|pane| pane.screen.screen())
                            .map(available_scrollback)
                    })
                    .unwrap_or(0);
                *dirty |= self.tui.scroll_pane(
                    pane_id,
                    scrollback_len,
                    matches!(mouse.kind, MouseEventKind::ScrollUp),
                );
            }
            Event::Resize(width, height) => {
                if self.tui.modal_open() {
                    return Ok(false);
                }
                *cols = width;
                *rows = height;
                self.tui
                    .set_agent_overlay_viewport(Rect::new(0, 0, width, height));
                self.tui.ensure_agent_selection_visible();
                self.reflow_local_panes(Rect::new(0, 0, width, height))?;
                *dirty = true;
            }
            _ => {}
        }
        Ok(false)
    }

    fn copy_selection_to_clipboard(&mut self) {
        let Some(selection) = self.tui.selection() else {
            return;
        };
        let scrollback = self.tui.scrollback_offset(selection.pane_id);
        let text = self
            .local
            .get(&selection.pane_id)
            .and_then(|pane| {
                selection_text(&viewed_screen(pane.screen.screen(), scrollback), selection)
            })
            .or_else(|| {
                self.remote
                    .get(&selection.pane_id)
                    .and_then(|pane| pane.screen.screen())
                    .and_then(|screen| {
                        selection_text(&viewed_screen(screen, scrollback), selection)
                    })
            });
        let Some(text) = text else {
            return;
        };
        match copy_selection_to_clipboard(&text) {
            Ok(lines) => {
                self.status.clear();
                self.copied_lines = Some(lines);
            }
            Err(error) => {
                self.copied_lines = None;
                self.status = format!("clipboard copy failed: {error}");
            }
        }
    }

    fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut changed = false;
        self.retry_tick = self.retry_tick.saturating_add(1);
        while let Some(event) = self.control.try_event(self.tui.snapshot().revision) {
            self.handle_control_event(event)?;
            changed = true;
        }
        while let Ok((pane_id, result)) = self.subscription_rx.try_recv() {
            match result {
                Ok(pane) => {
                    if self.remote_descriptors.contains_key(&pane_id) {
                        self.subscriptions.succeeded(pane_id);
                        self.remote.insert(pane_id, SharedRemotePane::new(pane));
                    } else {
                        self.spawn_remote_shutdown(pane);
                    }
                }
                Err(error) => {
                    self.subscriptions.failed(pane_id, self.retry_tick);
                    self.status = format!("pane {pane_id}: {error}; retrying");
                }
            }
            changed = true;
        }
        self.start_eligible_subscriptions();
        for pane in self.local.values_mut() {
            let drained = pane.drain()?;
            changed |= drained.changed;
            if drained.newly_exited {
                self.pending_exits.entry(pane.pane_id).or_insert(0);
            }
        }
        self.send_pending_exit_marks()?;
        if let Some(snapshot) = self.agent_sampler.latest_snapshot() {
            let now = Instant::now();
            for pane in self.local.values_mut() {
                if !pane.exited {
                    changed |= pane.apply_agent_snapshot(&snapshot, now);
                }
            }
        }
        changed |= self.publish_local_agent_roster();
        let disconnected = self
            .remote
            .iter_mut()
            .filter_map(|(pane_id, pane)| match pane.drain() {
                RemotePaneDrain::Unchanged => None,
                RemotePaneDrain::Changed => {
                    changed = true;
                    None
                }
                RemotePaneDrain::Disconnected => Some(*pane_id),
            })
            .collect::<Vec<_>>();
        for pane_id in disconnected {
            if let Some(pane) = self.remote.remove(&pane_id) {
                self.spawn_remote_shutdown(pane.pane);
            }
            if self.remote_descriptors.contains_key(&pane_id) {
                self.subscriptions.failed(pane_id, self.retry_tick);
                self.status = format!("pane {pane_id} disconnected; retrying");
            }
            changed = true;
        }
        changed |= self.refresh_local_views();
        changed |= self.refresh_agent_rows();
        Ok(changed)
    }

    fn spawn_remote_shutdown(&self, pane: GuestPane) {
        self.runtime.spawn(async move { pane.shutdown().await });
    }

    fn refresh_local_views(&mut self) -> bool {
        let mut changed = false;
        for (pane_id, pane) in &self.local {
            changed |= self.tui.set_pane_view(*pane_id, pane.view_state());
        }
        for (pane_id, pane) in &self.remote {
            changed |= self.tui.set_pane_view(*pane_id, pane.view_state());
        }
        changed
    }

    fn publish_local_agent_roster(&mut self) -> bool {
        let now = Instant::now();
        let entries = self
            .local
            .values_mut()
            .filter(|pane| !pane.exited)
            .filter_map(|pane| pane.agent_roster_entry(now))
            .collect::<Vec<_>>();
        if entries == self.last_local_agent_entries && now < self.next_agent_roster_heartbeat {
            return false;
        }
        self.agent_roster_generation = self.agent_roster_generation.saturating_add(1);
        let roster = AgentRoster {
            host_peer_id: self.control.peer_id(),
            generation: self.agent_roster_generation,
            entries: entries.clone(),
        };
        if self.control.try_agent_roster(roster.clone()).is_err() {
            return false;
        }
        self.last_local_agent_entries = entries;
        self.next_agent_roster_heartbeat = now + Duration::from_secs(5);
        self.agent_rosters
            .insert(roster.host_peer_id.clone(), roster);
        true
    }

    fn refresh_agent_rows(&mut self) -> bool {
        self.tui.set_agent_rows(self.agent_overlay_rows())
    }

    pub fn agent_overlay_rows(&self) -> Vec<AgentOverlayRow> {
        let pane_locations =
            self.tui
                .snapshot()
                .tabs
                .iter()
                .enumerate()
                .flat_map(|(tab_index, tab)| {
                    visible_leaf_panes(&tab.root).into_iter().enumerate().map(
                        move |(pane_index, pane_id)| (pane_id, (tab_index + 1, pane_index + 1)),
                    )
                })
                .collect::<BTreeMap<_, _>>();
        self.agent_rosters
            .values()
            .flat_map(|roster| {
                roster.entries.iter().filter_map(|entry| {
                    let pane = self.tui.snapshot().panes.get(&entry.pane_id)?;
                    if pane.exited {
                        return None;
                    }
                    let view = self.tui.pane_view(entry.pane_id)?;
                    let &(tab_ordinal, pane_ordinal) = pane_locations.get(&entry.pane_id)?;
                    let tab = self
                        .tui
                        .snapshot()
                        .tabs
                        .iter()
                        .find(|tab| contains_leaf(&tab.root, entry.pane_id))?;
                    let host = sanitize_single_line(&member_label(
                        &pane.host_peer_id,
                        &self.tui.snapshot().members,
                    ));
                    let controller = view
                        .controller_peer_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .map(|id| {
                            sanitize_single_line(&member_label(id, &self.tui.snapshot().members))
                        })
                        .unwrap_or_else(|| String::from("free"));
                    Some(AgentOverlayRow {
                        pane_id: entry.pane_id,
                        tab_ordinal,
                        pane_ordinal,
                        tab_label: tab
                            .title
                            .clone()
                            .unwrap_or_else(|| format!("Tab #{tab_ordinal}")),
                        pane_label: pane
                            .title
                            .clone()
                            .unwrap_or_else(|| format!("Pane #{pane_ordinal}")),
                        kind: sanitize_single_line(&entry.agent_kind),
                        cwd: sanitize_single_line(&entry.cwd),
                        state: AgentRosterState::from_wire(entry.state),
                        working_since_unix_ms: entry.working_since_unix_ms,
                        host,
                        controller,
                    })
                })
            })
            .collect()
    }

    fn handle_control_event(&mut self, event: LayoutControlEvent) -> Result<(), Box<dyn Error>> {
        self.reconciler.apply(&event)?;
        match event {
            LayoutControlEvent::Snapshot(snapshot) => {
                self.apply_layout_state(snapshot.state.as_ref().ok_or("missing layout state")?)?;
            }
            LayoutControlEvent::AgentRoster(roster) => {
                self.agent_rosters
                    .insert(roster.host_peer_id.clone(), roster);
            }
            LayoutControlEvent::Commit(commit) => {
                self.apply_layout_state(commit.state.as_ref().ok_or("missing layout state")?)?;
            }
            LayoutControlEvent::Reservation(reservation) => self.accept_reservation(reservation)?,
            LayoutControlEvent::Reject(reject) => self.reject_request(reject.request_id),
            LayoutControlEvent::Disconnected => {
                self.status = String::from("layout coordinator disconnected")
            }
        }
        Ok(())
    }

    fn apply_layout_state(
        &mut self,
        state: &crate::protocol::LayoutState,
    ) -> Result<(), Box<dyn Error>> {
        let snapshot = layout_snapshot_from_state(state)
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let current_ids = snapshot.panes.keys().copied().collect::<BTreeSet<_>>();
        let prior_revision = self.tui.snapshot().revision;
        self.agent_rosters.retain(|host, roster| {
            roster.entries.retain(|entry| {
                snapshot
                    .panes
                    .get(&entry.pane_id)
                    .is_some_and(|pane| pane.host_peer_id == *host)
            });
            snapshot
                .members
                .iter()
                .any(|member| member.peer_id == *host)
        });
        // A successful authoritative commit is the only point at which a provisional local PTY
        // becomes a real pane. Forget the request bookkeeping then; rejection handles the other
        // path and tears the provisional PTY down.
        self.provisional
            .retain(|_, pane_id| !current_ids.contains(pane_id));
        let local_ids = self.local.keys().copied().collect::<Vec<_>>();
        for pane_id in local_ids {
            if !current_ids.contains(&pane_id) {
                let _ = self.panes.remove_local_pane(pane_id)?;
                if let Some(mut pane) = self.local.remove(&pane_id) {
                    pane.shutdown()?;
                }
            }
        }
        for (pane_id, pane) in &snapshot.panes {
            if let Some(local) = self.local.get_mut(pane_id) {
                local.set_locked(pane.locked)?;
                if pane.exited {
                    local.mark_exited()?;
                }
            }
            if pane.exited
                && let Some(remote) = self.remote.get_mut(pane_id)
            {
                remote.mark_exited();
            }
        }
        self.pending_locks.retain(|_, (pane_id, locked)| {
            snapshot
                .panes
                .get(pane_id)
                .is_some_and(|pane| pane.locked != *locked)
        });
        self.pending_exits
            .retain(|pane_id, _| snapshot.panes.get(pane_id).is_some_and(|pane| !pane.exited));
        let remote_ids = self.remote.keys().copied().collect::<Vec<_>>();
        for pane_id in remote_ids {
            if !current_ids.contains(&pane_id)
                && let Some(pane) = self.remote.remove(&pane_id)
            {
                self.spawn_remote_shutdown(pane.pane);
            }
        }
        let previously_focused = self.tui.focused_pane();
        self.tui
            .apply_snapshot(snapshot.clone())
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        self.release_blurred_pane(previously_focused)?;
        let me = self.control.peer_id();
        self.remote_descriptors.clear();
        for pane in state
            .panes
            .iter()
            .filter(|pane| pane.host_peer_id != me && !pane.exited)
        {
            let endpoint = state
                .members
                .iter()
                .find(|member| member.peer_id == pane.host_peer_id)
                .and_then(|member| serde_json::from_slice(&member.endpoint_addr).ok());
            let Some(endpoint) = endpoint else {
                self.status = format!("pane {} has no usable host address", pane.pane_id);
                continue;
            };
            self.remote_descriptors
                .insert(pane.pane_id, (endpoint, pane.clone()));
        }
        let remote_ids = self
            .remote_descriptors
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        self.subscriptions.retain(&remote_ids);
        self.subscriptions.nudge();
        self.start_eligible_subscriptions();
        self.refresh_local_views();
        if let Some(area) = area_from_terminal_size(terminal::size()) {
            self.reflow_local_panes(area)?;
        }
        if snapshot.revision > prior_revision {
            self.send_pending_exit_marks()?;
        }
        Ok(())
    }

    fn reflow_local_panes(&mut self, area: Rect) -> Result<(), Box<dyn Error>> {
        let geometry = self.tui.geometry(area);
        let mut updates = Vec::new();
        for (pane_id, rect) in geometry.panes {
            let Some(pane) = self.local.get_mut(&pane_id) else {
                continue;
            };
            if pane.exited
                || self
                    .tui
                    .snapshot()
                    .panes
                    .get(&pane_id)
                    .is_some_and(|descriptor| descriptor.exited)
            {
                continue;
            }
            let (rows, cols) = grid_for_pane(rect);
            if pane.screen.screen().size() == (rows, cols) {
                continue;
            }
            pane.resize(rows, cols)?;
            if self
                .tui
                .snapshot()
                .panes
                .get(&pane_id)
                .is_some_and(|descriptor| {
                    (descriptor.grid_rows, descriptor.grid_cols) != (rows, cols)
                })
            {
                updates.push(crate::protocol::PaneGrid {
                    pane_id,
                    grid_rows: u32::from(rows),
                    grid_cols: u32::from(cols),
                });
            }
        }
        if !updates.is_empty() {
            let request_id = self.next_id();
            self.send_request(LayoutRequest {
                request_id,
                base_revision: self.tui.snapshot().revision,
                create_pane: None,
                delete_pane: None,
                create_tab: None,
                delete_tab: None,
                set_split_ratio: None,
                update_pane_grids: Some(crate::protocol::UpdatePaneGrids { panes: updates }),

                rename_pane: None,
                rename_tab: None,
                set_pane_lock: None,
                mark_pane_exited: None,
            })?;
        }
        Ok(())
    }

    fn start_eligible_subscriptions(&mut self) {
        for (pane_id, (endpoint, descriptor)) in self.remote_descriptors.clone() {
            if self.remote.contains_key(&pane_id)
                || !self.subscriptions.start(pane_id, self.retry_tick)
            {
                continue;
            }
            let tx = self.subscription_tx.clone();
            let transport = self.transport.clone();
            let session_id = self.session_id.clone();
            self.runtime.spawn(async move {
                let result = subscribe_pane(transport, session_id, endpoint, descriptor)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send((pane_id, result));
            });
        }
    }

    fn send_pending_exit_marks(&mut self) -> Result<(), Box<dyn Error>> {
        let revision = self.tui.snapshot().revision;
        let pane_ids = self
            .pending_exits
            .iter()
            .filter_map(|(pane_id, attempted_revision)| {
                (*attempted_revision < revision).then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            let request_id = self.next_id();
            self.pending_exits.insert(pane_id, revision);
            self.send_request(LayoutRequest {
                request_id,
                base_revision: revision,
                create_pane: None,
                delete_pane: None,
                create_tab: None,
                delete_tab: None,
                set_split_ratio: None,
                update_pane_grids: None,
                rename_pane: None,
                rename_tab: None,
                set_pane_lock: None,
                mark_pane_exited: Some(MarkPaneExited { pane_id }),
            })?;
        }
        Ok(())
    }

    fn handle_intent(&mut self, intent: UiIntent) -> Result<(), Box<dyn Error>> {
        match intent {
            UiIntent::CreatePane {
                target_pane_id,
                axis,
                position,
                grid_rows,
                grid_cols,
            } => {
                let cwd = self
                    .local
                    .get(&target_pane_id)
                    .and_then(|pane| pane.host.process_id())
                    .and_then(cwd_for_pid);
                self.begin_create(
                    Some((target_pane_id, axis, position)),
                    grid_rows,
                    grid_cols,
                    cwd,
                )?;
            }
            UiIntent::CreateTab {
                grid_rows,
                grid_cols,
            } => {
                self.begin_create(None, grid_rows, grid_cols, None)?;
            }
            UiIntent::DeletePane { pane_id } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: Some(DeletePane { pane_id }),
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: None,
                    update_pane_grids: None,

                    rename_pane: None,
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                })?
            }
            UiIntent::DeleteTab { tab_id } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: Some(DeleteTab { tab_id }),
                    set_split_ratio: None,
                    update_pane_grids: None,

                    rename_pane: None,
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                })?
            }
            UiIntent::SetSplitRatio {
                pane_id,
                axis,
                first_share_bps,
                base_revision,
            } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: Some(crate::protocol::SetSplitRatio {
                        pane_id,
                        axis: Some(match axis {
                            Axis::LeftRight => SplitAxis::LeftRight as i32,
                            Axis::TopBottom => SplitAxis::TopBottom as i32,
                        }),
                        first_share_bps: u32::from(first_share_bps),
                    }),
                    update_pane_grids: None,
                    rename_pane: None,
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                })?;
            }
            UiIntent::RenamePane { pane_id, title } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: None,
                    update_pane_grids: None,
                    rename_pane: Some(RenamePane { pane_id, title }),
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                })?;
            }
            UiIntent::RenameTab { tab_id, title } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: None,
                    update_pane_grids: None,
                    rename_pane: None,
                    rename_tab: Some(RenameTab { tab_id, title }),
                    set_pane_lock: None,
                    mark_pane_exited: None,
                })?;
            }
            UiIntent::SetPaneLock { pane_id, locked } => {
                self.set_pane_lock(pane_id, locked)?;
            }
            UiIntent::SetSessionLock { locked } => {
                self.set_session_lock(locked)?;
            }
            UiIntent::FocusPane { .. } | UiIntent::SwitchTab { .. } => {}
        }
        Ok(())
    }

    /// Close or reopen the session to newcomers.
    ///
    /// Refused for a guest rather than forwarded: the coordinator is the only peer that
    /// answers joins, so a guest "locking" would change nothing while looking like it had.
    fn set_session_lock(&mut self, locked: bool) -> Result<(), Box<dyn Error>> {
        let SharedControl::Host(host) = &self.control else {
            self.status = String::from("only the session host can lock this session");
            return Ok(());
        };
        let locked = host.set_session_lock(locked)?;
        self.tui.set_session_locked(locked);
        self.status = String::from(if locked {
            "session locked — new peers are refused"
        } else {
            "session unlocked — anyone with the ticket can join"
        });
        Ok(())
    }

    fn begin_create(
        &mut self,
        pane: Option<(PaneId, Axis, NewPanePosition)>,
        grid_rows: u16,
        grid_cols: u16,
        cwd: Option<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        if self.pending_create.is_some() {
            self.status = String::from("waiting for current pane reservation");
            return Ok(());
        }
        let request_id = self.next_id();
        let base_revision = self.tui.snapshot().revision;
        self.pending_create = Some(PendingCreate {
            request_id,
            base_revision,
            grid_rows,
            grid_cols,
            cwd,
        });
        self.send_request(LayoutRequest {
            request_id,
            base_revision,
            create_pane: pane.map(|(target_pane_id, axis, position)| CreatePane {
                target_pane_id,
                axis: Some(match axis {
                    Axis::LeftRight => SplitAxis::LeftRight as i32,
                    Axis::TopBottom => SplitAxis::TopBottom as i32,
                }),
                grid_rows: u32::from(grid_rows),
                grid_cols: u32::from(grid_cols),
                position: Some(match position {
                    NewPanePosition::First => ProtocolNewPanePosition::First as i32,
                    NewPanePosition::Second => ProtocolNewPanePosition::Second as i32,
                }),
            }),
            delete_pane: None,
            create_tab: pane.is_none().then_some(CreateTab {
                grid_rows: u32::from(grid_rows),
                grid_cols: u32::from(grid_cols),
            }),
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,
            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
        })
    }

    fn set_pane_lock(&mut self, pane_id: PaneId, locked: bool) -> Result<(), Box<dyn Error>> {
        let peer_id = self.control.peer_id();
        if self
            .tui
            .snapshot()
            .panes
            .get(&pane_id)
            .is_none_or(|pane| pane.host_peer_id != peer_id)
        {
            self.footer_notice = Some(String::from("only the pane host can lock it"));
            return Ok(());
        }
        let Some(pane) = self.local.get_mut(&pane_id) else {
            self.footer_notice = Some(String::from("pane host is unavailable"));
            return Ok(());
        };
        let previous = pane.locked;
        pane.set_locked(locked)?;
        let request_id = self.next_id();
        self.pending_locks.insert(request_id, (pane_id, previous));
        let request = LayoutRequest {
            request_id,
            base_revision: self.tui.snapshot().revision,
            create_pane: None,
            delete_pane: None,
            create_tab: None,
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,
            rename_pane: None,
            rename_tab: None,
            set_pane_lock: Some(SetPaneLock { pane_id, locked }),
            mark_pane_exited: None,
        };
        if let Err(error) = self.send_request(request) {
            self.pending_locks.remove(&request_id);
            if let Some(pane) = self.local.get_mut(&pane_id) {
                pane.set_locked(previous)?;
            }
            return Err(error);
        }
        Ok(())
    }

    fn send_request(&mut self, request: LayoutRequest) -> Result<(), Box<dyn Error>> {
        if let Some(response) = self.control.try_request(request)? {
            match response {
                CoordinatorResponse::Reservation(reservation) => {
                    self.accept_reservation(reservation)?
                }
                CoordinatorResponse::Commit(commit) => {
                    self.handle_control_event(LayoutControlEvent::Commit(commit))?
                }
                CoordinatorResponse::Reject(reject) => self.reject_request(reject.request_id),
            }
        }
        Ok(())
    }

    fn accept_reservation(
        &mut self,
        reservation: crate::protocol::PaneReservation,
    ) -> Result<(), Box<dyn Error>> {
        let Some(pending) = self.pending_create.take() else {
            self.status = String::from("unexpected pane reservation");
            return Ok(());
        };
        let host_peer_id = self.control.peer_id();
        let pane = match SharedLocalPane::spawn_with_cwd(
            reservation.pane_id,
            pending.grid_rows,
            pending.grid_cols,
            host_peer_id.clone(),
            pending.cwd.as_deref(),
        ) {
            Ok(pane) => pane,
            Err(error) => {
                let _ = self.control.try_failed(PaneFailed {
                    reservation_id: reservation.reservation_id,
                    request_id: pending.request_id,
                    base_revision: pending.base_revision,
                });
                self.status = format!("pane spawn failed: {error}");
                return Ok(());
            }
        };
        let descriptor = PaneDescriptor {
            pane_id: reservation.pane_id,
            host_peer_id,
            grid_rows: u32::from(pending.grid_rows),
            grid_cols: u32::from(pending.grid_cols),
            title: None,
            locked: false,
            exited: false,
        };
        if let Err(error) = self.panes.register_local_pane(descriptor, pane.channels()) {
            let _ = self.control.try_failed(PaneFailed {
                reservation_id: reservation.reservation_id,
                request_id: pending.request_id,
                base_revision: pending.base_revision,
            });
            self.status = format!("pane registration failed: {error}");
            return Ok(());
        }
        self.provisional
            .insert(pending.request_id, reservation.pane_id);
        self.local.insert(reservation.pane_id, pane);
        if let Some(tab_id) = reservation.tab_id {
            self.tui.select_created_tab(tab_id);
        } else {
            self.tui.select_created_pane(reservation.pane_id);
        }
        match self.control.try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            request_id: pending.request_id,
            base_revision: pending.base_revision,
        })? {
            Some(CoordinatorResponse::Commit(commit)) => {
                self.handle_control_event(LayoutControlEvent::Commit(commit))?
            }
            Some(CoordinatorResponse::Reject(reject)) => self.reject_request(reject.request_id),
            Some(CoordinatorResponse::Reservation(_)) | None => {}
        }
        Ok(())
    }

    fn reject_request(&mut self, request_id: u64) {
        self.tui.cancel_resize_drag();
        self.pending_create = self
            .pending_create
            .take()
            .filter(|pending| pending.request_id != request_id);
        if let Some(pane_id) = self.provisional.remove(&request_id) {
            let _ = self.panes.remove_local_pane(pane_id);
            if let Some(mut pane) = self.local.remove(&pane_id) {
                let _ = pane.shutdown();
            }
        }
        if let Some((pane_id, previous)) = self.pending_locks.remove(&request_id)
            && let Some(pane) = self.local.get_mut(&pane_id)
        {
            let _ = pane.set_locked(previous);
        }
        self.footer_notice = Some(format!("layout request {request_id} rejected"));
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1).max(1);
        id
    }

    /// The mouse reporting the focused pane's child has turned on, if any.
    fn focused_pane_mouse_protocol(&self) -> PaneMouseProtocol {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return PaneMouseProtocol::default();
        }
        self.local
            .get(&pane_id)
            .map(|pane| PaneMouseProtocol::from_screen(pane.screen.screen()))
            .or_else(|| {
                self.remote
                    .get(&pane_id)
                    .and_then(|pane| pane.screen.screen())
                    .map(PaneMouseProtocol::from_screen)
            })
            .unwrap_or_default()
    }

    fn forward_mouse(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(bytes.clone())?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id) {
            pane.input(bytes);
        }
        Ok(())
    }

    fn forward_key(&mut self, key: KeyEvent) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        let mut sent = false;
        if let Some(pane) = self.local.get_mut(&pane_id)
            && let Some(bytes) = encode_key(
                key,
                pane.screen.screen(),
                pane.screen.kitty_keyboard_active(),
            )
        {
            pane.input(bytes)?;
            sent = true;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id)
            && let Some(screen) = pane.screen.screen()
            && let Some(bytes) = encode_key(key, screen, pane.screen.kitty_keyboard_active())
        {
            pane.input(bytes);
            sent = true;
        }
        if sent {
            self.tui.reset_scrollback(pane_id);
        }
        Ok(())
    }

    fn forward_paste(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(encode_paste(text, pane.screen.screen().bracketed_paste()))?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id)
            && let Some(screen) = pane.screen.screen()
        {
            pane.input(encode_paste(text, screen.bracketed_paste()));
        }
        self.tui.reset_scrollback(pane_id);
        Ok(())
    }

    fn shutdown(mut self) {
        self.agent_sampler.shutdown();
        for (_, mut pane) in std::mem::take(&mut self.local) {
            let _ = self.panes.remove_local_pane(pane.pane_id);
            let _ = pane.shutdown();
        }
        for (_, pane) in std::mem::take(&mut self.remote) {
            self.runtime.block_on(pane.pane.shutdown());
        }
        self.runtime.block_on(self.control.shutdown());
    }
}

fn area_from_terminal_size(size: io::Result<(u16, u16)>) -> Option<Rect> {
    size.ok()
        .map(|(width, height)| Rect::new(0, 0, width, height))
}

pub struct HostPaneRuntime {
    host: PtyHost,
    screen: HostScreen,
    lease: LeaseManager,
    host_peer_id: Vec<u8>,
    screen_tx: watch::Sender<ScreenFrame>,
    lease_tx: watch::Sender<LeaseState>,
    control_rx: mpsc::Receiver<HostControlEvent>,
    join_code: String,
}

impl HostPaneRuntime {
    pub fn new(
        size: PtySize,
        host_peer_id: Vec<u8>,
        screen_tx: watch::Sender<ScreenFrame>,
        lease_tx: watch::Sender<LeaseState>,
        control_rx: mpsc::Receiver<HostControlEvent>,
        join_code: String,
    ) -> Result<Self, Box<dyn Error>> {
        let screen = HostScreen::new(size.rows, size.cols)?;
        let lease = LeaseManager::new(Vec::new(), Instant::now());
        lease_tx.send_replace(lease.state().clone());
        Ok(Self {
            host: PtyHost::spawn_default_shell(size)?,
            screen,
            lease,
            host_peer_id,
            screen_tx,
            lease_tx,
            control_rx,
            join_code,
        })
    }
}

struct VtScreen<'a> {
    screen: &'a vt100::Screen,
    selection: Option<PaneTextSelection>,
}

impl<'a> VtScreen<'a> {
    fn new(screen: &'a vt100::Screen) -> Self {
        Self {
            screen,
            selection: None,
        }
    }

    fn with_selection(mut self, selection: Option<PaneTextSelection>) -> Self {
        self.selection = selection;
        self
    }
}

fn viewed_screen(screen: &vt100::Screen, scrollback: usize) -> vt100::Screen {
    let mut screen = screen.clone();
    screen.set_scrollback(scrollback);
    screen
}

fn available_scrollback(screen: &vt100::Screen) -> usize {
    let mut screen = screen.clone();
    screen.set_scrollback(SCROLLBACK_LINES);
    screen.scrollback()
}

impl Widget for VtScreen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        for row in 0..rows.min(area.height) {
            for col in 0..cols.min(area.width) {
                let Some(source) = self.screen.cell(row, col) else {
                    continue;
                };
                if source.is_wide_continuation() {
                    continue;
                }
                let target = &mut buf[(area.x + col, area.y + row)];
                let contents = source.contents();
                target.set_symbol(if contents.is_empty() { " " } else { contents });
                let style = if self
                    .selection
                    .is_some_and(|selection| selection.contains(ScreenCell { row, col }))
                {
                    vt_style(source)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::REVERSED)
                } else {
                    vt_style(source)
                };
                target.set_style(style);
            }
        }
    }
}

fn vt_style(cell: &vt100::Cell) -> Style {
    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers.insert(Modifier::BOLD);
    }
    if cell.dim() {
        modifiers.insert(Modifier::DIM);
    }
    if cell.italic() {
        modifiers.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        modifiers.insert(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        modifiers.insert(Modifier::REVERSED);
    }
    Style::default()
        .fg(vt_color(cell.fgcolor()))
        .bg(vt_color(cell.bgcolor()))
        .add_modifier(modifiers)
}

fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn render_guest_screen(frame: &mut Frame<'_>, screen: &vt100::Screen, footer: &str) {
    let area = frame.area();
    let screen_height = screen.size().0.min(area.height.saturating_sub(1));
    let screen_area = Rect::new(area.x, area.y, area.width, screen_height);
    frame.render_widget(VtScreen::new(screen), screen_area);
    let (row, col) = screen.cursor_position();
    if !screen.hide_cursor() && row < screen_height && col < area.width {
        frame.set_cursor_position((area.x + col, area.y + row));
    }
    if area.height > 0 {
        let footer_y = area.y + screen_area.height;
        frame
            .buffer_mut()
            .set_string(area.x, footer_y, footer, Style::default());
    }
}

fn render_host_screen(frame: &mut Frame<'_>, screen: &vt100::Screen, footer: &str) {
    render_guest_screen(frame, screen, footer);
}

fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL
}

fn encode_key(
    key: KeyEvent,
    screen: &vt100::Screen,
    kitty_keyboard_active: bool,
) -> Option<Vec<u8>> {
    if is_quit(key) {
        return None;
    }

    let modifiers = modifier_parameter(key.modifiers)?;
    let bytes = match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let character = character.to_ascii_lowercase();
            if !character.is_ascii_lowercase() {
                return None;
            }
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            bytes.push(character as u8 - b'a' + 1);
            bytes
        }
        KeyCode::Char(character) => {
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            bytes.extend(character.to_string().bytes());
            bytes
        }
        KeyCode::Enter if modifiers == 1 => b"\r".to_vec(),
        // Use kitty CSI-u when the child opted in; otherwise LF is the newline fallback.
        KeyCode::Enter if modifiers == 2 => {
            if kitty_keyboard_active {
                b"\x1b[13;2u".to_vec()
            } else {
                b"\n".to_vec()
            }
        }
        KeyCode::Tab if modifiers == 1 => b"\t".to_vec(),
        KeyCode::BackTab if modifiers == 2 => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if modifiers == 1 => b"\x7f".to_vec(),
        KeyCode::Esc if modifiers == 1 => b"\x1b".to_vec(),
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => {
            let suffix = match key.code {
                KeyCode::Up => b'A',
                KeyCode::Down => b'B',
                KeyCode::Right => b'C',
                KeyCode::Left => b'D',
                _ => unreachable!(),
            };
            if modifiers == 1 && screen.application_cursor() {
                vec![0x1b, b'O', suffix]
            } else if modifiers == 1 {
                vec![0x1b, b'[', suffix]
            } else {
                format!("\x1b[1;{modifiers}{}", suffix as char).into_bytes()
            }
        }
        KeyCode::Home if modifiers == 1 => b"\x1b[H".to_vec(),
        KeyCode::End if modifiers == 1 => b"\x1b[F".to_vec(),
        KeyCode::Delete if modifiers == 1 => b"\x1b[3~".to_vec(),
        KeyCode::Insert if modifiers == 1 => b"\x1b[2~".to_vec(),
        KeyCode::PageUp if modifiers == 1 => b"\x1b[5~".to_vec(),
        KeyCode::PageDown if modifiers == 1 => b"\x1b[6~".to_vec(),
        KeyCode::F(number) if modifiers == 1 => function_key(number)?,
        _ => return None,
    };
    Some(bytes)
}

fn modifier_parameter(modifiers: KeyModifiers) -> Option<u8> {
    let supported = KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL;
    if !(modifiers - supported).is_empty() {
        return None;
    }
    let mut parameter = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        parameter += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parameter += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        parameter += 4;
    }
    Some(parameter)
}

fn function_key(number: u8) -> Option<Vec<u8>> {
    let bytes = match number {
        1 => b"\x1bOP".as_slice(),
        2 => b"\x1bOQ".as_slice(),
        3 => b"\x1bOR".as_slice(),
        4 => b"\x1bOS".as_slice(),
        5 => b"\x1b[15~".as_slice(),
        6 => b"\x1b[17~".as_slice(),
        7 => b"\x1b[18~".as_slice(),
        8 => b"\x1b[19~".as_slice(),
        9 => b"\x1b[20~".as_slice(),
        10 => b"\x1b[21~".as_slice(),
        11 => b"\x1b[23~".as_slice(),
        12 => b"\x1b[24~".as_slice(),
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn encode_paste(text: &str, bracketed_paste: bool) -> Vec<u8> {
    if bracketed_paste {
        [
            b"\x1b[200~".as_slice(),
            text.as_bytes(),
            b"\x1b[201~".as_slice(),
        ]
        .concat()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Low two bits of an xterm button code, by button.
const MOUSE_BUTTON_LEFT: u8 = 0;
const MOUSE_BUTTON_MIDDLE: u8 = 1;
const MOUSE_BUTTON_RIGHT: u8 = 2;
/// Legacy encodings report every release with the button bits set.
const MOUSE_BUTTON_RELEASE: u8 = 3;
const MOUSE_MOTION_FLAG: u8 = 32;
const MOUSE_WHEEL_FLAG: u8 = 64;
const MOUSE_SHIFT_FLAG: u8 = 4;
const MOUSE_ALT_FLAG: u8 = 8;
const MOUSE_CONTROL_FLAG: u8 = 16;
/// Legacy button/coordinate bytes are biased so they land in printable ASCII.
const MOUSE_LEGACY_BIAS: u16 = 32;
/// The largest coordinate a single biased byte can carry.
const MOUSE_LEGACY_MAX: u16 = 223;

/// The xterm mouse reporting a pane's child has asked for, if any.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneMouseProtocol {
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
}

impl PaneMouseProtocol {
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        Self {
            mode: screen.mouse_protocol_mode(),
            encoding: screen.mouse_protocol_encoding(),
        }
    }

    /// Whether the child wants mouse events at all; when false the mux keeps them.
    pub fn reports_mouse(self) -> bool {
        self.mode != vt100::MouseProtocolMode::None
    }
}

fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => MOUSE_BUTTON_LEFT,
        MouseButton::Middle => MOUSE_BUTTON_MIDDLE,
        MouseButton::Right => MOUSE_BUTTON_RIGHT,
    }
}

fn mouse_modifier_flags(modifiers: KeyModifiers) -> u8 {
    let mut flags = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        flags |= MOUSE_SHIFT_FLAG;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        flags |= MOUSE_ALT_FLAG;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        flags |= MOUSE_CONTROL_FLAG;
    }
    flags
}

/// Encodes a pane-local mouse event the way the child asked to receive it.
///
/// Returns `None` when the child's [`vt100::MouseProtocolMode`] does not subscribe to
/// this event, so callers can fall back to mux-local handling.
fn encode_mouse(
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    cell: ScreenCell,
    protocol: PaneMouseProtocol,
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode as Mode;

    if !protocol.reports_mouse() {
        return None;
    }
    let (button, release) = match kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), false),
        MouseEventKind::Up(button) => {
            // X10 mode reports presses only.
            if protocol.mode == Mode::Press {
                return None;
            }
            (mouse_button_code(button), true)
        }
        MouseEventKind::Drag(button) => {
            if !matches!(protocol.mode, Mode::ButtonMotion | Mode::AnyMotion) {
                return None;
            }
            (mouse_button_code(button) | MOUSE_MOTION_FLAG, false)
        }
        MouseEventKind::Moved => {
            if protocol.mode != Mode::AnyMotion {
                return None;
            }
            (MOUSE_BUTTON_RELEASE | MOUSE_MOTION_FLAG, false)
        }
        // Wheel reports are presses without a matching release in every mode.
        MouseEventKind::ScrollUp => (MOUSE_WHEEL_FLAG, false),
        MouseEventKind::ScrollDown => (MOUSE_WHEEL_FLAG | 1, false),
        MouseEventKind::ScrollLeft => (MOUSE_WHEEL_FLAG | 2, false),
        MouseEventKind::ScrollRight => (MOUSE_WHEEL_FLAG | 3, false),
    };
    let code = button | mouse_modifier_flags(modifiers);
    let column = cell.col.saturating_add(1);
    let row = cell.row.saturating_add(1);

    Some(match protocol.encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let final_byte = if release { 'm' } else { 'M' };
            format!("\x1b[<{code};{column};{row}{final_byte}").into_bytes()
        }
        encoding => {
            // Legacy encodings cannot name the released button, only that one was.
            let code = if release {
                code | MOUSE_BUTTON_RELEASE
            } else {
                code
            };
            let mut bytes = b"\x1b[M".to_vec();
            for value in [u16::from(code), column, row] {
                let biased = value.checked_add(MOUSE_LEGACY_BIAS)?;
                if encoding == vt100::MouseProtocolEncoding::Utf8 {
                    let mut buffer = [0u8; 4];
                    bytes.extend_from_slice(
                        char::from_u32(u32::from(biased))?
                            .encode_utf8(&mut buffer)
                            .as_bytes(),
                    );
                } else {
                    // A cell past the single-byte ceiling has no representation at all.
                    if biased > MOUSE_LEGACY_MAX {
                        return None;
                    }
                    bytes.push(biased as u8);
                }
            }
            bytes
        }
    })
}

struct TerminalGuard {
    raw_mode: bool,
    keyboard_enhancement: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self {
            raw_mode: false,
            keyboard_enhancement: false,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_capture: false,
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhancement {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
            if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
                let _ = execute!(tty, PopKeyboardEnhancementFlags);
                let _ = tty.flush();
            }
        }
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.mouse_capture {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        if self.alternate_screen {
            let _ = execute!(stdout, crossterm::cursor::Show, LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

fn enable_keyboard_enhancement() -> io::Result<bool> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    Ok(true)
}

/// Run one local shell in a PTY whose dimensions never change after startup.
pub fn run_local() -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut host = PtyHost::spawn_default_shell(size)?;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let mut kitty_keyboard = KittyKeyboardTracker::default();

    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    execute!(io::stdout(), SetTitle("p2pmux"))?;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;

    let backend = CrosstermBackend::new(io::stdout());
    let fixed_area = Rect::new(0, 0, cols, rows);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(fixed_area),
        },
    )?;
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    let mut sync_gate = SyncGate::default();

    loop {
        let drain_started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        let ready = if pending.is_empty() {
            sync_gate.flush_stale(Instant::now())
        } else {
            sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            kitty_keyboard.observe(&ready);
            parser.process(&ready);
            if let Some(reply) = kitty_keyboard.take_query_reply() {
                host.write_input(&reply)?;
            }
            dirty = true;
        }
        if host.output_closed() {
            break;
        }

        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                let screen = parser.screen();
                let area = frame.area();
                frame.render_widget(VtScreen::new(screen), area);
                let (row, col) = screen.cursor_position();
                if !screen.hide_cursor() && row < area.height && col < area.width {
                    frame.set_cursor_position((area.x + col, area.y + row));
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }

        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if is_quit(key) {
                        quit = true;
                        break;
                    }
                    if let Some(bytes) = encode_key(key, parser.screen(), kitty_keyboard.active()) {
                        host.write_input(&bytes)?;
                    }
                }
                Event::Paste(text) => {
                    let bytes = encode_paste(&text, parser.screen().bracketed_paste());
                    host.write_input(&bytes)?;
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }

    Ok(())
}

/// Run the one fixed-grid host PTY and keep all peer work outside its drain loop.
pub fn run_host(mut runtime: HostPaneRuntime) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    let footer = format!("{CONTROL_HELP} | join: p2pmux join {}", runtime.join_code);
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    let mut sync_gate = SyncGate::default();
    loop {
        if let Some(state) = runtime.lease.clear_if_idle(Instant::now())? {
            runtime.lease_tx.send_replace(state);
        }
        while let Ok(event) = runtime.control_rx.try_recv() {
            match event {
                HostControlEvent::Input { peer_id, input } => match runtime.lease.input(
                    &peer_id,
                    input.lease_epoch,
                    input.data,
                    Instant::now(),
                ) {
                    LeaseDecision::AcceptInput(bytes) => {
                        runtime.host.write_input(&bytes)?;
                        runtime.lease_tx.send_replace(runtime.lease.state().clone());
                    }
                    LeaseDecision::Publish(_)
                    | LeaseDecision::RejectStaleInput
                    | LeaseDecision::RejectStaleRequest
                    | LeaseDecision::RejectActiveController => {}
                },
                HostControlEvent::TakeControl { peer_id, request } => {
                    let decision = runtime.lease.take_control(
                        peer_id,
                        request.known_lease_epoch,
                        Instant::now(),
                    )?;
                    match decision {
                        LeaseDecision::Publish(state) => {
                            runtime.lease_tx.send_replace(state);
                        }
                        LeaseDecision::RejectActiveController => {
                            runtime.lease_tx.send_replace(runtime.lease.state().clone());
                        }
                        LeaseDecision::AcceptInput(_)
                        | LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest => {}
                    }
                }
                HostControlEvent::ReleaseControl { peer_id } => {
                    if runtime.lease.state().controller_peer_id == peer_id
                        && let Some(state) = runtime.lease.clear_controller(Instant::now())?
                    {
                        runtime.lease_tx.send_replace(state);
                    }
                }
            }
        }
        let drain_started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = runtime.host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        let ready = if pending.is_empty() {
            sync_gate.flush_stale(Instant::now())
        } else {
            sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            if let Ok(frame) = runtime.screen.process_pty(&ready) {
                if let Some(reply) = runtime.screen.take_kitty_keyboard_query_reply() {
                    runtime.host.write_input(&reply)?;
                }
                runtime.screen_tx.send_replace(frame);
            }
            dirty = true;
        }
        if runtime.host.output_closed() {
            break;
        }
        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                let screen = runtime.screen.screen();
                render_host_screen(frame, screen, &footer);
                let (row, col) = screen.cursor_position();
                let screen_height = screen.size().0.min(frame.area().height.saturating_sub(1));
                if !screen.hide_cursor() && row < screen_height && col < frame.area().width {
                    frame.set_cursor_position((frame.area().x + col, frame.area().y + row));
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }
        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if is_quit(key) {
                        quit = true;
                        break;
                    }
                    if let Some(bytes) = encode_key(
                        key,
                        runtime.screen.screen(),
                        runtime.screen.kitty_keyboard_active(),
                    ) {
                        let now = Instant::now();
                        let epoch = runtime.lease.state().epoch;
                        let decision =
                            runtime
                                .lease
                                .input(&runtime.host_peer_id, epoch, bytes, now);
                        match decision {
                            LeaseDecision::AcceptInput(bytes) => {
                                runtime.host.write_input(&bytes)?;
                                runtime.lease_tx.send_replace(runtime.lease.state().clone());
                            }
                            LeaseDecision::Publish(_) => {}
                            LeaseDecision::RejectStaleInput
                            | LeaseDecision::RejectStaleRequest
                            | LeaseDecision::RejectActiveController => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    let bytes = encode_paste(&text, runtime.screen.screen().bracketed_paste());
                    let now = Instant::now();
                    let epoch = runtime.lease.state().epoch;
                    let decision = runtime
                        .lease
                        .input(&runtime.host_peer_id, epoch, bytes, now);
                    match decision {
                        LeaseDecision::AcceptInput(bytes) => {
                            runtime.host.write_input(&bytes)?;
                            runtime.lease_tx.send_replace(runtime.lease.state().clone());
                        }
                        LeaseDecision::Publish(_) => {}
                        LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest
                        | LeaseDecision::RejectActiveController => {}
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }
    Ok(())
}

/// Render one remote, immutable terminal grid. Input forwarding arrives in milestone 12.
pub fn run_guest(mut pane: GuestPane) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    let mut remote = GuestScreen::new();
    let mut footer = String::from("controller: waiting spectator");
    let mut lease = None;
    let mut last_lease = Instant::now();
    let mut pending_control = false;
    let mut held_input = Vec::new();
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;

    loop {
        let mut received_lease = false;
        loop {
            match pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    if remote
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok()
                    {
                        remote.set_kitty_keyboard_active(snapshot.kitty_keyboard_active);
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    if remote
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok()
                    {
                        remote.set_kitty_keyboard_active(delta.kitty_keyboard_active);
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenGap { .. }) => {}
                Ok(GuestEvent::Lease(state)) => {
                    received_lease = true;
                    footer = format!(
                        "controller: {} typing",
                        short_peer(&state.controller_peer_id)
                    );
                    last_lease = Instant::now();
                    pending_control = false;
                    lease = Some(state);
                    dirty = true;
                }
                Ok(GuestEvent::Disconnected)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "host session disconnected",
                    )
                    .into());
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            }
        }

        if received_lease && let Some(state) = lease.as_ref() {
            reconcile_remote_control_attempt(
                &mut pending_control,
                &mut held_input,
                &state.controller_peer_id,
                pane.controls.peer_id(),
            );
        }

        if !pending_control
            && !held_input.is_empty()
            && let Some(state) = lease.as_ref()
            && lease_allows_held_input(&state.controller_peer_id, pane.controls.peer_id())
        {
            let bytes = std::mem::take(&mut held_input);
            if pane
                .controls
                .try_input(state.lease_epoch, bytes.clone())
                .is_err()
            {
                held_input = bytes;
            }
        }

        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                if let Some(screen) = remote.screen() {
                    render_guest_screen(frame, screen, &footer);
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }

        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && is_quit(key) =>
                {
                    quit = true;
                    break;
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen())
                        && let Some(bytes) = encode_key(key, screen, remote.kitty_keyboard_active())
                    {
                        if state.controller_peer_id == pane.controls.peer_id() {
                            if held_input.is_empty() {
                                let _ = pane.controls.try_input(state.lease_epoch, bytes);
                            } else {
                                held_input.extend_from_slice(&bytes);
                            }
                        } else if state.controller_peer_id.is_empty() {
                            let _ = pane.controls.try_input(state.lease_epoch, bytes);
                        } else if last_lease.elapsed() >= IDLE_AFTER {
                            held_input.extend_from_slice(&bytes);
                            if !pending_control {
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen()) {
                        let bytes = encode_paste(&text, screen.bracketed_paste());
                        if state.controller_peer_id == pane.controls.peer_id() {
                            if held_input.is_empty() {
                                let _ = pane.controls.try_input(state.lease_epoch, bytes);
                            } else {
                                held_input.extend_from_slice(&bytes);
                            }
                        } else if state.controller_peer_id.is_empty() {
                            let _ = pane.controls.try_input(state.lease_epoch, bytes);
                        } else if last_lease.elapsed() >= IDLE_AFTER {
                            held_input.extend_from_slice(&bytes);
                            if !pending_control {
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                        }
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }
    Ok(())
}

fn short_peer(peer_id: &[u8]) -> String {
    peer_id
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn member_label(peer_id: &[u8], members: &[crate::layout::Member]) -> String {
    let Some(member) = members.iter().find(|member| member.peer_id == peer_id) else {
        return short_peer(peer_id);
    };
    if member.display_name.is_empty() {
        return short_peer(peer_id);
    }
    let duplicates = members
        .iter()
        .filter(|candidate| candidate.display_name == member.display_name)
        .count();
    if duplicates > 1 {
        format!("{} · {}", member.display_name, short_peer(peer_id))
    } else {
        member.display_name.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs, io,
        net::Ipv4Addr,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::{mpsc, watch};

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    };
    use portable_pty::PtySize;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Color, Modifier},
    };

    use crate::layout::{Axis, LayoutError, LayoutSnapshot, NewPanePosition, Node, Pane, Tab};
    use crate::lease::{IDLE_AFTER, LeaseManager, LeaseState};
    use crate::screen::{GuestScreen, HostScreen};
    use crate::{agent_detect::cwd_for_pid, config::UiTheme};
    use crate::{
        protocol::PaneDescriptor,
        session::{HostSession, SharedLayoutHost, layout_snapshot_from_state},
        transport::{ALPN, Transport},
    };
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    use super::{
        AGENT_TOGGLE_WINDOW, CHORD_IDLE_TIMEOUT, ChordMode, ESC_PREFIX_WINDOW, FooterMouseInput,
        FooterSegment, HostControlEvent, HostPaneChannels, HostPaneRuntime, KeyHandling,
        LayoutControlEvent, MultiPaneTui, PaneMouseProtocol, PaneTextSelection, PaneViewState,
        PendingEscape, RemoteSubscriptionState, ScreenCell, SharedLayoutRuntime, SharedLocalPane,
        UiIntent, VtScreen, allocate_node_with_preview, area_from_terminal_size,
        chord_footer_badge, contextual_footer, copied_line_count, encode_key, encode_mouse,
        encode_paste, grid_for_pane, initial_root_pane_grid, is_chord_command, is_chord_navigation,
        lease_allows_held_input, member_label, mouse_to_screen_cell, pane_border_color, pane_title,
        pane_wire_id, reconcile_remote_control_attempt, render_guest_screen, render_multi_pane,
        render_multi_pane_with_copy_feedback, render_shared_multi_pane, selection_text, text_width,
        viewed_screen, visible_leaf_panes,
    };

    fn mouse_protocol(
        mode: vt100::MouseProtocolMode,
        encoding: vt100::MouseProtocolEncoding,
    ) -> PaneMouseProtocol {
        PaneMouseProtocol { mode, encoding }
    }

    fn sgr_protocol(mode: vt100::MouseProtocolMode) -> PaneMouseProtocol {
        mouse_protocol(mode, vt100::MouseProtocolEncoding::Sgr)
    }

    #[test]
    fn encode_mouse_skips_panes_without_mouse_reporting() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                PaneMouseProtocol::default(),
            ),
            None
        );
    }

    #[test]
    fn encode_mouse_reports_sgr_press_with_one_based_cells() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 4, col: 11 },
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            Some(b"\x1b[<0;12;5M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_marks_sgr_release_with_a_lowercase_final_byte() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Right),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            Some(b"\x1b[<2;1;1m".to_vec())
        );
    }

    #[test]
    fn encode_mouse_folds_modifiers_into_the_button_code() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                ScreenCell { row: 0, col: 0 },
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            Some(b"\x1b[<24;1;1M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_drops_release_in_press_only_mode() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                sgr_protocol(vt100::MouseProtocolMode::Press),
            ),
            None
        );
    }

    #[test]
    fn encode_mouse_reports_drag_only_once_the_child_asks_for_motion() {
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let cell = ScreenCell { row: 0, col: 0 };
        assert_eq!(
            encode_mouse(
                drag,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                drag,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::ButtonMotion),
            ),
            Some(b"\x1b[<32;1;1M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_reports_bare_motion_only_in_any_motion_mode() {
        let cell = ScreenCell { row: 0, col: 0 };
        assert_eq!(
            encode_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::ButtonMotion),
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::AnyMotion),
            ),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_reports_wheel_even_in_press_only_mode() {
        let cell = ScreenCell { row: 2, col: 2 };
        assert_eq!(
            encode_mouse(
                MouseEventKind::ScrollUp,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::Press),
            ),
            Some(b"\x1b[<64;3;3M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::ScrollDown,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::Press),
            ),
            Some(b"\x1b[<65;3;3M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_biases_legacy_reports_into_printable_bytes() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 4, col: 11 },
                mouse_protocol(
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Default,
                ),
            ),
            Some(vec![0x1b, b'[', b'M', 32, 32 + 12, 32 + 5])
        );
    }

    #[test]
    fn encode_mouse_reports_legacy_release_without_naming_the_button() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Right),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                mouse_protocol(
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Default,
                ),
            ),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33])
        );
    }

    #[test]
    fn encode_mouse_drops_legacy_cells_past_the_single_byte_ceiling() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 300 },
                mouse_protocol(
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Default,
                ),
            ),
            None
        );
    }

    #[test]
    fn encode_mouse_widens_utf8_cells_past_the_single_byte_ceiling() {
        let bytes = encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            ScreenCell { row: 0, col: 300 },
            mouse_protocol(
                vt100::MouseProtocolMode::PressRelease,
                vt100::MouseProtocolEncoding::Utf8,
            ),
        )
        .expect("utf8 encoding carries wide coordinates");
        let mut expected = b"\x1b[M".to_vec();
        expected.push(32);
        expected.extend_from_slice(char::from_u32(333).unwrap().to_string().as_bytes());
        expected.push(33);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn disambiguate_escape_codes_uses_kitty_flag_one() {
        assert_eq!(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.bits(),
            1
        );
    }

    fn layout(tabs: Vec<Tab>, panes: &[(u64, u16, u16)]) -> LayoutSnapshot {
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

    fn agent_row(pane_id: u64, tab_ordinal: usize, pane_ordinal: usize) -> super::AgentOverlayRow {
        super::AgentOverlayRow {
            pane_id,
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

    fn agent_overlay_tui(count: u64) -> MultiPaneTui {
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
        tui.set_agent_rows(
            (1..=count)
                .map(|pane_id| agent_row(pane_id, pane_id as usize, 1))
                .collect(),
        );
        tui.modal = super::ModalState::Agents;
        tui
    }

    #[test]
    fn attached_agent_rows_mark_only_unfocused_working_to_idle_transitions_unread() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: None,
            }],
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        let working = agent_row(2, 1, 2);
        let mut idle = working.clone();
        idle.state = crate::protocol::AgentRosterState::Idle;

        assert_eq!(
            tui.update_attached_agent_rows(vec![working.clone()]),
            Vec::<u64>::new()
        );
        assert_eq!(tui.update_attached_agent_rows(vec![idle.clone()]), vec![2]);
        assert!(tui.unread_agent_panes.contains(&2));
        assert_eq!(
            tui.update_attached_agent_rows(vec![idle]),
            Vec::<u64>::new()
        );

        let mut focused_working = working;
        focused_working.pane_id = 1;
        focused_working.pane_ordinal = 1;
        let mut focused_idle = focused_working.clone();
        focused_idle.state = crate::protocol::AgentRosterState::Idle;
        assert_eq!(
            tui.update_attached_agent_rows(vec![focused_working]),
            Vec::<u64>::new()
        );
        assert_eq!(
            tui.update_attached_agent_rows(vec![focused_idle]),
            Vec::<u64>::new()
        );
        assert!(!tui.unread_agent_panes.contains(&1));
    }

    #[test]
    fn focusing_a_pane_does_not_re_announce_the_same_work_episode() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: None,
            }],
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        let working = agent_row(2, 1, 2);
        let mut idle = working.clone();
        idle.state = crate::protocol::AgentRosterState::Idle;
        // A real host sends the `0` sentinel on a non-working row, so the episode a completion
        // refers to is only ever visible on the working row that preceded it.
        idle.working_since_unix_ms = 0;

        tui.update_attached_agent_rows(vec![working.clone()]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle.clone()]), vec![2]);

        // Looking at the pane clears the unread marker. That must not re-arm the sound: the
        // user has already been told about this episode.
        tui.set_focus(1, 2).expect("known pane");
        tui.set_focus(1, 1).expect("known pane");
        assert!(!tui.unread_agent_panes.contains(&2));

        tui.update_attached_agent_rows(vec![working.clone()]);
        assert_eq!(
            tui.update_attached_agent_rows(vec![idle.clone()]),
            Vec::<u64>::new(),
            "the same work episode must only be announced once"
        );

        // A genuinely new working interval is a new episode and does announce again.
        let mut next_working = working;
        next_working.working_since_unix_ms += 1;
        tui.update_attached_agent_rows(vec![next_working]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle]), vec![2]);
    }

    #[test]
    fn attached_agent_rows_preserve_state_and_unread_when_rows_vanish() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: None,
            }],
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        let working = agent_row(2, 1, 2);
        let mut idle = working.clone();
        idle.state = crate::protocol::AgentRosterState::Idle;

        tui.update_attached_agent_rows(vec![working]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle]), vec![2]);
        tui.update_attached_agent_rows(Vec::new());
        assert_eq!(
            tui.prior_agent_states[&2],
            crate::protocol::AgentRosterState::Idle
        );
        assert!(tui.unread_agent_panes.contains(&2));
    }

    #[test]
    fn focus_routes_clear_unread_agent_markers() {
        let snapshot = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },
                    title: None,
                },
            ],
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");

        tui.unread_agent_panes.insert(2);
        tui.select_tab(2).expect("known tab");
        assert!(!tui.unread_agent_panes.contains(&2));

        tui.unread_agent_panes.insert(1);
        tui.set_focus(1, 1).expect("known pane");
        assert!(!tui.unread_agent_panes.contains(&1));

        tui.unread_agent_panes.insert(2);
        assert_eq!(
            tui.jump_to_agent_pane(2, "test"),
            vec![UiIntent::FocusPane { pane_id: 2 }]
        );
        assert!(!tui.unread_agent_panes.contains(&2));
    }

    #[test]
    fn agents_overlay_opens_immediately_and_double_tap_forwards_ctrl_a() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(
            tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24)),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.overlay_open());
        assert_eq!(
            tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24)),
            KeyHandling::Forward
        );
        assert!(!tui.overlay_open());
        assert_eq!(
            tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24)),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.overlay_open());
        tui.pending_agent_toggle = Some(Instant::now() - AGENT_TOGGLE_WINDOW);
        assert!(!tui.expire_agent_toggle(Instant::now()));
        assert!(tui.overlay_open());
        assert_eq!(tui.pending_agent_toggle, None);
        assert_eq!(
            tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24)),
            KeyHandling::Consumed(vec![])
        );
        assert!(!tui.overlay_open());
    }

    #[test]
    fn agents_overlay_is_modal_and_enter_jumps_to_another_tab() {
        let snapshot = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },
                    title: None,
                },
            ],
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();
        tui.set_agent_rows(vec![agent_row(2, 2, 1)]);
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24));
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                Rect::new(0, 0, 80, 24)
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Rect::new(0, 0, 80, 24)
            ),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
        assert_eq!(tui.pending_agent_toggle, None);
        assert_eq!(
            tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24)),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.overlay_open());
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Rect::new(0, 0, 80, 24)
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.pending_agent_toggle, None);
        assert_eq!(
            tui.handle_key(ctrl_a, Rect::new(0, 0, 80, 24)),
            KeyHandling::Consumed(vec![])
        );
    }

    #[test]
    fn agents_overlay_cards_show_state_elapsed_and_location() {
        let row = agent_row(2, 2, 1);

        let rendered = super::format_agent_overlay_card(
            &row,
            true,
            120,
            1_725_000_084_123,
            0,
            &UiTheme::default(),
        );

        assert!(rendered[0].to_string().starts_with("› Codex · "));
        assert!(
            rendered[0]
                .to_string()
                .contains("/very/long/repository/path")
        );
        assert!(rendered[1].to_string().starts_with("◐ working 1m 24s"));
        assert!(rendered[1].to_string().contains("Tab #2 · Pane #1"));
        assert!(rendered[1].to_string().contains("host: Host"));
        assert!(rendered[1].to_string().contains("control: free"));
    }

    #[test]
    fn agents_overlay_cards_distinguish_idle_done_and_future_work() {
        let mut row = agent_row(2, 2, 1);
        row.state = crate::protocol::AgentRosterState::Idle;
        let idle = super::format_agent_overlay_card(
            &row,
            false,
            120,
            1_725_000_000_123,
            0,
            &UiTheme::default(),
        );
        assert!(idle[1].to_string().starts_with("○ idle"));

        row.state = crate::protocol::AgentRosterState::Done;
        let done = super::format_agent_overlay_card(
            &row,
            false,
            120,
            1_725_000_000_123,
            0,
            &UiTheme::default(),
        );
        assert!(done[1].to_string().starts_with("✓ done"));

        row.state = crate::protocol::AgentRosterState::Working;
        row.working_since_unix_ms = 1_725_000_001_123;
        let future = super::format_agent_overlay_card(
            &row,
            false,
            120,
            1_725_000_000_123,
            0,
            &UiTheme::default(),
        );
        assert!(future[1].to_string().starts_with("◐ working 0s"));
    }

    #[test]
    fn agents_overlay_cards_mark_blocked_and_failed_agents() {
        use crate::protocol::AgentRosterState;

        let theme = UiTheme::default();
        let mut row = agent_row(2, 2, 1);

        row.state = AgentRosterState::Pending;
        let pending =
            super::format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &theme);
        assert!(pending[1].to_string().starts_with("◆ needs you"));
        // A blocked agent is the one row costing someone time, so it carries
        // the attention colour and the only weight in the state column.
        let state_span = &pending[1].spans[2];
        assert_eq!(state_span.style.fg, Some(theme.agent_overlay_attention));
        assert!(state_span.style.add_modifier.contains(Modifier::BOLD));

        row.state = AgentRosterState::Error;
        let error =
            super::format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &theme);
        assert!(error[1].to_string().starts_with("✗ error"));
        assert_eq!(error[1].spans[2].style.fg, Some(theme.agent_overlay_error));

        // Working keeps the chrome accent and stays unweighted.
        row.state = AgentRosterState::Working;
        let working =
            super::format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &theme);
        assert_eq!(
            working[1].spans[2].style.fg,
            Some(theme.agent_overlay_chrome)
        );
        assert!(
            !working[1].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn agents_overlay_title_counts_agents_blocked_on_a_human() {
        use crate::protocol::AgentRosterState;

        let working = agent_row(1, 1, 1);
        assert_eq!(super::agents_overlay_title(&[]), " Agents ");
        assert_eq!(
            super::agents_overlay_title(std::slice::from_ref(&working)),
            " Agents "
        );

        let mut pending = agent_row(2, 1, 2);
        pending.state = AgentRosterState::Pending;
        assert_eq!(
            super::agents_overlay_title(&[working.clone(), pending.clone()]),
            " Agents · 1 needs you "
        );

        // Errors count too: a failed turn is blocking whoever asked for it.
        let mut failed = agent_row(3, 1, 3);
        failed.state = AgentRosterState::Error;
        assert_eq!(
            super::agents_overlay_title(&[working.clone(), pending, failed]),
            " Agents · 2 need you "
        );

        // A finished agent is worth a glance but is not holding anyone up.
        let mut done = agent_row(4, 1, 4);
        done.state = AgentRosterState::Done;
        assert_eq!(super::agents_overlay_title(&[working, done]), " Agents ");
    }

    #[test]
    fn agents_overlay_elapsed_uses_compact_saturating_durations() {
        assert_eq!(super::format_agent_elapsed(1_000, 43_000), "42s");
        assert_eq!(super::format_agent_elapsed(1_000, 85_000), "1m 24s");
        assert_eq!(super::format_agent_elapsed(1_000, 3_721_000), "1h 2m");
        assert_eq!(super::format_agent_elapsed(2_000, 1_000), "0s");
    }

    #[test]
    fn agents_overlay_rows_use_chrome_locations_and_sort_by_them() {
        let snapshot = layout(
            vec![
                Tab {
                    tab_id: 10,
                    root: Node::Split {
                        axis: Axis::LeftRight,
                        first_share_bps: 5_000,
                        first: Box::new(Node::Leaf { pane_id: 8 }),
                        second: Box::new(Node::Leaf { pane_id: 6 }),
                    },

                    title: None,
                },
                Tab {
                    tab_id: 20,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
            ],
            &[(8, 2, 8), (6, 2, 8), (3, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();

        tui.set_agent_rows(vec![
            agent_row(3, 99, 99),
            agent_row(6, 99, 99),
            agent_row(8, 99, 99),
        ]);

        assert_eq!(
            tui.agent_rows
                .iter()
                .map(|row| (row.pane_id, row.tab_ordinal, row.pane_ordinal))
                .collect::<Vec<_>>(),
            vec![(8, 1, 1), (6, 1, 2), (3, 2, 1)]
        );
        assert!(
            super::format_agent_overlay_card(
                &tui.agent_rows[2],
                false,
                120,
                0,
                0,
                &UiTheme::default(),
            )[1]
            .to_string()
            .contains("Tab #2 · Pane #1")
        );
    }

    #[test]
    fn agents_overlay_uses_orange_red_chrome_soft_selection_and_modal_padding() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .expect("valid layout");
        tui.set_agent_rows(vec![agent_row(1, 1, 1)]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

        terminal
            .draw(|frame| super::render_agents_overlay(frame, &tui, 1_725_000_084_123))
            .expect("render");
        let area = Rect::new(0, 0, 80, 24);
        let panel = super::agents_overlay_panel(area);
        let inner = super::agents_overlay_inner(area);
        let content = super::agents_overlay_content(area);
        let help = super::agents_overlay_help(area).expect("help row");
        let buffer = terminal.backend().buffer();

        assert_eq!(panel, Rect::new(1, 1, 78, 22));
        assert_eq!(buffer[(panel.x, panel.y)].fg, Color::Rgb(255, 69, 0));
        assert_eq!(
            buffer[(panel.x.saturating_add(1), panel.y)].modifier,
            Modifier::BOLD
        );
        assert_eq!(buffer[(content.x, content.y)].bg, Color::Rgb(42, 42, 42));
        assert_eq!(
            buffer[(content.x.saturating_add(50), content.y.saturating_add(1))].bg,
            Color::Rgb(42, 42, 42)
        );
        assert_eq!(
            buffer[(content.right().saturating_sub(1), content.y)].bg,
            Color::Rgb(42, 42, 42)
        );
        assert_eq!(buffer[(inner.x, content.y)].bg, Color::Reset);
        assert_eq!(buffer[(content.x, inner.y)].bg, Color::Reset);
        assert_eq!(
            buffer[(content.x, content.y.saturating_add(2))].bg,
            Color::Reset
        );
        for x in help.x..help.right() {
            assert_eq!(buffer[(x, help.y)].bg, tui.theme.footer_background);
        }
        assert_eq!(buffer[(help.x.saturating_add(1), help.y)].symbol(), "<");
        assert_eq!(buffer[(help.x.saturating_add(2), help.y)].symbol(), "↑");
        assert_eq!(buffer[(help.x.saturating_add(14), help.y)].symbol(), "E");
    }

    #[test]
    fn agents_overlay_help_renders_for_empty_rosters() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

        terminal
            .draw(|frame| super::render_agents_overlay(frame, &tui, 1_725_000_084_123))
            .expect("render");
        let help = super::agents_overlay_help(Rect::new(0, 0, 80, 24)).expect("help row");
        let buffer = terminal.backend().buffer();

        for x in help.x..help.right() {
            assert_eq!(buffer[(x, help.y)].bg, tui.theme.footer_background);
        }
        assert_eq!(buffer[(help.x.saturating_add(2), help.y)].symbol(), "↑");
        assert_eq!(buffer[(help.x.saturating_add(14), help.y)].symbol(), "E");
    }

    #[test]
    fn agents_overlay_click_jumps_to_the_clicked_row_and_ignores_outside_clicks() {
        let snapshot = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();
        tui.set_agent_rows(vec![agent_row(1, 1, 1), agent_row(2, 2, 1)]);
        let area = Rect::new(0, 0, 80, 24);
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        tui.handle_key(ctrl_a, area);

        assert_eq!(tui.handle_agent_overlay_click(0, 0, area), Vec::new());
        assert!(tui.overlay_open());
        let inner = super::agents_overlay_inner(area);
        let content = super::agents_overlay_content(area);
        let help = super::agents_overlay_help(area).expect("help row");
        assert_eq!(
            tui.agent_overlay_row_at(content.x.saturating_sub(1), content.y, area),
            None
        );
        assert_eq!(tui.agent_overlay_row_at(content.x, inner.y, area), None);
        assert_eq!(tui.agent_overlay_row_at(help.x, help.y, area), None);
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y, area),
            Some(1)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(1), area),
            Some(1)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(3), area),
            Some(2)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(4), area),
            Some(2)
        );
        assert_eq!(
            tui.handle_agent_overlay_click(content.x, content.y.saturating_add(3), area),
            vec![UiIntent::FocusPane { pane_id: 2 }]
        );
        assert!(!tui.overlay_open());
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
        assert_eq!(tui.pending_agent_toggle, None);
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
    }

    #[test]
    fn agents_overlay_hit_test_accounts_for_terminal_line_scroll() {
        let area = Rect::new(0, 0, 80, 8);
        let mut tui = agent_overlay_tui(3);
        let content = super::agents_overlay_content(area);

        assert!(tui.scroll_agent_overlay(area, false));
        assert_eq!(tui.agent_overlay_scroll_line, 3);
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y, area),
            Some(2)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(2), area),
            None
        );
    }

    #[test]
    fn agents_overlay_selection_auto_scrolls_to_keep_cards_visible() {
        let area = Rect::new(0, 0, 80, 8);
        let mut tui = agent_overlay_tui(4);

        tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area);
        assert_eq!(tui.agent_selected_pane, Some(2));
        assert_eq!(tui.agent_overlay_scroll_line, 3);
        tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area);
        assert_eq!(tui.agent_selected_pane, Some(3));
        assert_eq!(tui.agent_overlay_scroll_line, 6);
    }

    #[test]
    fn agents_overlay_hides_help_below_the_minimum_content_height() {
        let short = Rect::new(0, 0, 80, 7);
        let threshold = Rect::new(0, 0, 80, 8);

        assert_eq!(super::agents_overlay_inner(short).height, 3);
        assert_eq!(super::agents_overlay_help(short), None);
        assert_eq!(super::agents_overlay_content(short).height, 2);
        assert!(super::agents_overlay_help(threshold).is_some());
        assert_eq!(super::agents_overlay_content(threshold).height, 2);
    }

    #[test]
    fn agents_overlay_scroll_clamps_when_roster_shrinks() {
        let area = Rect::new(0, 0, 80, 8);
        let mut tui = agent_overlay_tui(4);

        assert!(tui.scroll_agent_overlay(area, false));
        assert!(tui.scroll_agent_overlay(area, false));
        assert_eq!(tui.agent_overlay_scroll_line, 6);
        tui.set_agent_rows(vec![agent_row(1, 1, 1)]);
        assert_eq!(tui.agent_overlay_scroll_line, 0);
    }

    #[test]
    fn agents_overlay_working_glyph_uses_animation_phase() {
        assert_eq!(super::agent_overlay_working_glyph(0), "◐");
        assert_eq!(super::agent_overlay_working_glyph(1), "◓");
        assert_eq!(super::agent_overlay_working_glyph(4), "◐");
    }

    #[test]
    fn terminal_area_is_absent_when_terminal_size_is_unavailable() {
        assert_eq!(
            area_from_terminal_size(Err(io::Error::from(io::ErrorKind::WouldBlock))),
            None
        );
    }

    #[test]
    fn failed_remote_subscription_retries_with_bounded_backoff_and_commit_nudge() {
        let mut subscriptions = RemoteSubscriptionState::default();
        assert!(subscriptions.start(7, 0));
        subscriptions.failed(7, 0);
        assert!(!subscriptions.start(7, 0));
        assert!(subscriptions.start(7, 1));
        subscriptions.failed(7, 1);
        assert!(!subscriptions.start(7, 2));
        assert!(subscriptions.start(7, 3));

        for tick in [3, 7, 15, 31, 63, 95] {
            subscriptions.failed(7, tick);
            assert!(
                subscriptions
                    .next_retry_at(7)
                    .is_some_and(|next| next <= tick + 32)
            );
        }
        subscriptions.nudge();
        assert!(subscriptions.start(7, 95));
    }

    async fn loopback_transport() -> Transport {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("localhost address")
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .expect("loopback endpoint");
        Transport::from_endpoint(endpoint)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_create_then_committed_delete_updates_its_local_pane_lifecycle() {
        let directory = std::env::temp_dir().join(format!(
            "p2pmux-runtime-create-cwd-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        fs::create_dir(&directory).expect("create temporary directory");
        let expected_cwd = fs::canonicalize(&directory).expect("canonicalize temporary directory");
        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            2,
            8,
        )
        .expect("shared host");
        let pane_server = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let initial = SharedLocalPane::spawn(1, 2, 8, host_id.clone()).expect("initial pty");
        pane_server
            .register_local_pane(
                PaneDescriptor {
                    pane_id: 1,
                    host_peer_id: host_id,
                    grid_rows: 2,
                    grid_cols: 8,
                    title: None,
                    locked: false,
                    exited: false,
                },
                initial.channels(),
            )
            .expect("initial pane registered");
        let state = host
            .session_snapshot()
            .expect("snapshot")
            .state
            .expect("layout state");
        let snapshot = layout_snapshot_from_state(&state).expect("render layout");
        let mut runtime = SharedLayoutRuntime::host(
            host,
            pane_server,
            snapshot,
            initial,
            String::from("TESTCODE"),
            tokio::runtime::Handle::current(),
        )
        .expect("runtime");
        runtime.set_session_id(b"session".to_vec());

        thread::sleep(Duration::from_secs(2));
        let source_pid = runtime
            .local
            .get(&1)
            .and_then(|pane| pane.host.process_id())
            .expect("source PTY child PID");
        runtime
            .local
            .get_mut(&1)
            .expect("source local pane")
            .host
            .write_input(format!("cd -- {}\n", directory.display()).as_bytes())
            .expect("change source PTY directory");
        let source_cwd = (0..20).find_map(|_| {
            let cwd = cwd_for_pid(source_pid);
            if cwd
                .as_ref()
                .and_then(|cwd| fs::canonicalize(cwd).ok())
                .as_ref()
                == Some(&expected_cwd)
            {
                cwd
            } else {
                thread::sleep(Duration::from_millis(25));
                None
            }
        });
        assert!(source_cwd.is_some(), "source PTY changed directory");

        runtime
            .handle_intent(UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::LeftRight,
                position: NewPanePosition::Second,
                grid_rows: 2,
                grid_cols: 8,
            })
            .expect("create intent commits after registering a local pane");
        assert!(runtime.local.contains_key(&2));
        let created_pid = runtime
            .local
            .get(&2)
            .and_then(|pane| pane.host.process_id())
            .expect("created PTY child PID");
        assert_eq!(
            cwd_for_pid(created_pid)
                .as_ref()
                .and_then(|cwd| fs::canonicalize(cwd).ok()),
            Some(expected_cwd.clone())
        );
        assert!(runtime.panes.has_registered_pane(2).expect("pane registry"));
        assert!(
            runtime.provisional.is_empty(),
            "committed panes clear provisional state"
        );
        assert_eq!(runtime.tui.snapshot().panes.len(), 2);
        assert_eq!(runtime.tui.focused_pane(), 2);

        runtime.footer_notice = Some(String::from("layout request 5 rejected"));
        let peer_id = runtime.control.peer_id();
        let pane = runtime.local.get_mut(&2).expect("created local pane");
        pane.lease = LeaseManager::new(peer_id, Instant::now());
        let mut lease_rx = pane.lease_tx.subscribe();
        assert!(
            !runtime
                .handle_key(
                    KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
                    Rect::new(0, 0, 80, 24),
                )
                .expect("focus change")
        );
        assert_eq!(runtime.footer_notice, None);
        assert!(lease_rx.has_changed().expect("lease published immediately"));
        assert!(lease_rx.borrow_and_update().controller_peer_id.is_empty());

        runtime
            .handle_intent(UiIntent::DeletePane { pane_id: 2 })
            .expect("host-owned deletion commits");
        assert!(!runtime.local.contains_key(&2));
        assert!(
            !runtime.panes.has_registered_pane(2).expect("pane registry"),
            "committed removal revokes the direct-pane service before the PTY is shut down"
        );
        assert_eq!(runtime.tui.snapshot().panes.len(), 1);
        drop(runtime);
        fs::remove_dir(&directory).expect("remove temporary directory");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn member_runtime_reconciles_snapshot_into_a_direct_remote_pane_attachment() {
        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            1,
            1,
        )
        .expect("shared host");
        let host_panes = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let screen = HostScreen::new(1, 1).expect("screen");
        let (_screen_tx, screen_rx) = watch::channel(screen.current_frame().clone());
        let (_lease_tx, lease_rx) = watch::channel(LeaseState {
            controller_peer_id: host_id.clone(),
            epoch: 1,
            last_activity: Instant::now(),
        });
        let (control_tx, _control_rx) = mpsc::channel(8);
        let descriptor = PaneDescriptor {
            pane_id: 1,
            host_peer_id: host_id.clone(),
            grid_rows: 1,
            grid_cols: 1,
            title: None,
            locked: false,
            exited: false,
        };
        host_panes
            .register_local_pane(
                descriptor.clone(),
                HostPaneChannels {
                    pane_id: pane_wire_id(1),
                    host_peer_id: host_id,
                    screen_rx: screen_rx.clone(),
                    lease_rx: lease_rx.clone(),
                    control_tx: control_tx.clone(),
                },
            )
            .expect("host pane");
        let dispatcher = host
            .incoming_dispatcher(host_panes.clone())
            .expect("single dispatcher");
        let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });

        let mut member =
            crate::session::join_layout(loopback_transport().await, host.ticket().clone())
                .await
                .expect("member joins");
        let LayoutControlEvent::Snapshot(snapshot) = member.events.recv().await.expect("snapshot")
        else {
            panic!("member must receive snapshot first");
        };
        let state = snapshot.state.expect("state");
        let member_panes = member
            .pane_server(host.ticket().session_id().to_vec())
            .expect("member pane server");
        let mut runtime = SharedLayoutRuntime::member_from_state(
            member,
            member_panes,
            host.ticket().session_id().to_vec(),
            state.clone(),
            tokio::runtime::Handle::current(),
        )
        .expect("member runtime");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain");
            if runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            runtime.remote.contains_key(&1),
            "remote pane attached from snapshot"
        );

        host_panes
            .remove_local_pane(1)
            .expect("remove direct pane")
            .expect("registered pane");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain after direct close");
            if !runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !runtime.remote.contains_key(&1),
            "a direct stream close removes the stale remote pane"
        );
        host_panes
            .register_local_pane(
                descriptor,
                HostPaneChannels {
                    pane_id: pane_wire_id(1),
                    host_peer_id: host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                    screen_rx,
                    lease_rx,
                    control_tx,
                },
            )
            .expect("restore direct pane");
        runtime
            .apply_layout_state(&state)
            .expect("authoritative snapshot nudges reconnect");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain after restore");
            if runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            runtime.remote.contains_key(&1),
            "remote pane reconnects after a transient direct close"
        );
        dispatcher_task.abort();
    }

    #[test]
    fn shared_footer_places_rejection_notice_after_help() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "",
                    None,
                    Some("layout request 5 rejected"),
                    Some("TESTCODE"),
                    None,
                );
            })
            .expect("draw");
        let footer = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        let help = footer.find("QUIT").expect("help rendered");
        let notice = footer
            .find("layout request 5 rejected")
            .expect("rejection notice rendered");
        let join = footer.find("join:").expect("join code rendered");
        assert!(help < notice, "notice sits after helper text");
        assert!(notice < join, "notice sits before join code");
        assert_eq!(
            terminal.backend().buffer()[(notice as u16, 4)].fg,
            UiTheme::default().footer_orange
        );
        assert!(
            !footer.trim_start().starts_with("layout request"),
            "rejection no longer occupies the left status slot: {footer}"
        );
    }

    #[test]
    fn pane_view_activity_transition_reports_a_redraw() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("layout");
        let active = PaneViewState {
            ready: true,
            controller_peer_id: Some(b"controller".to_vec()),
            controller_active: true,
            scrollback: 0,
        };
        assert!(tui.set_pane_view(1, active.clone()));
        assert!(!tui.set_pane_view(1, active));
        assert!(tui.set_pane_view(
            1,
            PaneViewState {
                controller_active: false,
                ..tui.pane_view(1).expect("pane view").clone()
            },
        ));
    }

    #[test]
    fn shared_renderer_draws_status_before_join_code_in_the_footer() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "waiting for current pane reservation",
                    None,
                    None,
                    Some("TESTCODE"),
                    None,
                );
            })
            .expect("draw");
        let footer = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        assert!(footer.contains("waiting for current pane reservation"));
        assert!(footer.contains("join: p2pmux join TESTCODE"));
        let join = "join: p2pmux join TESTCODE";
        let join_x = footer.find(join).expect("join code rendered") as u16;
        for x in join_x..160 {
            assert!(
                terminal.backend().buffer()[(x, 4)]
                    .modifier
                    .contains(Modifier::UNDERLINED),
                "join cell at x={x} is underlined"
            );
        }
    }

    #[test]
    fn attach_renderer_draws_join_code_in_the_footer() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_multi_pane_with_copy_feedback(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    None,
                    Some("copied join command"),
                    Some("TESTCODE"),
                    None,
                    None,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("join: p2pmux join TESTCODE"));
        assert!(rendered.contains("copied join command"));
    }

    #[test]
    fn shared_footer_places_copy_feedback_after_quit_with_a_red_line_count() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "",
                    Some(3),
                    None,
                    None,
                    None,
                );
            })
            .expect("draw");
        let footer = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        let quit = footer.find("> QUIT").expect("quit help rendered");
        let copied = footer
            .find("copied 3 lines")
            .expect("copy feedback rendered");
        let count = footer.find("3 lines").expect("line count rendered");

        assert!(copied > quit + "> QUIT".len());
        assert_eq!(
            terminal.backend().buffer()[(text_width(&footer[..copied]), 4)].fg,
            Color::White
        );
        assert_eq!(
            terminal.backend().buffer()[(text_width(&footer[..count]), 4)].fg,
            Color::Rgb(255, 69, 0)
        );
    }

    #[test]
    fn rejected_busy_claim_republishes_lease_and_idle_claim_succeeds() {
        let owner = b"owner".to_vec();
        let requester = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, owner.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(owner.clone(), Instant::now());
        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: requester.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: requester.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("busy takeover event");
        pane.drain().expect("drain busy takeover");
        assert!(lease_rx.has_changed().expect("lease watch"));
        assert_eq!(lease_rx.borrow_and_update().controller_peer_id, owner);

        pane.lease = LeaseManager::with_epoch_for_test(
            pane.host_peer_id.clone(),
            1,
            Instant::now() - IDLE_AFTER,
        );
        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: requester.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: requester.clone(),
                    known_lease_epoch: 2,
                },
            })
            .expect("idle takeover event");
        pane.drain().expect("drain idle takeover");
        assert_eq!(pane.lease.state().controller_peer_id, requester);
        assert_eq!(pane.lease.state().epoch, 3);
    }

    #[test]
    fn idle_controller_is_cleared_and_published_to_watchers() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::with_epoch_for_test(host_id, 1, Instant::now() - IDLE_AFTER);

        pane.drain().expect("drain idle controller");

        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert!(lease.controller_peer_id.is_empty());
        assert_eq!(lease.epoch, 2);
    }

    #[test]
    fn exited_local_pane_clears_lease_and_rejects_all_controls() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(host_id.clone(), Instant::now());
        assert!(pane.transition_exited().expect("exit transition"));
        assert!(pane.exited);
        assert!(pane.lease.state().controller_peer_id.is_empty());
        assert!(lease_rx.has_changed().expect("lease publication"));

        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: host_id,
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: b"host".to_vec(),
                    known_lease_epoch: pane.lease.state().epoch,
                },
            })
            .expect("queued control");
        let drained = pane.drain().expect("drain exited pane");
        assert!(drained.newly_exited);
        assert!(pane.lease.state().controller_peer_id.is_empty());
        pane.shutdown().expect("shutdown exited pane");
    }

    #[test]
    fn locked_local_pane_rejects_guest_control_and_accepts_host_control() {
        let host_id = b"host".to_vec();
        let guest_id = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        pane.set_locked(true).expect("lock pane");

        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: guest_id.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: guest_id.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("guest takeover event");
        pane.drain().expect("drain guest takeover");
        assert!(pane.lease.state().controller_peer_id.is_empty());

        pane.control_tx
            .try_send(HostControlEvent::Input {
                peer_id: guest_id.clone(),
                input: crate::protocol::Input {
                    pane_id: pane_wire_id(99),
                    lease_epoch: 1,
                    data: b"blocked".to_vec(),
                },
            })
            .expect("guest input event");
        pane.drain().expect("drain guest input");
        assert!(pane.lease.state().controller_peer_id.is_empty());

        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: host_id.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: host_id.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("host takeover event");
        pane.drain().expect("drain host takeover");
        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn locking_local_pane_clears_guest_lease_but_keeps_host_lease() {
        let host_id = b"host".to_vec();
        let guest_id = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(guest_id, Instant::now());

        pane.set_locked(true).expect("lock pane");
        assert!(lease_rx.has_changed().expect("lease watch"));
        assert!(lease_rx.borrow_and_update().controller_peer_id.is_empty());

        pane.lease = LeaseManager::new(host_id.clone(), Instant::now());
        pane.set_locked(false).expect("unlock pane");
        pane.set_locked(true).expect("relock pane");
        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn remote_release_clears_the_host_lease_immediately() {
        let host_id = b"host".to_vec();
        let guest_id = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(guest_id.clone(), Instant::now());
        pane.control_tx
            .try_send(HostControlEvent::ReleaseControl { peer_id: guest_id })
            .expect("remote release event");

        pane.drain().expect("drain remote release");

        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert!(lease.controller_peer_id.is_empty());
        assert_eq!(lease.epoch, 2);

        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn local_input_crossing_the_idle_timeout_reclaims_and_delivers_it() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease =
            LeaseManager::with_epoch_for_test(b"remote".to_vec(), 1, Instant::now() - IDLE_AFTER);

        pane.input(b"printf boundary-input-delivered\\n".to_vec())
            .expect("first input at idle boundary");

        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        assert_eq!(pane.lease.state().epoch, 3);
        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert_eq!(lease.controller_peer_id, b"host");
        assert_eq!(lease.epoch, 3);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            while let Some(bytes) = pane.host.try_read_output().expect("PTY reader") {
                output.extend(bytes);
            }
            if String::from_utf8_lossy(&output).contains("boundary-input-delivered") {
                pane.shutdown().expect("shutdown local pane");
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pane.shutdown().expect("shutdown local pane");
        panic!(
            "first input was not delivered to the PTY: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn new_local_pane_starts_free_while_the_host_retains_pty_ownership() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");

        assert!(pane.lease.state().controller_peer_id.is_empty());
        assert_eq!(pane.host_peer_id, host_id);

        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn local_pane_resize_publishes_a_replacement_screen_frame() {
        let mut pane = SharedLocalPane::spawn(99, 1, 1, b"host".to_vec()).expect("pane");
        let before = pane.screen.current_frame().sequence;
        pane.resize(3, 4).expect("resize");
        assert_eq!(pane.screen.screen().size(), (3, 4));
        assert!(pane.screen.current_frame().sequence > before);
        assert_eq!(pane.screen.current_frame().base_sequence, 0);
        pane.shutdown().expect("shutdown");
    }

    #[test]
    fn new_host_runtime_starts_free_while_the_host_retains_pty_ownership() {
        let host_id = b"host".to_vec();
        let screen = HostScreen::new(1, 1).expect("screen");
        let (screen_tx, _) = watch::channel(screen.current_frame().clone());
        let (lease_tx, lease_rx) = watch::channel(LeaseState {
            controller_peer_id: host_id.clone(),
            epoch: 1,
            last_activity: Instant::now(),
        });
        let (_control_tx, control_rx) = mpsc::channel(8);
        let mut runtime = HostPaneRuntime::new(
            PtySize {
                rows: 1,
                cols: 1,
                pixel_width: 0,
                pixel_height: 0,
            },
            host_id.clone(),
            screen_tx,
            lease_tx,
            control_rx,
            String::from("TESTCODE"),
        )
        .expect("host runtime");

        assert!(runtime.lease.state().controller_peer_id.is_empty());
        assert_eq!(runtime.host_peer_id, host_id);
        assert!(lease_rx.borrow().controller_peer_id.is_empty());

        runtime.host.shutdown().expect("shutdown host runtime");
    }

    #[test]
    fn remote_held_input_follows_the_final_lease_owner() {
        let peer = b"requester";
        let mut pending_control = true;
        let mut held_input = b"first".to_vec();

        assert!(lease_allows_held_input(b"", peer));
        reconcile_remote_control_attempt(&mut pending_control, &mut held_input, b"", peer);
        assert!(!pending_control, "free leases release the retry gate");
        assert_eq!(held_input, b"first", "free leases retain queued input");

        reconcile_remote_control_attempt(&mut pending_control, &mut held_input, b"other", peer);
        assert!(
            held_input.is_empty(),
            "a later controller wins and discards stale input"
        );
    }

    fn split_layout() -> LayoutSnapshot {
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

    #[test]
    fn ratio_allocation_respects_recursive_minima_and_small_areas() {
        let node = Node::Split {
            axis: Axis::LeftRight,
            first_share_bps: 7_500,
            first: Box::new(Node::Leaf { pane_id: 1 }),
            second: Box::new(Node::Split {
                axis: Axis::LeftRight,
                first_share_bps: 5_000,
                first: Box::new(Node::Leaf { pane_id: 2 }),
                second: Box::new(Node::Leaf { pane_id: 3 }),
            }),
        };
        let mut panes = BTreeMap::new();
        allocate_node_with_preview(&node, Rect::new(0, 0, 12, 9), &mut panes, None);
        assert_eq!(panes[&1].width, 6, "nested sibling needs six columns");
        assert_eq!(panes[&2].width, 3);
        assert_eq!(panes[&3].width, 3);

        panes.clear();
        allocate_node_with_preview(&node, Rect::new(0, 0, 2, 2), &mut panes, None);
        assert_eq!(panes[&1].width + panes[&2].width + panes[&3].width, 2);
    }

    #[test]
    fn content_drag_keeps_the_selection_path() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(!tui.begin_resize_drag(2, 3, area));
        assert!(tui.resize_drag.is_none());
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        assert!(tui.end_selection_drag());
        assert!(tui.selection().is_some());
    }

    fn join_footer_input() -> FooterMouseInput<'static> {
        FooterMouseInput {
            join_code: Some("TESTCODE"),
            ..FooterMouseInput::default()
        }
    }

    fn left_mouse(kind: MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_down_on_visible_join_command_arms_join_copy() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let footer = join_footer_input();
        let join_rect = tui
            .footer_join_rect(area, footer)
            .expect("visible join command");

        let handling = tui.handle_mouse(
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );

        assert_eq!(handling, super::MouseHandling::default());
        assert!(tui.pending_join_click);
        assert_eq!(tui.focused_pane(), 1);
        assert!(tui.resize_drag.is_none());
    }

    #[test]
    fn join_click_cancels_on_drag_away_or_release_outside() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let footer = join_footer_input();
        let join_rect = tui
            .footer_join_rect(area, footer)
            .expect("visible join command");
        let outside_x = join_rect.x.saturating_sub(1);

        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                outside_x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        let handling = tui.handle_mouse(
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        assert!(!tui.pending_join_click);
        assert!(handling.join_copy_command.is_none());

        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        let handling = tui.handle_mouse(
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                outside_x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        assert!(!tui.pending_join_click);
        assert!(handling.join_copy_command.is_none());
    }

    #[test]
    fn join_click_requests_runnable_join_command() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let footer = join_footer_input();
        let join_rect = tui
            .footer_join_rect(area, footer)
            .expect("visible join command");

        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        let handling = tui.handle_mouse(
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );

        assert_eq!(
            handling.join_copy_command.as_deref(),
            Some("p2pmux join TESTCODE")
        );
        assert!(!handling.copy_selection_requested);
    }

    #[test]
    fn mouse_down_on_footer_help_does_not_arm_join_copy() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let footer = join_footer_input();
        let layout = super::footer_layout(
            tui.geometry(area).footer,
            tui.chord_mode(),
            footer.status,
            footer.footer_notice,
            footer.copied_lines,
            footer.join_code,
        );
        assert!(layout.help_start < layout.help_end);

        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                layout.help_start,
                tui.geometry(area).footer.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        let handling = tui.handle_mouse(
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                layout.help_start,
                tui.geometry(area).footer.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );

        assert!(!tui.pending_join_click);
        assert!(handling.join_copy_command.is_none());
    }

    #[test]
    fn join_click_preserves_existing_pane_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let footer = join_footer_input();
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        assert!(tui.end_selection_drag());
        let selection = tui.selection();
        let join_rect = tui
            .footer_join_rect(area, footer)
            .expect("visible join command");

        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );
        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                join_rect.x,
                join_rect.y,
            ),
            area,
            footer,
            PaneMouseProtocol::default(),
        );

        assert_eq!(tui.selection(), selection);
    }

    fn reporting_child() -> PaneMouseProtocol {
        mouse_protocol(
            vt100::MouseProtocolMode::ButtonMotion,
            vt100::MouseProtocolEncoding::Sgr,
        )
    }

    #[test]
    fn clicking_a_focused_reporting_pane_forwards_instead_of_selecting() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, Some(b"\x1b[<0;2;2M".to_vec()));
        assert!(handling.intents.is_empty());
        assert_eq!(tui.selection(), None);
    }

    #[test]
    fn a_forwarded_press_keeps_the_drag_and_release_away_from_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            FooterMouseInput::default(),
            protocol,
        );
        let drag = tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 4, 3),
            area,
            FooterMouseInput::default(),
            protocol,
        );
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 4, 3),
            area,
            FooterMouseInput::default(),
            protocol,
        );

        assert_eq!(drag.forward_bytes, Some(b"\x1b[<32;4;2M".to_vec()));
        assert_eq!(up.forward_bytes, Some(b"\x1b[<0;4;2m".to_vec()));
        assert!(!up.copy_selection_requested);
        assert_eq!(tui.selection(), None);
    }

    #[test]
    fn a_drag_that_leaves_the_pane_still_reports_inside_its_edge() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            FooterMouseInput::default(),
            protocol,
        );
        let drag = tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 70, 20),
            area,
            FooterMouseInput::default(),
            protocol,
        );

        // The 10x4 grid clamps the report to its last cell rather than dropping it.
        assert_eq!(drag.forward_bytes, Some(b"\x1b[<32;10;4M".to_vec()));
    }

    #[test]
    fn shift_click_selects_even_when_the_child_reports_mouse() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let shift_down = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::SHIFT,
        };

        let handling = tui.handle_mouse(
            shift_down,
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
        assert!(tui.selection_dragging);
    }

    #[test]
    fn clicking_an_unfocused_reporting_pane_only_moves_focus() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(tui.focused_pane(), 1);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 45, 3),
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
        assert_eq!(tui.focused_pane(), 2);
        assert!(matches!(
            handling.intents.as_slice(),
            [UiIntent::FocusPane { pane_id: 2 }]
        ));
    }

    #[test]
    fn a_scrolled_back_pane_selects_history_instead_of_forwarding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.set_pane_scrollback_offset(1, 5));

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
        assert!(tui.selection_dragging);
    }

    #[test]
    fn the_wheel_reaches_a_reporting_child_over_its_own_pane() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::ScrollUp, 2, 3),
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, Some(b"\x1b[<64;2;2M".to_vec()));
    }

    #[test]
    fn the_wheel_stays_local_over_a_pane_the_reporting_child_does_not_own() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::ScrollDown, 45, 3),
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
    }

    #[test]
    fn the_wheel_stays_local_while_a_reporting_pane_shows_history() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.set_pane_scrollback_offset(1, 3));

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::ScrollDown, 2, 3),
            area,
            FooterMouseInput::default(),
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
    }

    #[test]
    fn a_reporting_child_never_takes_a_border_drag_from_the_mux() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        let down = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 39, 5),
            area,
            FooterMouseInput::default(),
            protocol,
        );
        tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 49, 5),
            area,
            FooterMouseInput::default(),
            protocol,
        );
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 49, 5),
            area,
            FooterMouseInput::default(),
            protocol,
        );

        assert_eq!(down.forward_bytes, None);
        assert!(matches!(
            up.intents.as_slice(),
            [UiIntent::SetSplitRatio { .. }]
        ));
    }

    #[test]
    fn mouse_up_requests_copy_only_for_a_nonempty_selection_without_resize_commit() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let down = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };

        assert!(
            tui.handle_mouse(
                down,
                area,
                FooterMouseInput::default(),
                PaneMouseProtocol::default()
            )
            .intents
            .is_empty()
        );
        tui.handle_mouse(
            drag,
            area,
            FooterMouseInput::default(),
            PaneMouseProtocol::default(),
        );
        let handling = tui.handle_mouse(
            up,
            area,
            FooterMouseInput::default(),
            PaneMouseProtocol::default(),
        );
        assert!(handling.intents.is_empty());
        assert!(handling.copy_selection_requested);

        let resize_down = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 39,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let resize_drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 49,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let resize_up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 49,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        tui.handle_mouse(
            resize_down,
            area,
            FooterMouseInput::default(),
            PaneMouseProtocol::default(),
        );
        tui.handle_mouse(
            resize_drag,
            area,
            FooterMouseInput::default(),
            PaneMouseProtocol::default(),
        );
        let handling = tui.handle_mouse(
            resize_up,
            area,
            FooterMouseInput::default(),
            PaneMouseProtocol::default(),
        );
        assert!(matches!(
            handling.intents.as_slice(),
            [UiIntent::SetSplitRatio { .. }]
        ));
        assert!(!handling.copy_selection_requested);
    }

    #[test]
    fn dragging_a_shared_vertical_border_resizes_its_split() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));
        assert!(matches!(
            tui.end_resize_drag(49, 5),
            Some(UiIntent::SetSplitRatio {
                pane_id: 1,
                axis: Axis::LeftRight,
                first_share_bps: 6_250,
                base_revision: 1,
            })
        ));
        assert!(tui.end_resize_drag(49, 5).is_none());

        assert!(tui.begin_resize_drag(40, 5, area));
        assert!(tui.extend_resize_drag(50, 5));
        assert!(matches!(
            tui.end_resize_drag(50, 5),
            Some(UiIntent::SetSplitRatio {
                pane_id: 2,
                axis: Axis::LeftRight,
                first_share_bps: 6_250,
                base_revision: 1,
            })
        ));
    }

    #[test]
    fn resize_drag_previews_geometry_without_mutating_the_snapshot() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let snapshot = tui.snapshot().clone();
        let initial = tui.geometry(area);

        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));

        let preview = tui.geometry(area);
        assert_eq!(initial.panes[&1], Rect::new(0, 1, 40, 22));
        assert_eq!(preview.panes[&1], Rect::new(0, 1, 50, 22));
        assert_eq!(tui.snapshot(), &snapshot);
        assert!(matches!(
            tui.end_resize_drag(49, 5),
            Some(UiIntent::SetSplitRatio {
                pane_id: 1,
                axis: Axis::LeftRight,
                first_share_bps: 6_250,
                base_revision: 1,
            })
        ));
        assert_eq!(tui.snapshot(), &snapshot);
        assert_eq!(tui.geometry(area), initial);
    }

    #[test]
    fn same_revision_snapshot_preserves_resize_preview_and_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let initial = tui.geometry(area);

        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        assert_ne!(tui.geometry(area), initial);

        tui.apply_snapshot(tui.snapshot().clone())
            .expect("valid snapshot");
        assert!(tui.resize_drag.is_some());
        assert!(tui.selection().is_some());
        assert_ne!(tui.geometry(area), initial);
    }

    #[test]
    fn same_revision_different_snapshot_is_rejected() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let mut conflicting = tui.snapshot().clone();
        conflicting.tabs[0].title = Some("conflict".into());

        assert_eq!(
            tui.apply_snapshot(conflicting),
            Err(LayoutError::ConflictingSnapshotRevision { revision: 1 })
        );
    }

    #[test]
    fn higher_revision_snapshot_cancels_resize_preview_and_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        let mut newer = tui.snapshot().clone();
        newer.revision += 1;

        tui.apply_snapshot(newer).expect("valid snapshot");
        assert!(tui.resize_drag.is_none());
        assert!(tui.selection().is_none());
    }

    #[test]
    fn dragging_a_shared_horizontal_border_resizes_its_split() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let initial = tui.geometry(area);
        assert!(tui.begin_resize_drag(60, 11, area));
        assert!(tui.extend_resize_drag(60, 11));
        assert_eq!(tui.geometry(area), initial);
        assert!(tui.extend_resize_drag(60, 16));
        assert_eq!(tui.geometry(area).panes[&2], Rect::new(40, 1, 40, 15));
        assert!(matches!(
            tui.end_resize_drag(60, 16),
            Some(UiIntent::SetSplitRatio {
                pane_id: 2,
                axis: Axis::TopBottom,
                first_share_bps: 7_272,
                base_revision: 1,
            })
        ));
    }

    #[test]
    fn pane_title_uses_stable_leaf_order_and_control_state() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 8 }),
                    second: Box::new(Node::Leaf { pane_id: 3 }),
                },

                title: None,
            }],
            &[(3, 1, 1), (8, 1, 1)],
        );
        let mut members = snapshot.members.clone();
        members[0].display_name = String::from("Host");
        members.push(crate::layout::Member {
            peer_id: b"guest".to_vec(),
            endpoint_addr: b"guest-endpoint".to_vec(),
            display_name: String::from("Guest"),
        });

        assert_eq!(visible_leaf_panes(&snapshot.tabs[0].root), vec![8, 3]);
        assert_eq!(
            pane_title(None, 1, b"host", Some(b""), false, false, &members, 80).to_string(),
            "Pane #1 host: Host control: free"
        );
        assert_eq!(
            pane_title(None, 2, b"host", Some(b"guest"), false, false, &members, 80).to_string(),
            "Pane #2 host: Host control: Guest"
        );
        assert_eq!(
            pane_title(None, 2, b"host", None, false, false, &members, 80).to_string(),
            "Pane #2 host: Host control: …"
        );
        assert_eq!(
            pane_title(None, 2, b"host", Some(b"guest"), true, false, &members, 80).to_string(),
            "Pane #2 host: Host control: host-only"
        );
        assert_eq!(
            pane_title(None, 2, b"host", Some(b"guest"), true, true, &members, 80).to_string(),
            "Pane #2 host: Host control: exited"
        );
    }

    #[test]
    fn title_chrome_uses_custom_labels_and_cell_width_ellipsis() {
        assert_eq!(
            super::tab_label(Some("build logs"), 1, false, false),
            "build logs"
        );
        assert_eq!(super::tab_label(None, 2, false, false), "Tab #2");
        assert_eq!(
            super::tab_label(Some("build logs"), 1, false, true),
            "* build logs"
        );
        assert_eq!(super::truncate_trailing("界界", 3), "界…");
        assert_eq!(super::truncate_trailing("界", 1), "…");
        assert_eq!(super::truncate_trailing("title", 0), "");

        let members = vec![crate::layout::Member {
            peer_id: b"host".to_vec(),
            endpoint_addr: b"endpoint".to_vec(),
            display_name: String::from("Host"),
        }];
        assert_eq!(
            pane_title(
                Some("build logs"),
                1,
                b"host",
                Some(b""),
                false,
                false,
                &members,
                12
            )
            .to_string(),
            "build logs …"
        );
    }

    #[test]
    fn unread_agent_badges_use_the_same_starred_tab_label_for_hit_testing() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: Some(String::from("build")),
            }],
            &[(1, 2, 8), (2, 2, 8)],
        ))
        .expect("valid layout");
        tui.unread_agent_panes.insert(2);
        let area = Rect::new(0, 0, 80, 8);
        let geometry = tui.geometry(area);
        assert_eq!(
            geometry.tab_labels[&1].width,
            text_width(&super::tab_label(Some("build"), 1, true, true))
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test terminal");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let tab_bar = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        let pane_titles = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(tab_bar.contains("* build"));
        assert!(pane_titles.contains("* Pane #2"));
    }

    #[test]
    fn locked_badge_keeps_right_title_space_on_narrow_chrome() {
        let mut snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 2)],
        );
        snapshot.panes.get_mut(&1).expect("pane").locked = true;
        snapshot.members[0].display_name = String::from("Host");
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(18, 4)).expect("terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("draw");
        let chrome = (0..18)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(chrome.contains("locked by Host"), "{chrome}");
        assert!(!chrome.contains("Pane #1"));
    }

    #[test]
    fn rename_prompt_captures_chord_target_edits_and_consumes_modal_input() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        tui.snapshot.panes.get_mut(&1).unwrap().title = Some(String::from("old"));

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(matches!(
            &tui.modal,
            super::ModalState::Rename(prompt) if prompt.value == "old"
        ));
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('🙂'), KeyModifiers::SHIFT),
                area
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::RenamePane {
                pane_id: 1,
                title: String::from("old"),
            }])
        );
        assert!(!tui.overlay_open());
    }

    #[test]
    fn rename_prompt_rejects_invalid_input_and_ctrl_q_wins() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        tui.open_rename(super::RenameTarget::Tab(1));
        for character in "x".repeat(33).chars() {
            tui.handle_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                area,
            );
        }
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(matches!(tui.modal, super::ModalState::Rename(_)));
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Quit
        );
        assert!(matches!(tui.modal, super::ModalState::None));
    }

    #[test]
    fn multi_pane_tab_delete_requires_confirmation_and_blocks_pane_input() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(tui.modal_open());
        assert!(matches!(
            &tui.modal,
            super::ModalState::ConfirmDeleteTab {
                tab_id: 1,
                pane_count: 3
            }
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let mut rendered = String::new();
        for row in 0..24 {
            for column in 0..80 {
                rendered.push_str(terminal.backend().buffer()[(column, row)].symbol());
            }
        }
        assert!(rendered.contains("Delete tab?"));
        assert!(rendered.contains("3 panes"));
        assert!(rendered.contains("Enter/y yes · Esc/n no"));

        let mouse = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 60,
            row: 17,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            tui.handle_mouse(
                mouse,
                area,
                FooterMouseInput::default(),
                PaneMouseProtocol::default()
            ),
            super::MouseHandling::default()
        );
        assert_eq!(tui.focused_pane(), 1);
        assert!(!tui.scroll_mouse_pane(60, 17, area, 10, true));
        assert!(tui.resize_drag.is_none());
        assert!(tui.selection.is_none());
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.modal_open());

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(!tui.modal_open());

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 1 }])
        );
        assert!(!tui.modal_open());
    }

    #[test]
    fn single_pane_tab_delete_is_immediate() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .expect("layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 1 }])
        );
        assert!(!tui.modal_open());
    }

    #[test]
    fn free_panes_use_a_mid_gray_border_when_hovered_unfocused() {
        let theme = UiTheme::default();
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b""),
                false,
                true,
                false,
                ChordMode::None
            ),
            Color::White
        );
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b""),
                false,
                false,
                true,
                ChordMode::None
            ),
            Color::Gray
        );
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b""),
                false,
                false,
                false,
                ChordMode::None
            ),
            Color::DarkGray
        );
        assert_eq!(
            pane_border_color(&theme, false, None, false, true, false, ChordMode::None),
            Color::Yellow
        );
        assert_eq!(
            pane_border_color(&theme, false, None, false, false, true, ChordMode::None),
            Color::Gray
        );
        assert_eq!(
            pane_border_color(&theme, false, None, false, false, false, ChordMode::None),
            Color::DarkGray
        );
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b"guest"),
                true,
                true,
                true,
                ChordMode::None
            ),
            Color::Rgb(255, 69, 0)
        );
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b"guest"),
                false,
                false,
                true,
                ChordMode::None
            ),
            Color::Rgb(255, 69, 0)
        );

        let themed = UiTheme {
            pane_border_remote_control: Color::Rgb(1, 2, 3),
            ..Default::default()
        };
        assert_eq!(
            pane_border_color(
                &themed,
                false,
                Some(b"guest"),
                false,
                false,
                false,
                ChordMode::None,
            ),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b"guest"),
                true,
                true,
                true,
                ChordMode::Pane,
            ),
            theme.pane_border_chord_focused
        );
        assert_eq!(
            pane_border_color(
                &theme,
                false,
                Some(b"guest"),
                true,
                true,
                true,
                ChordMode::Tab,
            ),
            theme.pane_border_remote_control
        );
    }

    #[test]
    fn multi_pane_geometry_recursively_splits_the_content_area() {
        let tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let geometry = tui.geometry(Rect::new(0, 0, 80, 24));

        assert_eq!(geometry.tab_bar, Rect::new(0, 0, 80, 1));
        assert_eq!(geometry.footer, Rect::new(0, 23, 80, 1));
        assert_eq!(geometry.content, Rect::new(0, 1, 80, 22));
        assert_eq!(geometry.panes[&1], Rect::new(0, 1, 40, 22));
        assert_eq!(geometry.panes[&2], Rect::new(40, 1, 40, 11));
        assert_eq!(geometry.panes[&3], Rect::new(40, 12, 40, 11));
    }

    #[test]
    fn mouse_hit_testing_focuses_the_clicked_pane_and_ignores_misses() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );

        assert!(tui.focus_pane_at(60, 17, area));
        assert_eq!(tui.focused_pane(), 3);
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert!(!tui.focus_pane_at(10, 0, area));
        assert_eq!(tui.focused_pane(), 3);
    }

    #[test]
    fn hovering_an_unfocused_pane_does_not_move_focus() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert!(tui.hover_pane_at(60, 17, area));
        assert_eq!(tui.hovered_pane, Some(3));
        assert_eq!(tui.focused_pane(), 1);
        assert!(tui.hover_pane_at(10, 0, area));
        assert_eq!(tui.hovered_pane, None);
        assert_eq!(tui.focused_pane(), 1);
    }

    #[test]
    fn mouse_coordinates_map_to_visible_screen_cells_only() {
        let viewport = Rect::new(10, 5, 3, 2);

        assert_eq!(
            mouse_to_screen_cell(viewport, 12, 6),
            Some(ScreenCell { row: 1, col: 2 })
        );
        assert_eq!(mouse_to_screen_cell(viewport, 9, 5), None);
        assert_eq!(mouse_to_screen_cell(viewport, 13, 6), None);
    }

    #[test]
    fn selection_hit_testing_uses_the_bordered_pane_content_area() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 19, 78)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(tui.screen_cell_at(0, 1, area), None);
        assert_eq!(
            tui.screen_cell_at(1, 2, area),
            Some((1, ScreenCell { row: 0, col: 0 }))
        );
    }

    #[test]
    fn mouse_wheel_adjusts_the_hovered_pane_scrollback_with_clamping() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let pane_id = tui.pane_at_or_focused(60, 17, area);
        assert_eq!(pane_id, 3);

        assert!(tui.scroll_pane(pane_id, 10, true));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 3);
        assert!(tui.scroll_pane(pane_id, 4, true));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 4);
        assert!(!tui.scroll_pane(pane_id, 4, true));
        assert!(tui.scroll_pane(pane_id, 4, false));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 1);
        assert!(tui.scroll_pane(pane_id, 4, false));
        assert!(!tui.scroll_pane(pane_id, 4, false));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 0);
    }

    #[test]
    fn input_resets_the_pane_scrollback_to_the_live_edge() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        assert!(tui.scroll_pane(1, 4, true));
        assert!(tui.reset_scrollback(1));
        assert_eq!(tui.pane_view(1).expect("pane view").scrollback, 0);
        assert!(!tui.reset_scrollback(1));
    }

    #[test]
    fn appended_local_history_pins_a_scrolled_back_viewport() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        assert!(tui.scroll_pane(1, 10, true));
        assert!(tui.scroll_pane(1, 10, true));
        assert!(tui.pin_scrollback_after_output(1, 3, 10));
        assert_eq!(tui.pane_view(1).expect("pane view").scrollback, 9);
        tui.reset_scrollback(1);
        assert!(!tui.pin_scrollback_after_output(1, 3, 10));
    }

    #[test]
    fn selection_text_uses_stream_semantics_in_document_order() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 2, col: 1 },
            cursor: ScreenCell { row: 0, col: 2 },
        };

        assert_eq!(
            selection_text(parser.screen(), selection),
            Some("cd\nefgh\nij".to_owned())
        );
    }

    #[test]
    fn selection_text_uses_the_scrolled_view_cells() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let screen = viewed_screen(parser.screen(), 1);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 0, col: 0 },
            cursor: ScreenCell { row: 0, col: 1 },
        };

        assert_eq!(selection_text(&screen, selection), Some("on".to_owned()));
    }

    #[test]
    fn empty_selection_has_no_clipboard_text() {
        let parser = vt100::Parser::new(1, 1, 0);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 0, col: 0 },
            cursor: ScreenCell { row: 0, col: 0 },
        };

        assert_eq!(selection_text(parser.screen(), selection), None);
    }

    #[test]
    fn copied_line_count_reports_newline_separated_lines() {
        assert_eq!(copied_line_count("one line"), 1);
        assert_eq!(copied_line_count("first\nsecond\nthird"), 3);
        assert_eq!(copied_line_count(""), 1);
    }

    #[test]
    fn tab_hit_testing_switches_the_clicked_rendered_label_without_exiting_tab_mode() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 32, 8);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );

        let second_tab = tui.geometry(area).tab_labels[&2];
        assert_eq!(
            tui.switch_tab_at(second_tab.x, 0, area),
            Some(UiIntent::SwitchTab { tab_id: 2 })
        );
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        assert_eq!(tui.switch_tab_at(0, 0, area), None);
    }

    #[test]
    fn tiny_terminal_geometry_stays_in_bounds() {
        let tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let geometry = tui.geometry(Rect::new(u16::MAX, u16::MAX, 1, 1));

        assert_eq!(geometry.tab_bar, Rect::new(u16::MAX, u16::MAX, 1, 1));
        assert_eq!(geometry.footer, Rect::new(u16::MAX, u16::MAX, 1, 0));
        assert_eq!(geometry.content, Rect::new(u16::MAX, u16::MAX, 1, 0));
        assert!(
            geometry
                .panes
                .values()
                .all(|rect| rect.x == u16::MAX && rect.y == u16::MAX)
        );
    }

    #[test]
    fn snapshot_commit_repairs_removed_tab_and_pane_selection() {
        let initial = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        );
        let mut tui = MultiPaneTui::new(initial).expect("valid layout");
        tui.select_tab(2).expect("select second tab");

        let mut committed = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        );
        committed.revision = 2;
        tui.apply_snapshot(committed).expect("valid commit");

        assert_eq!(tui.current_tab(), 1);
        assert_eq!(tui.focused_pane(), 1);
    }

    #[test]
    fn chrome_reports_the_pane_host_and_controller() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: Some(b"peer".to_vec()),
                controller_active: true,
                scrollback: 0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(36, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), "┌");
        assert_eq!(buffer[(1, 1)].symbol(), " ");
        assert_eq!(buffer[(2, 1)].symbol(), "P");
        assert!((2..9).all(|x| buffer[(x, 1)].modifier.contains(Modifier::BOLD)));
        assert!(!buffer[(9, 1)].modifier.contains(Modifier::BOLD));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "h"));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "t"));
    }

    #[test]
    fn pane_badge_uses_the_snapshot_host_not_mutable_view_state() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let title = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();

        assert!(title.contains("host: 686f7374"));
        assert!(!title.contains("host: 66616b65"));
    }

    #[test]
    fn focused_ready_pane_maps_its_visible_vt_cursor_into_the_letterboxed_viewport() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"ab");
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 3)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
            },
        );
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");

        terminal.backend_mut().assert_cursor_position((3, 2));
    }

    #[test]
    fn focused_pane_never_draws_a_hidden_or_clipped_vt_cursor() {
        for sequence in [b"\x1b[?25l".as_slice(), b"abcd".as_slice()] {
            let mut parser = vt100::Parser::new(1, 5, 0);
            parser.process(sequence);
            let mut tui = MultiPaneTui::new(layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                }],
                &[(1, 1, 3)],
            ))
            .expect("valid layout");
            tui.set_pane_view(
                1,
                PaneViewState {
                    ready: true,
                    controller_peer_id: None,
                    controller_active: false,
                    scrollback: 0,
                },
            );
            let screens = BTreeMap::from([(1, parser.screen())]);
            let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &screens))
                .expect("render");

            assert!(!terminal.backend().cursor_visible());
        }
    }

    #[test]
    fn fixed_grid_view_is_top_left_and_clears_letterbox_margins() {
        let mut previous_parser = vt100::Parser::new(3, 6, 0);
        previous_parser.process(b"XXXXXX\r\nYYYYYY\r\nZZZZZZ");
        let previous_tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 3, 6)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(8, 7)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_multi_pane(
                    frame,
                    &previous_tui,
                    &BTreeMap::from([(1, previous_parser.screen())]),
                )
            })
            .expect("initial render");

        let mut parser = vt100::Parser::new(1, 5, 0);
        parser.process(b"abcde");
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 5)],
        ))
        .expect("valid layout");
        let screens = BTreeMap::from([(1, parser.screen())]);

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(1, 2)].symbol(), "a");
        assert_eq!(buffer[(5, 2)].symbol(), "e");
        assert_eq!(buffer[(7, 3)].symbol(), "│");
        for (x, y) in [(6, 2), (1, 3), (6, 3), (1, 4), (6, 4)] {
            assert_eq!(buffer[(x, y)].symbol(), " ", "letterbox cell ({x}, {y})");
        }
    }

    #[test]
    fn pane_chord_consumes_commands_and_uses_focused_rect_aspect() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::LeftRight,
                position: NewPanePosition::Second,
                grid_rows: 20,
                grid_cols: 38,
            }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.focused_pane(), 2);
    }

    #[test]
    fn option_arrows_focus_in_normal_and_pane_modes_without_forwarding_at_edges() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), area),
            KeyHandling::Consumed(vec![]),
            "an edge Option-arrow is still consumed"
        );

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 3 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
    }

    #[test]
    fn pane_mode_invite_requests_a_ticket_copy_once_and_leaves_the_mode() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![]),
            "invite is a mux command and never reaches the PTY"
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);

        assert!(tui.take_ticket_copy_request());
        assert!(
            !tui.take_ticket_copy_request(),
            "a claimed request must not copy again on the next key"
        );
    }

    #[test]
    fn plain_invite_key_reaches_the_pty_outside_pane_mode() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert!(!tui.take_ticket_copy_request());
    }

    #[test]
    fn option_arrows_accept_extra_modifiers_but_not_control() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Right, KeyModifiers::ALT | KeyModifiers::SHIFT),
                area,
            ),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Forward
        );
        assert_eq!(tui.focused_pane(), 2);
    }

    #[test]
    fn esc_then_arrow_uses_option_focus_and_expired_esc_keeps_its_prior_behavior() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let now = Instant::now();
        let mut pending_escape = PendingEscape::default();

        pending_escape.start(now);
        let option_arrow = pending_escape
            .take_option_arrow(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .expect("Esc followed by an arrow becomes Option-arrow focus");
        assert_eq!(
            tui.handle_key(option_arrow, area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        pending_escape.start(now);
        assert!(pending_escape.take_if_expired(now + ESC_PREFIX_WINDOW));
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn pane_chord_directional_splits_select_axis_and_child_position() {
        let area = Rect::new(0, 0, 80, 24);
        for (key, axis, position) in [
            ('r', Axis::LeftRight, NewPanePosition::Second),
            ('l', Axis::LeftRight, NewPanePosition::First),
            ('d', Axis::TopBottom, NewPanePosition::Second),
            ('u', Axis::TopBottom, NewPanePosition::First),
        ] {
            let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
            let _ = tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            );
            assert_eq!(
                tui.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE), area),
                KeyHandling::Consumed(vec![UiIntent::CreatePane {
                    target_pane_id: 1,
                    axis,
                    position,
                    grid_rows: 20,
                    grid_cols: 38,
                }]),
                "key: {key}"
            );
            assert_eq!(tui.focused_pane(), 1, "key: {key}");
            assert_eq!(tui.chord_mode(), ChordMode::None, "key: {key}");
        }
    }

    #[test]
    fn directional_split_keys_are_commands_but_not_chord_navigation() {
        for key in ['r', 'l', 'd', 'u'] {
            assert!(is_chord_command(
                ChordMode::Pane,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
            ));
            assert!(!is_chord_navigation(
                ChordMode::Pane,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
            ));
        }
    }

    #[test]
    fn pane_lock_chord_toggles_lock_and_exits_mode() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SetPaneLock {
                pane_id: 1,
                locked: true,
            }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
        ));
        assert!(!is_chord_navigation(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn pane_commands_exit_mode_even_when_no_intent_is_available() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        tui.snapshot.panes.clear();

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn sticky_pane_mode_moves_focus_across_multiple_arrows() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 3 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
    }

    #[test]
    fn escape_clears_sticky_chord_mode_without_forwarding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn sticky_chord_mode_expires_two_seconds_after_last_key() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let now = Instant::now();
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        tui.chord_last_activity = now.checked_sub(CHORD_IDLE_TIMEOUT);
        assert!(tui.expire_chord_mode(now));
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(tui.chord_last_activity.is_none());

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        tui.chord_last_activity = now.checked_sub(Duration::from_millis(1_999));
        assert!(!tui.expire_chord_mode(now));
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area);
        tui.chord_last_activity = now.checked_sub(CHORD_IDLE_TIMEOUT);
        assert!(tui.expire_chord_mode(now));
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn forwarding_key_exits_sticky_pane_mode_once() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn paste_exits_sticky_chord_mode_before_forwarding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );

        tui.exit_chord_mode();

        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn shared_footer_uses_help_for_each_chord_mode() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        for (mode, expected) in [
            (
                ChordMode::None,
                "Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS",
            ),
            (
                ChordMode::Pane,
                "PANE MODE  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <k> LOCK   <i> INVITE   <Esc> BACK",
            ),
            (
                ChordMode::Tab,
                "TAB MODE  <←→> SWITCH   <e> RENAME   <n> NEW   <x> CLOSE   <Esc> BACK",
            ),
        ] {
            tui.chord_mode = mode;
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = (0..120)
                .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
                .collect::<String>();
            assert!(
                footer.starts_with(expected),
                "mode: {mode:?}, footer: {footer}"
            );
            if mode == ChordMode::None {
                assert!(footer.contains("drag border RESIZE"), "footer: {footer}");
            }
        }
    }

    #[test]
    fn shared_footer_accents_key_glyphs_on_a_dark_background() {
        let theme = UiTheme::default();
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        for mode in [ChordMode::None, ChordMode::Pane, ChordMode::Tab] {
            tui.chord_mode = mode;
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = terminal.backend().buffer();
            assert_eq!(footer[(0, 3)].bg, Color::Rgb(30, 30, 30));
            let (_, segments) = contextual_footer(mode);
            let mut x = 0;
            if let Some(badge) = chord_footer_badge(mode, 120) {
                for character in badge.chars() {
                    assert_eq!(footer[(x, 3)].fg, theme.footer_orange);
                    assert_eq!(footer[(x, 3)].bg, theme.footer_background);
                    assert!(footer[(x, 3)].modifier.contains(Modifier::BOLD));
                    assert_eq!(footer[(x, 3)].symbol(), character.to_string());
                    x += 1;
                }
            }
            for segment in segments {
                let (text, fg, bg, bold) = match segment {
                    FooterSegment::Text(text) => {
                        (*text, theme.footer_muted, theme.footer_background, false)
                    }
                    FooterSegment::Key(text) => {
                        (*text, theme.footer_accent, theme.footer_background, true)
                    }
                    FooterSegment::OrangeKey(text) => {
                        (*text, theme.footer_orange, theme.footer_background, true)
                    }
                };
                for key in text.chars() {
                    assert_eq!(footer[(x, 3)].fg, fg, "mode: {mode:?}, key: {key}");
                    assert_eq!(footer[(x, 3)].bg, bg, "mode: {mode:?}, key: {key}");
                    assert_eq!(
                        footer[(x, 3)].modifier.contains(Modifier::BOLD),
                        bold,
                        "mode: {mode:?}, key: {key}"
                    );
                    x += 1;
                }
            }
        }
    }

    #[test]
    fn chord_footer_badges_fall_back_atomically_on_narrow_terminals() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");

        for (mode, width, expected) in [
            (ChordMode::Pane, 9, "PANE MODE"),
            (ChordMode::Pane, 8, "PANE"),
            (ChordMode::Pane, 4, "PANE"),
            (ChordMode::Pane, 3, ""),
            (ChordMode::Tab, 8, "TAB MODE"),
            (ChordMode::Tab, 7, "TAB"),
        ] {
            let mut tui = tui.clone();
            tui.chord_mode = mode;
            let mut terminal = Terminal::new(TestBackend::new(width, 4)).expect("test terminal");
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = (0..width)
                .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
                .collect::<String>();

            assert!(
                footer.starts_with(expected),
                "mode: {mode:?}, footer: {footer}"
            );
            assert!(
                !footer.starts_with("PANE MO") || footer.starts_with("PANE MODE"),
                "footer: {footer}"
            );
            assert!(
                !footer.starts_with("TAB MOD") || footer.starts_with("TAB MODE"),
                "footer: {footer}"
            );
        }
    }

    #[test]
    fn chord_mode_chrome_switches_and_restores_remote_focused_pane_colors() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState::from_chrome(true, Some(b"guest".to_vec()), true),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test terminal");

        tui.chord_mode = ChordMode::Pane;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, tui.theme.pane_border_chord_focused);
        assert_eq!(
            (0..9).map(|x| buffer[(x, 7)].symbol()).collect::<String>(),
            "PANE MODE"
        );

        tui.chord_mode = ChordMode::Tab;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, tui.theme.pane_border_remote_control);
        assert_eq!(
            (0..8).map(|x| buffer[(x, 7)].symbol()).collect::<String>(),
            "TAB MODE"
        );

        tui.chord_mode = ChordMode::None;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, tui.theme.pane_border_remote_control);
        assert!(
            (0..4)
                .map(|x| buffer[(x, 7)].symbol())
                .collect::<String>()
                .starts_with("Ctrl")
        );
    }

    #[test]
    fn themed_tui_overrides_footer_chrome() {
        let theme = UiTheme {
            footer_background: Color::Rgb(1, 2, 3),
            footer_orange: Color::Rgb(4, 5, 6),
            ..Default::default()
        };
        let tui = MultiPaneTui::with_theme(
            layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                    title: None,
                }],
                &[(1, 2, 2)],
            ),
            theme,
        )
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let footer = terminal.backend().buffer();
        assert_eq!(footer[(0, 3)].bg, theme.footer_background);
        assert_eq!(footer[(0, 3)].fg, theme.footer_orange);
    }

    #[test]
    fn tab_bar_uses_a_branded_footer_like_strip_and_highlights_the_active_tab() {
        let mut tui = MultiPaneTui::new(layout(
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
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(32, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let tab_bar = (0..32).map(|x| buffer[(x, 0)].symbol()).collect::<String>();

        assert!(tab_bar.starts_with("p2pmux │  Tab #1  · Tab #2"));
        assert!(tab_bar.contains("Tab #1"));
        assert!(tab_bar.contains("Tab #2"));
        assert_eq!(buffer[(0, 0)].fg, Color::White);
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(30, 30, 30));
        assert_eq!(buffer[(9, 0)].fg, Color::White);
        assert_eq!(buffer[(9, 0)].bg, Color::Rgb(220, 50, 47));
        assert_eq!(buffer[(20, 0)].fg, Color::White);
        assert_eq!(buffer[(20, 0)].bg, Color::Rgb(30, 30, 30));

        tui.chord_mode = ChordMode::Tab;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(9, 0)].fg, Color::White);
        assert_eq!(buffer[(9, 0)].bg, Color::Rgb(220, 50, 47));
        assert_eq!(buffer[(20, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(20, 0)].bg, Color::Rgb(30, 30, 30));

        tui.chord_mode = ChordMode::None;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        assert_eq!(terminal.backend().buffer()[(20, 0)].fg, Color::White);
    }

    #[test]
    fn member_labels_disambiguate_duplicate_display_names() {
        let members = vec![
            crate::layout::Member {
                peer_id: vec![0xaa, 0xbb, 0xcc, 0xdd],
                endpoint_addr: vec![1],
                display_name: "sam".into(),
            },
            crate::layout::Member {
                peer_id: vec![0x11, 0x22, 0x33, 0x44],
                endpoint_addr: vec![2],
                display_name: "sam".into(),
            },
            crate::layout::Member {
                peer_id: vec![0x55, 0x66, 0x77, 0x88],
                endpoint_addr: vec![3],
                display_name: "pat".into(),
            },
        ];

        assert_eq!(
            member_label(&members[0].peer_id, &members),
            "sam · aabbccdd"
        );
        assert_eq!(member_label(&members[2].peer_id, &members), "pat");
    }

    #[test]
    fn initial_root_pane_grid_matches_its_bordered_viewport() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        ))
        .expect("valid layout");

        let pane = tui
            .geometry(Rect::new(0, 0, 80, 24))
            .panes
            .get(&1)
            .copied()
            .expect("root pane");
        assert_eq!(grid_for_pane(pane), (20, 78));
        assert_eq!(initial_root_pane_grid(80, 24), (20, 78));
    }

    #[test]
    fn square_pane_create_chord_splits_top_to_bottom_without_a_tab_bar_gap() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 12, 14);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::TopBottom,
                position: NewPanePosition::Second,
                grid_rows: 10,
                grid_cols: 10,
            }])
        );
    }

    #[test]
    fn pane_delete_chord_targets_the_focused_pane() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area);
        assert_eq!(tui.focused_pane(), 2);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeletePane { pane_id: 2 }])
        );
    }

    #[test]
    fn tall_pane_create_chord_uses_a_top_bottom_split_and_usable_grid() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 20, 40);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::TopBottom,
                position: NewPanePosition::Second,
                grid_rows: 36,
                grid_cols: 18,
            }])
        );
    }

    #[test]
    fn forwarding_keys_exit_a_chord_mode_and_are_forwarded() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        for prefix in ['p', 't'] {
            assert_eq!(
                tui.handle_key(
                    KeyEvent::new(KeyCode::Char(prefix), KeyModifiers::CONTROL),
                    area
                ),
                KeyHandling::Consumed(vec![])
            );
            assert_eq!(
                tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
                KeyHandling::Forward
            );
            assert_eq!(tui.chord_mode(), ChordMode::None);
        }
    }

    #[test]
    fn pane_focus_uses_the_nearest_directional_leaf_and_stops_at_edges() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 3 }])
        );
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 1 }])
        );
        assert_eq!(tui.focused_pane(), 1);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.focused_pane(), 2);

        let mut edge_tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let _ = edge_tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            edge_tui.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(edge_tui.focused_pane(), 1);

        let _ = edge_tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = edge_tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area);
        assert_eq!(edge_tui.focused_pane(), 2);
        let _ = edge_tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = edge_tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area);
        assert_eq!(edge_tui.focused_pane(), 3);
        for key in [KeyCode::Down, KeyCode::Right] {
            let _ = edge_tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            );
            assert_eq!(
                edge_tui.handle_key(KeyEvent::new(key, KeyModifiers::NONE), area),
                KeyHandling::Consumed(vec![])
            );
            assert_eq!(edge_tui.focused_pane(), 3);
        }
    }

    #[test]
    fn creator_selects_only_its_explicitly_reserved_tab_after_that_tab_commits() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");

        tui.select_created_tab(2);
        let mut unrelated = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (3, 2, 2)],
        );
        unrelated.revision = 2;
        tui.apply_snapshot(unrelated).expect("unrelated commit");
        assert_eq!(tui.current_tab(), 1);

        let mut targeted = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2), (3, 2, 2)],
        );
        targeted.revision = 3;
        tui.apply_snapshot(targeted).expect("targeted tab commit");

        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
    }

    #[test]
    fn tab_chord_switches_and_creates_or_deletes_tabs_without_forwarding_keys() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 12, 8);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SwitchTab { tab_id: 2 }])
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreateTab {
                grid_rows: 4,
                grid_cols: 10,
            }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 2 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn sticky_tab_mode_switches_tabs_repeatedly() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2), (3, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 12, 8);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SwitchTab { tab_id: 2 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SwitchTab { tab_id: 3 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
    }

    #[test]
    fn normal_keys_escape_and_function_keys_leave_f9_and_f10_for_the_pty() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Quit
        );
    }

    #[test]
    fn remote_renderer_keeps_host_grid_fixed_and_draws_a_footer() {
        let mut host = HostScreen::new(1, 3).expect("host screen");
        let frame = host.process_pty(b"abc").expect("frame");
        let mut guest = GuestScreen::new();
        guest
            .apply_snapshot(frame.sequence, &frame.snapshot)
            .expect("snapshot");
        let mut terminal = Terminal::new(TestBackend::new(5, 3)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_guest_screen(
                    frame,
                    guest.screen().expect("guest screen"),
                    "controller: abcdef idle",
                )
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 0)].symbol(), "c");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].symbol(), "c");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
    }

    #[test]
    fn remote_renderer_places_the_host_cursor() {
        let mut host = HostScreen::new(1, 3).expect("host screen");
        let frame = host.process_pty(b"ab").expect("frame");
        let mut guest = GuestScreen::new();
        guest
            .apply_snapshot(frame.sequence, &frame.snapshot)
            .expect("snapshot");
        let mut terminal = Terminal::new(TestBackend::new(5, 3)).expect("test terminal");

        terminal
            .draw(|frame| {
                render_guest_screen(
                    frame,
                    guest.screen().expect("guest screen"),
                    "controller: peer typing",
                );
            })
            .expect("render");

        terminal.backend_mut().assert_cursor_position((2, 0));
    }

    #[test]
    fn an_uppercase_chord_letter_is_reachable_despite_its_shift_modifier() {
        // Shift is how `L` is typed, not a modifier the user chose to add. An earlier
        // version rejected every modified key here, which made the session lock
        // impossible to trigger from a real keyboard while every unit test passed.
        assert!(is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT)
        ));
        assert!(is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE)
        ));
        // A genuinely extra modifier is still not a chord command.
        assert!(!is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::CONTROL)
        ));
        assert!(!is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn shift_l_toggles_the_session_lock_from_whatever_state_it_is_in() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT), area),
            KeyHandling::Consumed(vec![UiIntent::SetSessionLock { locked: true }]),
            "Shift+L should offer to lock a session that is currently open"
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);

        tui.set_session_locked(true);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT), area),
            KeyHandling::Consumed(vec![UiIntent::SetSessionLock { locked: false }]),
            "and to unlock one that is locked"
        );
    }

    #[test]
    fn renders_vt100_cell_styles() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"\x1b[31;44;1mX");
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render should work");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "X");
        assert_eq!(buffer[(0, 0)].fg, Color::Indexed(1));
        assert_eq!(buffer[(0, 0)].bg, Color::Indexed(4));
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn renderer_highlights_stream_selected_cells() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 2, col: 1 },
            cursor: ScreenCell { row: 0, col: 2 },
        };
        let mut terminal = Terminal::new(TestBackend::new(4, 3)).expect("test terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(
                    VtScreen::new(parser.screen()).with_selection(Some(selection)),
                    frame.area(),
                )
            })
            .expect("render should work");

        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(1, 0)].bg, Color::DarkGray);
        assert_eq!(buffer[(2, 0)].bg, Color::DarkGray);
        assert_eq!(buffer[(0, 1)].bg, Color::DarkGray);
        assert_eq!(buffer[(1, 2)].bg, Color::DarkGray);
        assert_ne!(buffer[(2, 2)].bg, Color::DarkGray);
        assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn renderer_keeps_the_parser_grid_fixed() {
        let mut parser = vt100::Parser::new(2, 3, 0);
        parser.process(b"abc\r\ndef");
        let mut terminal = Terminal::new(TestBackend::new(5, 4)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render should work");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 1)].symbol(), "f");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
    }

    #[test]
    fn renderer_uses_the_requested_scrollback_view() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let live = parser.screen().clone();
        let history = viewed_screen(parser.screen(), 1);
        assert_eq!(history.scrollback(), 1);
        assert_ne!(
            history.cell(0, 0).expect("history cell").contents(),
            live.cell(0, 0).expect("live cell").contents()
        );
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(&history), frame.area()))
            .expect("render should work");

        assert_eq!(
            terminal.backend().buffer()[(0, 0)].symbol(),
            history.cell(0, 0).expect("history cell").contents()
        );
    }

    #[test]
    fn renderer_erases_cells_cleared_by_the_pty() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");
        parser.process(b"abc");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("initial render");

        parser.process(b"\x1b[2J\x1b[H");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("clear render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), " ");
    }

    #[test]
    fn up_respects_application_cursor_mode() {
        let normal = vt100::Parser::new(1, 1, 0);
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                normal.screen(),
                false,
            ),
            Some(b"\x1b[A".to_vec())
        );

        let mut application = vt100::Parser::new(1, 1, 0);
        application.process(b"\x1b[?1h");
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                application.screen(),
                false,
            ),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn paste_respects_bracketed_paste_mode() {
        assert_eq!(encode_paste("one\ntwo", false), b"one\ntwo");
        assert_eq!(
            encode_paste("one\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }

    #[test]
    fn encodes_supported_keys_and_reserves_ctrl_q_only() {
        let parser = vt100::Parser::new(1, 1, 0);
        let screen = parser.screen();
        let cases = [
            (
                KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE),
                Some("é"),
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some("\r"),
            ),
            (KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), Some("\t")),
            (
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                Some("\x7f"),
            ),
            (
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Some("\x1b"),
            ),
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Some("\x03"),
            ),
            (
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
                Some("\x1bx"),
            ),
            (
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                Some("\x1b[H"),
            ),
            (
                KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                Some("\x1b[F"),
            ),
            (
                KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                Some("\x1b[3~"),
            ),
            (
                KeyEvent::new(KeyCode::Insert, KeyModifiers::NONE),
                Some("\x1b[2~"),
            ),
            (
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                Some("\x1b[5~"),
            ),
            (
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                Some("\x1b[6~"),
            ),
            (
                KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
                Some("\x1bOP"),
            ),
            (
                KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE),
                Some("\x1b[20~"),
            ),
            (
                KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE),
                Some("\x1b[21~"),
            ),
            (
                KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
                Some("\x1b[24~"),
            ),
            (
                KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
                Some("\x1b[1;5C"),
            ),
            (
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                None,
            ),
            (KeyEvent::new(KeyCode::Null, KeyModifiers::NONE), None),
        ];

        for (event, expected) in cases {
            assert_eq!(
                encode_key(event, screen, false).as_deref(),
                expected.map(str::as_bytes),
                "{event:?}"
            );
        }
    }

    #[test]
    fn shift_enter_uses_kitty_csi_u_when_active_and_lf_when_inactive() {
        let parser = vt100::Parser::new(1, 1, 0);
        let plain = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            parser.screen(),
            false,
        );
        let plain_active = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            parser.screen(),
            true,
        );
        let shifted_inactive = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            parser.screen(),
            false,
        );
        let shifted_active = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            parser.screen(),
            true,
        );

        assert_eq!(plain, Some(b"\r".to_vec()));
        assert_eq!(plain_active, Some(b"\r".to_vec()));
        assert_eq!(shifted_inactive, Some(b"\n".to_vec()));
        assert_eq!(shifted_active, Some(b"\x1b[13;2u".to_vec()));
        assert_ne!(shifted_inactive, plain);
        assert_ne!(shifted_active, plain);
    }
}
