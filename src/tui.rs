//! The fixed-grid local terminal renderer and input loop.

mod clock;
mod debug_log;
mod geometry;
mod input;
mod multi_pane;
mod pane;
mod render;
mod selection;
mod share;
mod snapshot;
mod state;
#[cfg(test)]
mod test_support;
mod text;

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs::OpenOptions,
    io,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

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
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};

use crate::{
    agent_detect::{AgentKind, AgentScan, AgentState, cwd_for_pid},
    kitty_keyboard::KittyKeyboardTracker,
    layout::{Axis, LayoutSnapshot, NewPanePosition, PaneId, TabId},
    lease::{IDLE_AFTER, LeaseDecision, LeaseManager, LeaseState},
    local_ipc::AgentOverlaySnapshotRow,
    protocol::{
        AgentRoster, AgentRosterEntry, AgentRosterState, CreatePane, CreateTab, DeletePane,
        DeleteTab, LayoutRequest, MAX_AGENT_CWD_BYTES, MarkPaneExited,
        NewPanePosition as ProtocolNewPanePosition, PaneDescriptor, PaneFailed, PaneReady,
        RenamePane, RenameTab, SetPaneLock, SplitAxis,
    },
    pty_host::PtyHost,
    screen::{GuestScreen, HostScreen, ScreenFrame, SyncGate},
    session::{
        CoordinatorResponse, GuestEvent, GuestPane, HostControlEvent, LayoutControlEvent,
        PaneLayoutReconciler, PaneServer, SharedLayoutHost, SharedLayoutMember,
        layout_snapshot_from_state, subscribe_pane,
    },
    transport::Transport,
};

use clock::unix_ms_now;
pub(crate) use debug_log::ui_debug_log;
use geometry::{area_from_terminal_size, contains_leaf, visible_leaf_panes};
pub(crate) use geometry::{grid_for_pane, initial_root_pane_grid};
pub use input::mouse::PaneMouseProtocol;
use input::{
    events::{
        MAX_EVENTS_PER_CYCLE, begin_synchronized_output, collect_pending_events,
        end_synchronized_output, event_poll_timeout, frame_due,
    },
    keys::{PendingEscape, encode_key, encode_paste, is_quit},
};
pub use multi_pane::MultiPaneTui;
pub use pane::local::SharedLocalPane;
use pane::{
    control::{PendingCreate, RemoteSubscriptionState, SharedControl},
    local::{AGENT_SAMPLE_INTERVAL, AGENT_WATCH_INTERVAL, AgentSamplingWorker},
    remote::{
        RemoteInput, RemotePaneDrain, SharedRemotePane, lease_allows_held_input,
        reconcile_remote_control_attempt, remote_input_decision,
    },
};
use render::panes::render_shared_multi_pane;
pub use render::panes::{render_multi_pane, render_multi_pane_with_copy_feedback};
use render::vt::{
    VtScreen, available_scrollback, render_guest_screen, render_host_screen, viewed_screen,
};
pub(crate) use selection::copy_selection_to_clipboard;
use selection::selection_text;
pub(crate) use share::{resolve_local_ticket, share_copy_result};
pub(crate) use snapshot::{
    LocalScrollbackWindow, NodeLeaseSnapshots, NodeScreenSnapshot, NodeScreenSnapshots,
};
pub use state::{
    AgentOverlayRow, ChordMode, KeyHandling, MouseHandling, PaneGeometry, PaneViewState, ShareCopy,
    ShareView, UiIntent,
};
pub(in crate::tui) use state::{
    ModalState, PaneTextSelection, RenamePrompt, RenameTarget, ScreenCell,
};
use text::{sanitize_single_line, truncate_bytes};

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

/// How long a first Ctrl+A waits for a second one before the overlay commits.
pub(crate) const AGENT_TOGGLE_WINDOW: Duration = Duration::from_millis(200);
/// How often the working glyph in the agents overlay advances.
pub(crate) const AGENT_OVERLAY_ANIMATION_INTERVAL: Duration = Duration::from_millis(100);

