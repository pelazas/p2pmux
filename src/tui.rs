//! The fixed-grid local terminal renderer and input loop.

mod clock;
mod debug_log;
mod geometry;
mod input;
mod multi_pane;
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
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self as sync_mpsc, Receiver},
    },
    thread::{self, JoinHandle},
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
    agent_detect::{
        AgentKind, AgentScan, AgentState, PaneAgentTracker, ProcessSnapshot, SysinfoSampler,
        cwd_for_pid, sample_global_snapshot,
    },
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
        CoordinatorResponse, GuestEvent, GuestPane, HostControlEvent, HostPaneChannels,
        LayoutControlEvent, LayoutControlQueueError, PaneLayoutReconciler, PaneServer,
        SharedLayoutHost, SharedLayoutMember, layout_snapshot_from_state, pane_wire_id,
        subscribe_pane,
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

/// Scan cadence while any pane's agent state is being inferred from output
/// timing. Inference has to notice silence promptly, so this is the floor.
const AGENT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Scan cadence when nothing needs inference — no agents at all, or every agent
/// reporting through a hook. The scan is then only watching for a process to
/// appear or exit, and a full `sysinfo` refresh of every process on the machine
/// once a second to do that is most of this process's idle cost.
const AGENT_WATCH_INTERVAL: Duration = Duration::from_secs(5);

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
        let host = match PtyHost::spawn_default_shell_with_cwd(size, cwd, Some(pane_id)) {
            Ok(host) => host,
            Err(_) if cwd.is_some_and(|path| !path.is_dir()) => {
                PtyHost::spawn_default_shell_with_cwd(size, None, Some(pane_id))?
            }
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

    fn apply_agent_snapshot(&mut self, scan: &AgentScan<'_>, now: Instant) -> bool {
        let unix_ms_now = unix_ms_now();
        let before = self.agent_tracker.listed_agent(now, unix_ms_now);
        let session_child = self.host.process_id();
        let detected = session_child.and_then(|pid| scan.classify(pid));
        self.agent_tracker.update(detected, now, unix_ms_now);
        self.agent_tracker
            .observe_pane_liveness(session_child.is_some_and(|pid| scan.has_children(pid)), now);
        self.agent_tracker.listed_agent(now, unix_ms_now) != before
    }

    /// Whether this pane's reported state comes from output-timing inference
    /// rather than a producer inside it — the only case that needs the fast
    /// sampler cadence, since only inference has to notice silence promptly.
    fn agent_state_is_inferred(&self) -> bool {
        self.agent_tracker.active_agent.is_some() && !self.agent_tracker.has_owning_push()
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
                crate::agent_detect::AgentState::Pending => AgentRosterState::Pending as i32,
                crate::agent_detect::AgentState::Error => AgentRosterState::Error as i32,
            },
            working_since_unix_ms: if state == crate::agent_detect::AgentState::Working {
                self.agent_tracker.reported_working_since_unix_ms()
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
    interval_ms: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

impl AgentSamplingWorker {
    fn spawn() -> Self {
        let (snapshot_tx, snapshots) = sync_mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let interval_ms = Arc::new(AtomicU64::new(AGENT_SAMPLE_INTERVAL.as_millis() as u64));
        let worker_stop = stop.clone();
        let worker_interval = interval_ms.clone();
        let join = thread::spawn(move || {
            let mut sampler = SysinfoSampler::default();
            while !worker_stop.load(Ordering::Relaxed) {
                if snapshot_tx
                    .send(sample_global_snapshot(&mut sampler))
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(
                    worker_interval.load(Ordering::Relaxed),
                ));
            }
        });
        Self {
            snapshots,
            stop,
            interval_ms,
            join: Some(join),
        }
    }

    /// Set how often the global process scan runs.
    ///
    /// The scan is the single most expensive thing this process does when
    /// nothing is happening — a full `sysinfo` refresh of every process on the
    /// machine, with exe, cmdline and cwd. It only needs the fast cadence when
    /// a pane's state is being *inferred* from output timing, which is the one
    /// case that depends on noticing silence promptly. Panes whose agent
    /// reports through a hook, and panes with no agent at all, only need the
    /// scan for liveness and for spotting a new launch.
    fn set_interval(&self, interval: Duration) {
        self.interval_ms
            .store(interval.as_millis() as u64, Ordering::Relaxed);
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

/// What a guest should do with one keystroke aimed at a pane hosted by someone else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteInput {
    /// Send it now, stamped with the lease epoch this guest currently knows.
    Send,
    /// Append it to `held_input`: a lease change of ours is already in flight, and
    /// anything sent before the answer arrives would be stamped with a dead epoch.
    Hold,
    /// Ask the host for the pane, then hold this keystroke until it answers.
    Request,
    /// Somebody else is actively typing here. Their pane is protected.
    Ignore,
}

/// The guest-side input rule, in one place because two loops need it and they had
/// drifted apart.
///
/// The subtle case is `controller_peer_id` being empty -- a free pane, claimed by
/// typing into it. That claim is not free: the host bumps the lease epoch the moment it
/// accepts the first byte, so every keystroke typed during the round trip that follows
/// arrives stamped with an epoch the host has already left behind and is rejected as
/// stale. On loopback the window is under a millisecond and no scenario ever caught it;
/// across an 85ms internet path it silently swallows the next five characters, so
/// `echo N-CROSS` reaches the shell as `e-CROSS`. Holding the rest of the burst until
/// the new lease lands costs nothing and keeps the line intact.
fn remote_input_decision(
    controller_peer_id: &[u8],
    peer_id: &[u8],
    pending_control: bool,
    held_input_empty: bool,
    controller_idle: bool,
) -> RemoteInput {
    if controller_peer_id == peer_id {
        return if held_input_empty {
            RemoteInput::Send
        } else {
            RemoteInput::Hold
        };
    }
    if controller_peer_id.is_empty() {
        return if pending_control {
            RemoteInput::Hold
        } else {
            RemoteInput::Send
        };
    }
    if controller_idle {
        return if pending_control {
            RemoteInput::Hold
        } else {
            RemoteInput::Request
        };
    }
    RemoteInput::Ignore
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
        let claiming_free_pane = lease.controller_peer_id.is_empty();
        match remote_input_decision(
            &lease.controller_peer_id,
            self.pane.controls.peer_id(),
            self.pending_control,
            self.held_input.is_empty(),
            self.last_lease.elapsed() >= IDLE_AFTER,
        ) {
            RemoteInput::Send => {
                if self.pane.controls.try_input(lease.epoch, bytes).is_ok() && claiming_free_pane {
                    // The host will answer this byte with a new epoch; hold the rest of
                    // the burst until it does.
                    self.pending_control = true;
                }
            }
            RemoteInput::Hold => self.held_input.extend_from_slice(&bytes),
            RemoteInput::Request => {
                self.held_input.extend_from_slice(&bytes);
                self.pending_control = self.pane.controls.try_take_control(lease.epoch).is_ok();
                if !self.pending_control {
                    self.held_input.clear();
                }
            }
            RemoteInput::Ignore => {}
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
        collections::BTreeMap,
        fs,
        net::Ipv4Addr,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::{mpsc, watch};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use portable_pty::PtySize;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    use crate::layout::{Axis, LayoutError, NewPanePosition, Node, Tab};
    use crate::lease::{IDLE_AFTER, LeaseManager, LeaseState};
    use crate::screen::HostScreen;
    use crate::{agent_detect::cwd_for_pid, config::UiTheme};
    use crate::{
        protocol::PaneDescriptor,
        session::{HostSession, SharedLayoutHost, layout_snapshot_from_state},
        transport::{ALPN, Transport},
    };
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    use super::input::keys::{ESC_PREFIX_WINDOW, is_chord_command, is_chord_navigation};
    use super::multi_pane::CHORD_IDLE_TIMEOUT;
    use super::test_support::{agent_overlay_tui, agent_row, layout, mouse_protocol, split_layout};
    use super::{
        AGENT_TOGGLE_WINDOW, ChordMode, HostControlEvent, HostPaneChannels, HostPaneRuntime,
        KeyHandling, LayoutControlEvent, MultiPaneTui, PaneMouseProtocol, PaneTextSelection,
        PaneViewState, PendingEscape, RemoteInput, RemoteSubscriptionState, ScreenCell, ShareCopy,
        SharedLayoutRuntime, SharedLocalPane, UiIntent, grid_for_pane, initial_root_pane_grid,
        lease_allows_held_input, member_label, pane_wire_id, reconcile_remote_control_attempt,
        remote_input_decision, render_multi_pane, selection_text, viewed_screen,
    };

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
            super::render::agents::format_agent_overlay_card(
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
        let inner = super::render::agents::agents_overlay_inner(area);
        let content = super::render::agents::agents_overlay_content(area);
        let help = super::render::agents::agents_overlay_help(area).expect("help row");
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
        let content = super::render::agents::agents_overlay_content(area);

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

    #[test]
    fn claiming_a_free_remote_pane_holds_the_rest_of_the_burst() {
        let peer = b"typist";

        // The first keystroke goes out and claims the pane.
        assert_eq!(
            remote_input_decision(b"", peer, false, true, false),
            RemoteInput::Send
        );
        // Everything typed during the round trip has to wait: the host has already
        // bumped the epoch, so sending now means the host drops those bytes as stale.
        // This is the whole bug -- invisible on loopback, five lost characters at 85ms.
        assert_eq!(
            remote_input_decision(b"", peer, true, true, false),
            RemoteInput::Hold
        );
    }

    #[test]
    fn remote_input_respects_the_controller_lease() {
        let peer = b"typist";

        assert_eq!(
            remote_input_decision(peer, peer, false, true, false),
            RemoteInput::Send,
            "the controller types straight through"
        );
        assert_eq!(
            remote_input_decision(peer, peer, false, false, false),
            RemoteInput::Hold,
            "queued input keeps its place in line"
        );
        assert_eq!(
            remote_input_decision(b"other", peer, false, true, false),
            RemoteInput::Ignore,
            "an active controller is protected from takeover"
        );
        assert_eq!(
            remote_input_decision(b"other", peer, false, true, true),
            RemoteInput::Request,
            "an idle controller can be displaced"
        );
        assert_eq!(
            remote_input_decision(b"other", peer, true, true, true),
            RemoteInput::Hold,
            "one request at a time; the rest of the burst waits for the answer"
        );
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

    fn left_mouse(kind: MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
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
            protocol,
        );
        let drag = tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 4, 3),
            area,
            protocol,
        );
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 4, 3),
            area,
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
            protocol,
        );
        let drag = tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 70, 20),
            area,
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

        let handling = tui.handle_mouse(shift_down, area, reporting_child());

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
            protocol,
        );
        tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 49, 5),
            area,
            protocol,
        );
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 49, 5),
            area,
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
            tui.handle_mouse(down, area, PaneMouseProtocol::default())
                .intents
                .is_empty()
        );
        tui.handle_mouse(drag, area, PaneMouseProtocol::default());
        let handling = tui.handle_mouse(up, area, PaneMouseProtocol::default());
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
        tui.handle_mouse(resize_down, area, PaneMouseProtocol::default());
        tui.handle_mouse(resize_drag, area, PaneMouseProtocol::default());
        let handling = tui.handle_mouse(resize_up, area, PaneMouseProtocol::default());
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
            tui.handle_mouse(mouse, area, PaneMouseProtocol::default()),
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
    fn ctrl_s_toggles_the_share_modal_and_leaves_any_chord_mode() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Consumed(vec![]),
            "share is a mux command and never reaches the PTY"
        );
        assert!(tui.share_open());
        assert_eq!(tui.chord_mode(), ChordMode::None);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            area,
        );
        assert!(!tui.share_open(), "Ctrl+S closes what it opened");
    }

    #[test]
    fn share_modal_claims_each_copy_once_and_closes_on_escape() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            area,
        );

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area);
        assert_eq!(tui.take_share_copy_request(), Some(ShareCopy::Ticket));
        assert_eq!(
            tui.take_share_copy_request(),
            None,
            "a claimed request must not copy again on the next key"
        );

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), area);
        assert_eq!(tui.take_share_copy_request(), Some(ShareCopy::Code));

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area);
        assert!(!tui.share_open());
    }

    #[test]
    fn plain_share_key_reaches_the_pty_without_control() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert!(!tui.share_open());
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
}