/// The legacy fixed-grid host/guest footer, which has no chords, agents, or share modal.
const CONTROL_HELP: &str = "Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS";

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
    share_ticket: Option<String>,
    share_notice: Option<String>,
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
            // Resolved once at startup: the record is written before the runtime exists and
            // does not change while it lives.
            share_ticket: join_code.as_deref().and_then(resolve_local_ticket),
            join_code,
            share_notice: None,
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

    /// Apply a status pushed by a producer running inside one of this node's
    /// own panes. Returns whether it was accepted.
    ///
    /// The pane id is a claim from an unauthenticated local process, so it is
    /// checked against the panes this node actually hosts. That is the local
    /// half of the containment `Coordinator::accept_agent_roster` enforces
    /// between peers: a producer can only ever speak for a pane on the machine
    /// it runs on, and the roster it feeds is published under this node's own
    /// peer id.
    ///
    /// A kind or status this build does not know is refused rather than
    /// coerced — a lenient parse would let a typo file status under the wrong
    /// agent, or blank the row it meant to update.
    pub fn apply_agent_status(
        &mut self,
        pane_id: PaneId,
        kind: &str,
        status: &str,
        cwd: &str,
    ) -> bool {
        let (Some(kind), Some(state)) = (AgentKind::from_wire(kind), AgentState::from_wire(status))
        else {
            return false;
        };
        let Some(pane) = self.local.get_mut(&pane_id) else {
            return false;
        };
        if pane.exited {
            return false;
        }
        // Capped here rather than at publish time: an over-long cwd would fail
        // `validate_agent_roster` and silently drop this host's whole roster.
        let cwd = truncate_bytes(sanitize_single_line(cwd), MAX_AGENT_CWD_BYTES);
        pane.agent_tracker
            .record_pushed_status(kind, cwd, state, Instant::now(), unix_ms_now());
        true
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
                if let Some(request) = self.tui.take_share_copy_request() {
                    self.share_notice = Some(share_copy_result(
                        request,
                        self.share_ticket.as_deref(),
                        self.join_code.as_deref(),
                    ));
                }
                // The notice belongs to one visit to the modal, not to the session.
                if !self.tui.share_open() {
                    self.share_notice = None;
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
                        ShareView {
                            code: self.join_code.as_deref(),
                            ticket: self.share_ticket.as_deref(),
                            notice: self.share_notice.as_deref(),
                        },
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
                        let handling = self.tui.handle_mouse(mouse, area, protocol);
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
                let handling = self.tui.handle_mouse(mouse, area, protocol);
                if let Some(bytes) = handling.forward_bytes {
                    self.forward_mouse(bytes)?;
                }
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.copied_lines = None;
                }
                for intent in handling.intents {
                    self.handle_intent(intent)?;
                }
                if handling.copy_selection_requested {
                    self.copy_selection_to_clipboard();
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
                    && let Some(bytes) = self.tui.handle_mouse(mouse, area, protocol).forward_bytes
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
            // One scan for the whole session, not one per pane.
            let scan = AgentScan::new(&snapshot);
            let mut inferred_agents = false;
            for pane in self.local.values_mut() {
                if !pane.exited {
                    changed |= pane.apply_agent_snapshot(&scan, now);
                    inferred_agents |= pane.agent_state_is_inferred();
                }
            }
            self.agent_sampler.set_interval(if inferred_agents {
                AGENT_SAMPLE_INTERVAL
            } else {
                AGENT_WATCH_INTERVAL
            });
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
                        let claiming_free_pane = state.controller_peer_id.is_empty();
                        match remote_input_decision(
                            &state.controller_peer_id,
                            pane.controls.peer_id(),
                            pending_control,
                            held_input.is_empty(),
                            last_lease.elapsed() >= IDLE_AFTER,
                        ) {
                            RemoteInput::Send => {
                                if pane.controls.try_input(state.lease_epoch, bytes).is_ok()
                                    && claiming_free_pane
                                {
                                    pending_control = true;
                                }
                            }
                            RemoteInput::Hold => held_input.extend_from_slice(&bytes),
                            RemoteInput::Request => {
                                held_input.extend_from_slice(&bytes);
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                            RemoteInput::Ignore => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen()) {
                        let bytes = encode_paste(&text, screen.bracketed_paste());
                        let claiming_free_pane = state.controller_peer_id.is_empty();
                        match remote_input_decision(
                            &state.controller_peer_id,
                            pane.controls.peer_id(),
                            pending_control,
                            held_input.is_empty(),
                            last_lease.elapsed() >= IDLE_AFTER,
                        ) {
                            RemoteInput::Send => {
                                if pane.controls.try_input(state.lease_epoch, bytes).is_ok()
                                    && claiming_free_pane
                                {
                                    pending_control = true;
                                }
                            }
                            RemoteInput::Hold => held_input.extend_from_slice(&bytes),
                            RemoteInput::Request => {
                                held_input.extend_from_slice(&bytes);
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                            RemoteInput::Ignore => {}
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
        fs,
        net::Ipv4Addr,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::{mpsc, watch};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use portable_pty::PtySize;
    use ratatui::layout::Rect;

    use crate::agent_detect::cwd_for_pid;
    use crate::layout::{Axis, NewPanePosition};
    use crate::lease::{LeaseManager, LeaseState};
    use crate::screen::HostScreen;
    use crate::{
        protocol::PaneDescriptor,
        session::{
            HostPaneChannels, HostSession, SharedLayoutHost, layout_snapshot_from_state,
            pane_wire_id,
        },
        transport::{ALPN, Transport},
    };
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    use super::{
        HostPaneRuntime, LayoutControlEvent, SharedLayoutRuntime, SharedLocalPane, UiIntent,
        member_label,
    };

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
    async fn pushed_agent_status_is_confined_to_panes_this_node_hosts() {
        use crate::agent_detect::{AgentKind, AgentState};
        use crate::protocol::MAX_AGENT_CWD_BYTES;

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

        assert!(
            runtime.apply_agent_status(1, "claude", "pending", "/repo"),
            "a producer may report for a pane this node hosts"
        );
        assert_eq!(
            runtime
                .local
                .get_mut(&1)
                .expect("local pane")
                .agent_tracker
                .listed_agent(Instant::now(), 1_000)
                .map(|(agent, state)| (agent.kind, state)),
            Some((AgentKind::Claude, AgentState::Pending))
        );

        // A pane id this node does not host is refused outright. This is the
        // local half of `Coordinator::accept_agent_roster`: without it, any
        // process on this machine could publish status for a peer's pane under
        // this node's authenticated peer id.
        assert!(
            !runtime.apply_agent_status(99, "claude", "pending", "/repo"),
            "a producer may not report for a pane hosted elsewhere"
        );

        // Unparseable kinds and statuses are refused rather than coerced.
        assert!(!runtime.apply_agent_status(1, "gemini", "pending", "/repo"));
        assert!(!runtime.apply_agent_status(1, "claude", "pendign", "/repo"));

        // An over-long cwd is cut at intake: letting it through would fail
        // `validate_agent_roster` and drop this host's entire roster.
        assert!(runtime.apply_agent_status(
            1,
            "claude",
            "working",
            &"/x".repeat(MAX_AGENT_CWD_BYTES)
        ));
        let cwd = runtime
            .local
            .get(&1)
            .and_then(|pane| pane.agent_tracker.pushed.as_ref())
            .map(|pushed| pushed.cwd.clone())
            .expect("pushed status");
        assert!(cwd.len() <= MAX_AGENT_CWD_BYTES);
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
}
