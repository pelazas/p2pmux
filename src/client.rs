//! Terminal-facing half of a local session attachment.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};

use crate::{
    local_ipc::{ClientMessage, NodeMessage, ScreenUpdate},
    protocol::AgentRosterState,
    screen::{ApplyDelta, GuestScreen},
    session_store::SessionDescriptor,
    tui::{
        AGENT_OVERLAY_ANIMATION_INTERVAL, AgentOverlayRow, KeyHandling, MultiPaneTui,
        PaneMouseProtocol, PaneViewState, QuitAction, ShareView, clear_before_first_frame,
        copy_selection_to_clipboard, render_multi_pane_with_copy_feedback, resize_recheck_due,
        share_copy_result, stale_node_size,
    },
};

const IPC_BATCH_PER_WAKE: usize = 32;

#[derive(Default)]
struct HistoryCache {
    history_id: Option<u64>,
    total_rows: u64,
    grid: Option<(u16, u16)>,
    viewports: BTreeMap<usize, GuestScreen>,
}

impl HistoryCache {
    fn install_viewport(
        &mut self,
        history_id: u64,
        total_rows: u64,
        offset: usize,
        payload: &[u8],
    ) -> Result<(), crate::screen::ScreenError> {
        let mut viewport = GuestScreen::new();
        viewport.apply_snapshot(1, payload)?;
        let grid = viewport.screen().expect("decoded viewport").size();
        self.history_id = Some(history_id);
        self.grid = Some(grid);
        self.total_rows = total_rows;
        self.viewports.insert(offset, viewport);
        Ok(())
    }

    fn available_rows(&self) -> usize {
        self.total_rows as usize
    }
}

fn copy_attach_selection(
    tui: &MultiPaneTui,
    screens: &BTreeMap<u64, GuestScreen>,
    history: &BTreeMap<u64, HistoryCache>,
) -> Option<usize> {
    let pane_id = tui.selection_pane()?;
    // A client holds no scrollback of its own — it asks the node for a viewport
    // at a time and keeps what comes back. That cache is exactly the set of
    // offsets the selection can reach: dragging past the top scrolled through
    // every one of them to get there, and each of those steps fetched its rows.
    let text = tui.selected_text(|offset| {
        if offset == 0 {
            return screens.get(&pane_id)?.screen().map(Cow::Borrowed);
        }
        history
            .get(&pane_id)?
            .viewports
            .get(&offset)
            .and_then(GuestScreen::screen)
            .map(Cow::Borrowed)
    })?;
    copy_selection_to_clipboard(&text).ok()
}

#[derive(Clone, Copy)]
struct PendingScroll {
    request_id: u64,
    target: usize,
}

/// Claims the outstanding request for `pane_id` only when this reply answers it.
///
/// The obvious spelling — remove, then compare — throws away a live request every
/// time a stale reply lands first, and the reply that would have satisfied it then
/// finds nothing to claim. A wheel burst is exactly the case that produces stale
/// replies, so scrolling would stall precisely when it was asked to move most.
fn take_matching_pending(
    pending: &mut BTreeMap<u64, PendingScroll>,
    pane_id: u64,
    request_id: u64,
) -> Option<PendingScroll> {
    if pending.get(&pane_id)?.request_id != request_id {
        return None;
    }
    pending.remove(&pane_id)
}

/// Give up on a pane's history and put it back at its live edge.
///
/// The node answers with no window for a pane it does not host, for one on the
/// alternate screen, for a frozen session it has since evicted, and for one
/// that has no history yet — and the third of those can arrive for a pane that
/// is *already* parked in history.
///
/// Only the first three carry a reason to show. A pane nothing has scrolled off
/// yet answers with none, and `footer_notice` is assigned it unconditionally
/// below: a wheel notch that found no history is not news, and the bar should
/// keep whatever it was already saying rather than flash an error at a shell
/// that has been alive for one second.
/// Everything cached for it goes, so what the pane falls back to is its live
/// screen; leaving the offset behind would leave it reading as scrolled while
/// showing the newest output, which costs it its caret and swallows the next
/// several notches of wheel-down.
fn abandon_history(
    tui: Option<&mut MultiPaneTui>,
    history: &mut BTreeMap<u64, HistoryCache>,
    desired_scroll: &mut BTreeMap<u64, usize>,
    pane_id: u64,
) {
    desired_scroll.remove(&pane_id);
    history.remove(&pane_id);
    if let Some(tui) = tui {
        tui.set_pane_scrollback_offset(pane_id, 0);
    }
}

const PANE_SCROLL_WHEEL_STEP: usize = 3;

/// Rows pulled in per step by a selection dragged past a pane's edge.
///
/// One, unlike the wheel's three. Every step fetches its own viewport from the
/// node, and those viewports are what the copy is assembled from: a step of
/// three would leave two rows out of every three with no viewport to read.
const SELECTION_AUTOSCROLL_STEP: usize = 1;

/// Where one wheel notch should land, measured from the furthest point already asked
/// for rather than from what is on screen.
///
/// Reaching history costs a round trip to the node, so mid-burst the visible offset is
/// always several notches behind the wheel. Stepping from it made every notch of a
/// flick ask for the same row, so a gesture worth sixty rows travelled three — and each
/// of those identical queries still cost the node a full scrollback viewport.
fn next_scroll_target(
    visible: usize,
    requested: Option<usize>,
    max_rows: usize,
    up: bool,
    step: usize,
) -> usize {
    let base = requested.unwrap_or(visible);
    if up {
        base.saturating_add(step).min(max_rows)
    } else {
        base.saturating_sub(step)
    }
}

/// Move a pane's viewport `step` rows, fetching the rows if they are not here.
///
/// A client keeps no scrollback: the node holds it and hands over one viewport
/// at a time. So a scroll is either instant — the rows are cached, or it is a
/// return to the live edge — or a request whose reply moves the viewport later.
/// Both the wheel and a selection dragged past a pane's edge go through here,
/// which is what keeps the fetched viewports the drag scrolled through in the
/// cache that the eventual copy reads from.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site's locals, threaded"
)]
fn scroll_pane_toward(
    stream: &mut UnixStream,
    tui: &mut MultiPaneTui,
    history: &mut BTreeMap<u64, HistoryCache>,
    desired_scroll: &mut BTreeMap<u64, usize>,
    pending_scroll: &mut BTreeMap<u64, PendingScroll>,
    next_scrollback_request_id: &mut u64,
    pane_id: u64,
    up: bool,
    step: usize,
) -> io::Result<()> {
    let scrollback_len = history
        .get(&pane_id)
        .map(HistoryCache::available_rows)
        .unwrap_or(1_000);
    let target = next_scroll_target(
        tui.pane_scrollback_offset(pane_id),
        desired_scroll
            .get(&pane_id)
            .copied()
            .or_else(|| pending_scroll.get(&pane_id).map(|pending| pending.target)),
        scrollback_len,
        up,
        step,
    );
    let held = target == 0
        || history
            .get(&pane_id)
            .is_some_and(|history| history.viewports.contains_key(&target));
    if held {
        // The rows are already here, or the wheel is back at the live
        // edge. Move now and abandon whatever the burst had queued.
        desired_scroll.remove(&pane_id);
        pending_scroll.remove(&pane_id);
        tui.set_pane_scrollback_offset(pane_id, target);
        if target == 0 {
            history.remove(&pane_id);
        }
    } else if pending_scroll.contains_key(&pane_id) {
        desired_scroll.insert(pane_id, target);
    } else {
        desired_scroll.remove(&pane_id);
        request_scrollback(
            stream,
            pane_id,
            target,
            history,
            pending_scroll,
            next_scrollback_request_id,
        )?;
    }
    Ok(())
}

/// Which screen an attaching client lands on.
///
/// The inbox is for arriving somewhere that was already running: the question
/// it answers is "which of my agents needs me", and a session with other
/// machines in it has an answer worth reading before anything else. Bare
/// `p2pmux` opens on it when it rejoins a fleet session for that reason.
///
/// Everything that *starts* a session lands in the terminal instead — `create`,
/// `join`, `attach`, and bare `p2pmux` on a machine with no fleet. A session one
/// second old has a single pane and no agents, so its inbox is an empty list,
/// and opening on an empty list is indistinguishable from opening on nothing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StartScreen {
    #[default]
    Session,
    Home,
}

/// A node turning this client away at the door, carrying its reason verbatim.
///
/// Typed rather than folded into `io::Error` so a caller can tell the one
/// refusal it can recover from — the session already has a terminal in it —
/// from every other way attaching fails. Matching on the message text would
/// have worked until somebody reworded it.
#[derive(Debug)]
pub struct AttachRejected(String);

impl std::fmt::Display for AttachRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AttachRejected {}

/// Whether this is a node refusing a second client, rather than a session that
/// is genuinely broken.
///
/// A node serves one terminal at a time, so this is the ordinary state of every
/// session a user already has open — not a fault, and not something to end a
/// command over.
pub fn is_already_attached(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<AttachRejected>()
        .is_some_and(|rejected| rejected.0 == crate::local_ipc::ALREADY_ATTACHED)
}

pub fn run(descriptor: &SessionDescriptor) -> Result<(), Box<dyn std::error::Error>> {
    run_on(descriptor, StartScreen::Session)
}

pub fn run_on(
    descriptor: &SessionDescriptor,
    start: StartScreen,
) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = crate::config::config_path()?;
    let config = crate::config::load_config_from(&config_path).map_err(|error| {
        io::Error::other(format!(
            "could not load config {}: {error}",
            config_path.display()
        ))
    })?;
    let theme = config.ui.theme;
    let mut stream = UnixStream::connect(&descriptor.socket_path)?;
    let read_stream = stream.try_clone()?;
    let mut reader = BufReader::new(read_stream);
    let (initial_cols, initial_rows) = terminal::size()?;
    write_message(
        &mut stream,
        &ClientMessage::Hello {
            cols: initial_cols,
            rows: initial_rows,
        },
    )?;
    let generation = match read_message(&mut reader)? {
        Some(NodeMessage::AttachAccepted { generation }) => generation,
        Some(NodeMessage::AttachRejected { reason }) => return Err(AttachRejected(reason).into()),
        _ => return Err(io::Error::other("node did not accept attachment").into()),
    };
    let (wake_tx, wakes) = mpsc::channel();
    let reader_thread = spawn_message_reader(reader, wake_tx.clone());
    let terminal_stop = Arc::new(AtomicBool::new(false));
    let mut guard = ClientTerminalGuard::enter(&descriptor.name)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, initial_cols, initial_rows)),
        },
    )?;
    clear_before_first_frame(&mut terminal, Rect::new(0, 0, initial_cols, initial_rows))?;
    let terminal_thread = spawn_terminal_reader(wake_tx, Arc::clone(&terminal_stop));
    let mut tui = None;
    let mut screens = BTreeMap::new();
    let mut history = BTreeMap::new();
    let mut pending_scroll: BTreeMap<u64, PendingScroll> = BTreeMap::new();
    // Where the wheel has reached for panes whose query is still out. Holding it here
    // keeps one query per pane in flight, so a burst costs the node one viewport
    // instead of one per notch.
    let mut desired_scroll: BTreeMap<u64, usize> = BTreeMap::new();
    let mut next_scrollback_request_id = 1_u64;
    let mut copied_lines = None;
    let mut footer_notice = None;
    let mut link_summary: Option<String> = None;
    let mut dirty = false;
    let mut node_ended = false;
    let mut attach_error = None;
    let mut detach_sent = false;
    let mut killed = false;
    let mut last_agent_overlay_animation = Instant::now();
    // Started before the attach rather than after it, so the answer is usually
    // already waiting by the first frame — and started here rather than in the
    // node, because the notice is for the person sitting at this terminal.
    let update_notices = crate::update_check::spawn();
    let mut pending_focus = None;
    let mut pending_resync = BTreeSet::new();
    let mut history_refresh = BTreeSet::new();
    let mut local_peer_id = Vec::new();
    // Re-read when the file changes, and only then. Pairing is written by a
    // different process -- `p2pmux pair`, `p2pmux enroll`, or the node writing a
    // machine down when it arrives -- so a client that read it once at attach
    // was wrong for the rest of its life about every machine paired after it
    // started. That is not an edge case: pairing while a session is open is the
    // ordinary way to add a machine, and the fleet you just added came back as
    // "someone else's machine" until the session was restarted.
    //
    // Parsing it several times a second would be silly, and is not what this
    // does: it stats the file and re-reads only when the mtime moves.
    let mut paired_machines = crate::pairing::PairedMachineWatch::new();
    let mut started_on_home = false;
    // Carried on the snapshot rather than looked up here: only the node holds the ticket,
    // and a member's node has none to send.
    let mut share_ticket: Option<String> = None;
    let mut share_code: Option<String> = None;
    let mut share_notice: Option<String> = None;
    let mut pending_wake = None;
    let mut next_perf_id = 1_u64;
    let mut draw_perf_id = None;
    // The viewport is fixed, so what is drawn only follows the window because
    // the resize arm below resizes it. Tracked here to spot a resize that never
    // arrived as an event.
    let mut viewport = (initial_cols, initial_rows);
    // The size the node last heard, which `Hello` above has just told it. Tracked apart
    // from `viewport` because the two come apart whenever a resize is drawn but not
    // forwarded, and that gap is the only evidence the node is behind.
    let mut node_viewport = (initial_cols, initial_rows);
    let mut last_size_check: Option<Instant> = None;

    'attached: loop {
        for _ in 0..IPC_BATCH_PER_WAKE {
            let wake = pending_wake
                .take()
                .map(Ok)
                .unwrap_or_else(|| wakes.try_recv());
            match wake {
                Ok(WakeEvent::Ipc(ReaderEvent::Message(message))) => match *message {
                    NodeMessage::Snapshot {
                        room_name,
                        layout,
                        screens: next_screens,
                        leases,
                        rosters,
                        presence,
                        local_peer_id: next_local_peer_id,
                        tab_id,
                        pane_id,
                        ticket: next_ticket,
                        code: next_code,
                        ..
                    } => {
                        let apply_started = Instant::now();
                        let view = apply_layout(
                            &mut tui,
                            theme,
                            &mut screens,
                            &mut history,
                            room_name,
                            *layout,
                        )?;
                        let resync = apply_screens(
                            view,
                            &mut screens,
                            &mut history,
                            next_screens,
                            &mut pending_resync,
                            &mut history_refresh,
                        )?;
                        apply_leases(view, leases);
                        view.set_presence(presence);
                        apply_focus(view, &mut pending_focus, tab_id, pane_id)?;
                        announce_agent_completions(apply_rosters(view, rosters));
                        let apply_elapsed = apply_started.elapsed();
                        if crate::perf::enabled() && apply_elapsed >= Duration::from_millis(5) {
                            crate::perf::log(&format!(
                                "P2PMUX_PERF client apply_ms={} panes={}",
                                apply_elapsed.as_millis(),
                                screens.len(),
                            ));
                        }
                        for pane_id in new_resync_requests(&mut pending_resync, resync) {
                            write_message(&mut stream, &ClientMessage::ResyncScreen { pane_id })?;
                        }
                        for pane_id in std::mem::take(&mut history_refresh) {
                            request_scrollback(
                                &mut stream,
                                pane_id,
                                view.pane_scrollback_offset(pane_id),
                                &history,
                                &mut pending_scroll,
                                &mut next_scrollback_request_id,
                            )?;
                        }
                        local_peer_id = next_local_peer_id;
                        share_ticket = next_ticket;
                        share_code = next_code;
                        if let Some(tui) = tui.as_mut() {
                            tui.set_home_viewport_for(terminal.size()?.into());
                            // The machine list needs to know which row is the
                            // machine you are sitting at, and only the node can
                            // say which peer this client is.
                            tui.set_local_peer_id(local_peer_id.clone());
                            tui.set_paired_machines(paired_machines.machines().to_vec());
                            // The node outlives this client, so here — and only
                            // here — leaving and ending the session are two
                            // different things worth asking about.
                            tui.set_detachable(true);
                            // Deferred to the first snapshot rather than done at
                            // construction: the view does not exist until a
                            // layout arrives, and landing on Home is a decision
                            // about the first frame, not about every one.
                            if start == StartScreen::Home && !started_on_home {
                                started_on_home = true;
                                tui.open_home_on_start();
                            }
                        }
                        dirty = true;
                    }
                    NodeMessage::Layout { layout } => {
                        apply_layout(
                            &mut tui,
                            theme,
                            &mut screens,
                            &mut history,
                            String::new(),
                            *layout,
                        )?;
                        dirty = true;
                    }
                    NodeMessage::Screens {
                        screens: next_screens,
                        perf_id,
                    } => {
                        let view = tui.as_mut().ok_or_else(|| {
                            io::Error::other("screens received before attachment snapshot")
                        })?;
                        let resync = apply_screens(
                            view,
                            &mut screens,
                            &mut history,
                            next_screens,
                            &mut pending_resync,
                            &mut history_refresh,
                        )?;
                        for pane_id in new_resync_requests(&mut pending_resync, resync) {
                            write_message(&mut stream, &ClientMessage::ResyncScreen { pane_id })?;
                        }
                        for pane_id in std::mem::take(&mut history_refresh) {
                            request_scrollback(
                                &mut stream,
                                pane_id,
                                view.pane_scrollback_offset(pane_id),
                                &history,
                                &mut pending_scroll,
                                &mut next_scrollback_request_id,
                            )?;
                        }
                        dirty = true;
                        if let Some(perf_id) = perf_id {
                            crate::perf::log(&format!("P2PMUX_PERF id={perf_id} client_apply"));
                            draw_perf_id = Some(perf_id);
                        }
                    }
                    NodeMessage::ScrollbackWindow {
                        pane_id,
                        request_id,
                        history_id,
                        total_rows,
                        offset,
                        snapshot,
                        unavailable,
                    } => {
                        let Some(pending) =
                            take_matching_pending(&mut pending_scroll, pane_id, request_id)
                        else {
                            continue;
                        };
                        let Some(snapshot) = snapshot else {
                            // No history to reach: anything the burst queued behind this is
                            // unreachable too, and the cached session goes with it. That
                            // answer is also what a node gives for a history id it no
                            // longer holds, and keeping that dead id would put it on every
                            // later query -- one evicted session and the pane could never
                            // be scrolled again.
                            abandon_history(
                                tui.as_mut(),
                                &mut history,
                                &mut desired_scroll,
                                pane_id,
                            );
                            footer_notice = unavailable;
                            dirty = true;
                            continue;
                        };
                        let cache = history.entry(pane_id).or_default();
                        if cache.history_id.is_some_and(|id| id != history_id) {
                            *cache = HistoryCache::default();
                        }
                        cache.install_viewport(
                            history_id,
                            total_rows,
                            offset as usize,
                            &snapshot,
                        )?;
                        if let Some(view) = tui.as_mut() {
                            view.set_pane_scrollback_offset(
                                pane_id,
                                pending.target.min(total_rows as usize),
                            );
                        }
                        dirty = true;
                        // Notches that arrived while this query was out were folded into a
                        // single desired offset. Settle it now: one follow-up for wherever
                        // the wheel actually ended up, not one per notch.
                        if let Some(target) = desired_scroll.remove(&pane_id) {
                            let target = target.min(total_rows as usize);
                            if history
                                .get(&pane_id)
                                .is_some_and(|cache| cache.viewports.contains_key(&target))
                            {
                                if let Some(view) = tui.as_mut() {
                                    view.set_pane_scrollback_offset(pane_id, target);
                                }
                            } else {
                                request_scrollback(
                                    &mut stream,
                                    pane_id,
                                    target,
                                    &history,
                                    &mut pending_scroll,
                                    &mut next_scrollback_request_id,
                                )?;
                            }
                        }
                    }
                    NodeMessage::Leases { leases } => {
                        if let Some(view) = tui.as_mut() {
                            apply_leases(view, leases);
                            dirty = true;
                        }
                    }
                    NodeMessage::Status { message } => {
                        // The node clears its status by publishing an empty string, so an
                        // empty message must retract the notice rather than blank-flash it.
                        footer_notice = (!message.is_empty()).then_some(message);
                        dirty = true;
                    }
                    NodeMessage::SessionLock { locked } => {
                        if let Some(view) = tui.as_mut() {
                            view.set_session_locked(locked);
                            dirty = true;
                        }
                    }
                    NodeMessage::RemoteWork { command } => {
                        if let Some(view) = tui.as_mut() {
                            match command {
                                Some(command) => view.ask_remote_work(&command),
                                // Withdrawn: answered by someone else, or the
                                // reservation ran out. Either way the question
                                // is no longer live and must come off the
                                // screen rather than sit there granting
                                // nothing.
                                None => view.close_remote_work(),
                            }
                            dirty = true;
                        }
                    }
                    NodeMessage::Paths { paths } => {
                        // An empty list means every peer disconnected, which must clear
                        // the badge: a stale `direct 30ms` beside a dead session is worse
                        // than showing nothing.
                        link_summary = crate::transport::link_summary(&paths);
                        dirty = true;
                    }
                    NodeMessage::Rosters { rosters } => {
                        if let Some(view) = tui.as_mut() {
                            announce_agent_completions(apply_rosters(view, rosters));
                            dirty = true;
                        }
                    }
                    NodeMessage::Focus { tab_id, pane_id } => {
                        if let Some(view) = tui.as_mut() {
                            apply_focus(view, &mut pending_focus, tab_id, pane_id)?;
                            dirty = true;
                        }
                    }
                    NodeMessage::Presence { presence } => {
                        if let Some(view) = tui.as_mut() {
                            dirty |= view.set_presence(presence);
                        }
                    }
                    _ => {}
                },
                Ok(WakeEvent::Ipc(ReaderEvent::Ended)) | Err(TryRecvError::Disconnected) => {
                    node_ended = true;
                    break 'attached;
                }
                Ok(WakeEvent::Ipc(ReaderEvent::DecodeError(error))) => {
                    attach_error = Some(error);
                    break 'attached;
                }
                Ok(WakeEvent::Ipc(ReaderEvent::ReadError(error))) => {
                    attach_error = Some(error);
                    node_ended = true;
                    break 'attached;
                }
                Ok(WakeEvent::Terminal(event)) => {
                    pending_wake = Some(WakeEvent::Terminal(event));
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(tui) = tui.as_mut() {
            dirty |= refresh_tui_timers(tui, Instant::now(), &mut last_agent_overlay_animation);
            // Whenever it turns up, or never. The check runs on its own thread
            // from the moment this client started, so nothing here waited for
            // it and a machine with no network simply never fills this in.
            if let Ok(notice) = update_notices.try_recv() {
                dirty |= tui.set_update_notice(notice.line());
            }
            // A pointer held past a pane's edge sends no further drag events —
            // a terminal reports a drag when the cell under the pointer
            // changes, and it is not changing. This clock is what keeps the
            // rows coming while it sits there.
            if let Some((pane_id, up)) = tui.selection_autoscroll_due(Instant::now()) {
                scroll_pane_toward(
                    &mut stream,
                    tui,
                    &mut history,
                    &mut desired_scroll,
                    &mut pending_scroll,
                    &mut next_scrollback_request_id,
                    pane_id,
                    up,
                    SELECTION_AUTOSCROLL_STEP,
                )?;
            }
            // Every pass, not only the ones that stepped: a step whose rows had
            // to be fetched moves the viewport when the reply lands, which is
            // some later pass than the one that asked.
            if tui.selection_autoscroll_pane().is_some() {
                dirty |= tui.follow_selection_autoscroll(terminal.size()?.into());
            }
        }
        if dirty {
            if let Some(tui) = tui.as_ref() {
                let draw_started = Instant::now();
                terminal.draw(|frame| {
                    let viewport_screens = screens
                        .iter()
                        .filter(|(pane_id, _)| tui.pane_scrollback_offset(**pane_id) != 0)
                        .filter_map(|(pane_id, _)| {
                            history
                                .get(pane_id)
                                .and_then(|history| {
                                    history.viewports.get(&tui.pane_scrollback_offset(*pane_id))
                                })
                                .and_then(GuestScreen::screen)
                                .cloned()
                                .map(|viewport| (*pane_id, viewport))
                        })
                        .collect::<BTreeMap<_, _>>();
                    let visible = screens
                        .iter()
                        .filter_map(|(pane_id, guest)| {
                            viewport_screens
                                .get(pane_id)
                                .or_else(|| guest.screen())
                                .map(|screen| (*pane_id, screen))
                        })
                        .collect();
                    render_multi_pane_with_copy_feedback(
                        frame,
                        tui,
                        &visible,
                        copied_lines,
                        footer_notice.as_deref(),
                        ShareView {
                            ticket: share_ticket.as_deref(),
                            code: share_code.as_deref(),
                            notice: share_notice.as_deref(),
                        },
                        Some(&local_peer_id),
                        link_summary.as_deref(),
                    );
                })?;
                let draw_elapsed = draw_started.elapsed();
                if crate::perf::enabled() && draw_elapsed >= Duration::from_millis(5) {
                    crate::perf::log(&format!(
                        "P2PMUX_PERF client draw_ms={} panes={}",
                        draw_elapsed.as_millis(),
                        screens.len(),
                    ));
                }
                if let Some(perf_id) = draw_perf_id.take() {
                    crate::perf::log(&format!("P2PMUX_PERF id={perf_id} client_draw"));
                }
            }
            dirty = false;
        }
        if pending_wake.is_none() && resize_recheck_due(last_size_check, Instant::now()) {
            last_size_check = Some(Instant::now());
            if let Some((cols, rows)) = stale_node_size(
                viewport,
                node_viewport,
                terminal::size(),
                tui.as_ref().is_some_and(MultiPaneTui::modal_open),
            ) {
                pending_wake = Some(WakeEvent::Terminal(Event::Resize(cols, rows)));
            }
        }
        let event = match pending_wake.take() {
            Some(WakeEvent::Terminal(event)) => event,
            Some(wake) => {
                pending_wake = Some(wake);
                continue;
            }
            None => match wakes.recv_timeout(Duration::from_millis(16)) {
                Ok(WakeEvent::Terminal(event)) => event,
                Ok(wake) => {
                    pending_wake = Some(wake);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        let Some(tui) = tui.as_mut() else {
            continue;
        };
        // Zoom lives entirely in this client -- it never becomes a layout
        // request -- but it decides how much screen the pane is drawn in, and
        // the node is what sizes the PTYs it hosts. Compared around the whole
        // event rather than hooked into `toggle_zoom`, because the zoom also
        // stands itself down: opening Home, moving focus, switching tab and
        // clicking another pane all clear it.
        let zoom_before = tui.zoomed_pane();
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                // A forwarded key can still have ended chord mode on its way past, and the
                // footer names the mode it is in. Redrawing on every keystroke would be a
                // frame per character; redrawing when the mode actually moved is one frame
                // per chord. Without it a key the chord does not claim — one that produces
                // nothing on screen for the client to redraw on, a space at an empty prompt
                // say — leaves PANE MODE on the footer after the mode has ended.
                let chord_before = tui.chord_mode();
                match tui.handle_key(key, terminal.size()?.into()) {
                    KeyHandling::Quit(QuitAction::Detach) => {
                        write_message(&mut stream, &ClientMessage::Detach { generation })?;
                        detach_sent = true;
                        break;
                    }
                    // The node stops, so its panes stop with it. The record is
                    // removed here rather than left for the finder to reap, so
                    // `p2pmux ls` does not list a session that is already gone
                    // — the same order `p2pmux kill` uses.
                    KeyHandling::Quit(QuitAction::Kill) => {
                        write_message(&mut stream, &ClientMessage::Shutdown { generation })?;
                        detach_sent = true;
                        killed = true;
                        break;
                    }
                    KeyHandling::Consumed(intents) => {
                        send_intents(&mut stream, tui, intents, &mut pending_focus)?;
                        if let Some(request) = tui.take_share_copy_request() {
                            share_notice = Some(share_copy_result(
                                request,
                                share_ticket.as_deref(),
                                share_code.as_deref(),
                            ));
                        }
                        if tui.take_pair_offer()
                            && let Some(ticket) = share_ticket.as_deref()
                            && let Err(error) = crate::pairing::offer(ticket)
                        {
                            share_notice = Some(format!("could not record the pairing: {error}"));
                        }
                        // The notice belongs to one visit to the modal, not to the session.
                        if !tui.share_open() && !tui.add_machine_open() {
                            share_notice = None;
                        }
                        dirty = true;
                    }
                    KeyHandling::Forward => {
                        let kitty_keyboard_active = screens
                            .get(&tui.focused_pane())
                            .is_some_and(GuestScreen::kitty_keyboard_active);
                        if input_allowed(tui, &local_peer_id, tui.focused_pane())
                            && let Some(bytes) =
                                client_key_bytes(key.code, key.modifiers, kitty_keyboard_active)
                        {
                            let perf_id = next_perf_id_if_enabled(&mut next_perf_id);
                            if let Some(perf_id) = perf_id {
                                crate::perf::log(&format!("P2PMUX_PERF id={perf_id} client_input"));
                            }
                            write_message(
                                &mut stream,
                                &pane_input(tui.focused_pane(), bytes, perf_id),
                            )?;
                            history.remove(&tui.focused_pane());
                            pending_scroll.remove(&tui.focused_pane());
                            desired_scroll.remove(&tui.focused_pane());
                            tui.set_pane_scrollback_offset(tui.focused_pane(), 0);
                        }
                        dirty |= tui.chord_mode() != chord_before;
                    }
                }
            }
            Event::Paste(text) => {
                if should_forward_paste(tui, &local_peer_id) {
                    let perf_id = next_perf_id_if_enabled(&mut next_perf_id);
                    if let Some(perf_id) = perf_id {
                        crate::perf::log(&format!("P2PMUX_PERF id={perf_id} client_input"));
                    }
                    write_message(
                        &mut stream,
                        &pane_input(tui.focused_pane(), text.into_bytes(), perf_id),
                    )?;
                    history.remove(&tui.focused_pane());
                    pending_scroll.remove(&tui.focused_pane());
                    desired_scroll.remove(&tui.focused_pane());
                    tui.set_pane_scrollback_offset(tui.focused_pane(), 0);
                }
                dirty = true;
            }
            Event::Mouse(mouse) => {
                let area = terminal.size()?.into();
                if tui.modal_open() {
                    // Blocking dialogs consume mouse input without affecting panes.
                } else if tui.home_open() {
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) {
                        tui.scroll_home(area, matches!(mouse.kind, MouseEventKind::ScrollUp));
                    } else if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        let handling = tui.handle_mouse(mouse, area, PaneMouseProtocol::default());
                        send_intents(&mut stream, tui, handling.intents, &mut pending_focus)?;
                    }
                } else if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    // A child that reports mouse scrolls its own buffer; local
                    // scrollback would otherwise hide the wheel from it. Which
                    // child that is comes from the pointer, not from focus:
                    // asking the focused pane sent a notch aimed at a
                    // full-screen app next door into local scrollback, which
                    // answered "unavailable" for a pane that scrolls perfectly
                    // well the moment you focus it.
                    let pane_id = tui.pane_at_or_focused_for_mouse(mouse.column, mouse.row, area);
                    let protocol = pane_mouse_protocol(tui, &screens, &local_peer_id, pane_id);
                    let forwarded = tui.wheel_bytes_for_pane(mouse, area, protocol, pane_id);
                    if let Some(bytes) = forwarded {
                        let perf_id = next_perf_id_if_enabled(&mut next_perf_id);
                        write_message(&mut stream, &pane_input(pane_id, bytes, perf_id))?;
                    } else {
                        scroll_pane_toward(
                            &mut stream,
                            tui,
                            &mut history,
                            &mut desired_scroll,
                            &mut pending_scroll,
                            &mut next_scrollback_request_id,
                            pane_id,
                            matches!(mouse.kind, MouseEventKind::ScrollUp),
                            PANE_SCROLL_WHEEL_STEP,
                        )?;
                    }
                } else {
                    // Read before the click, which is allowed to move focus: the
                    // report was encoded against the pane that had focus then.
                    let addressed = tui.focused_pane();
                    let handling = tui.handle_mouse(
                        mouse,
                        area,
                        focused_pane_mouse_protocol(tui, &screens, &local_peer_id),
                    );
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        copied_lines = None;
                        footer_notice = None;
                    }
                    if let Some(bytes) = handling.forward_bytes {
                        let perf_id = next_perf_id_if_enabled(&mut next_perf_id);
                        write_message(&mut stream, &pane_input(addressed, bytes, perf_id))?;
                    }
                    send_intents(&mut stream, tui, handling.intents, &mut pending_focus)?;
                    if handling.copy_selection_requested {
                        copied_lines = copy_attach_selection(tui, &screens, &history);
                    }
                }
                dirty = true;
            }
            Event::Resize(cols, rows) => {
                terminal.resize(Rect::new(0, 0, cols, rows))?;
                viewport = (cols, rows);
                if !tui.modal_open() {
                    tui.set_home_viewport_for(Rect::new(0, 0, cols, rows));
                    // The cached viewports were built against the old grid, so
                    // they go — and with them the only rows a scrolled-back pane
                    // had to show. What it falls back to is its live screen, so
                    // the offsets have to come back to the live edge too, or the
                    // pane reads as scrolled while showing the newest output.
                    history.clear();
                    pending_scroll.clear();
                    desired_scroll.clear();
                    tui.reset_all_scrollback();
                    write_message(&mut stream, &ClientMessage::Resize { cols, rows })?;
                    node_viewport = (cols, rows);
                }
                dirty = true;
            }
            _ => {}
        }
        if tui.zoomed_pane() != zoom_before {
            let (cols, rows) = node_viewport;
            write_message(
                &mut stream,
                &ClientMessage::Zoom {
                    pane_id: tui.zoomed_pane(),
                    cols,
                    rows,
                },
            )?;
            // The panes are about to be reflowed under it, so history taken
            // against the old grid describes rows that no longer exist.
            history.clear();
            pending_scroll.clear();
            desired_scroll.clear();
        }
    }
    if !node_ended && !detach_sent {
        let _ = write_message(&mut stream, &ClientMessage::Detach { generation });
    }
    let _ = stream.shutdown(Shutdown::Both);
    terminal_stop.store(true, Ordering::Release);
    let _ = terminal_thread.join();
    let _ = reader_thread.join();
    guard.leave()?;
    if let Some(error) = attach_error {
        eprintln!("p2pmux attach error: {error}");
    }
    if killed {
        let _ = crate::session_store::SessionStore::for_current_user()
            .and_then(|store| store.remove(&descriptor.id));
        println!("Killed {}", descriptor.name);
    } else if node_ended {
        match node_exit_reason(descriptor) {
            Some(reason) => println!("p2pmux node ended: {reason}"),
            None => println!("p2pmux node ended"),
        }
    } else {
        println!(
            "Detached. Resume: p2pmux --resume  |  Attach: p2pmux attach {}  |  Kill: p2pmux kill {}",
            descriptor.name, descriptor.name
        );
    }
    Ok(())
}

/// What the node said on its way out, if it left anything.
///
/// Two places, because a node ends in two ways. One returns an error, which it
/// writes beside its socket -- the same file `launch_background_node` reads
/// while it is still waiting for a startup that never came. The other is a
/// panic, which returns nothing and writes only to stderr; that is why the node
/// is given a log rather than `/dev/null`, and why the last line of it is worth
/// reading here.
///
/// A session that ends under someone working in it is the case this serves. All
/// they had was `p2pmux node ended`, which names the event and nothing about it.
fn node_exit_reason(descriptor: &SessionDescriptor) -> Option<String> {
    let reported = descriptor.socket_path.with_extension("error");
    if let Ok(message) = std::fs::read_to_string(&reported) {
        let message = message.trim().to_owned();
        if !message.is_empty() {
            let _ = std::fs::remove_file(&reported);
            return Some(message);
        }
    }
    let log = descriptor.socket_path.with_extension("log");
    let text = std::fs::read_to_string(&log).ok()?;
    let last = text.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(format!("{}\n  its log: {}", last.trim(), log.display()))
}

fn refresh_tui_timers(
    tui: &mut MultiPaneTui,
    now: Instant,
    last_agent_overlay_animation: &mut Instant,
) -> bool {
    let mut dirty = tui.expire_chord_mode(now) || tui.expire_home_toggle(now);
    if tui.home_has_working_rows()
        && now.duration_since(*last_agent_overlay_animation) >= AGENT_OVERLAY_ANIMATION_INTERVAL
    {
        *last_agent_overlay_animation = now;
        dirty = true;
    }
    dirty
}

/// Bytes addressed to the pane they were encoded for.
///
/// Every byte this client sends was shaped by one pane's state -- its keyboard
/// mode, its xterm mouse mode -- so it names that pane rather than letting the
/// node re-decide from its own focus, which lags this one by a round trip after
/// every pane the session creates.
fn pane_input(pane_id: u64, bytes: Vec<u8>, perf_id: Option<u64>) -> ClientMessage {
    ClientMessage::Input {
        bytes,
        pane_id: Some(pane_id),
        perf_id,
    }
}

/// The mouse reporting the focused pane's child has turned on, if any.
fn focused_pane_mouse_protocol(
    tui: &MultiPaneTui,
    screens: &BTreeMap<u64, GuestScreen>,
    local_peer_id: &[u8],
) -> PaneMouseProtocol {
    pane_mouse_protocol(tui, screens, local_peer_id, tui.focused_pane())
}

/// The mouse reporting one pane's child has turned on, if any.
fn pane_mouse_protocol(
    tui: &MultiPaneTui,
    screens: &BTreeMap<u64, GuestScreen>,
    local_peer_id: &[u8],
    pane_id: u64,
) -> PaneMouseProtocol {
    if !input_allowed(tui, local_peer_id, pane_id) {
        return PaneMouseProtocol::default();
    }
    screens
        .get(&pane_id)
        .and_then(GuestScreen::screen)
        .map(PaneMouseProtocol::from_screen)
        .unwrap_or_default()
}

fn input_allowed(tui: &MultiPaneTui, local_peer_id: &[u8], pane_id: u64) -> bool {
    tui.snapshot()
        .panes
        .get(&pane_id)
        .is_none_or(|pane| !pane.exited && (!pane.locked || pane.host_peer_id == local_peer_id))
}

fn next_perf_id_if_enabled(next: &mut u64) -> Option<u64> {
    if !crate::perf::enabled() {
        return None;
    }
    let id = *next;
    *next = next.wrapping_add(1).max(1);
    Some(id)
}

fn should_forward_paste(tui: &MultiPaneTui, local_peer_id: &[u8]) -> bool {
    !tui.home_open() && !tui.modal_open() && input_allowed(tui, local_peer_id, tui.focused_pane())
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn apply_snapshot(
    tui: &mut Option<MultiPaneTui>,
    theme: crate::config::UiTheme,
    screens: &mut BTreeMap<u64, GuestScreen>,
    history: &mut BTreeMap<u64, HistoryCache>,
    room_name: String,
    layout: crate::layout::LayoutSnapshot,
    next_screens: Vec<crate::local_ipc::PaneScreenSnapshot>,
    leases: Vec<crate::local_ipc::PaneLeaseSnapshot>,
    rosters: Vec<crate::local_ipc::AgentOverlaySnapshotRow>,
    tab_id: u64,
    pane_id: u64,
    pending_focus: &mut Option<(u64, u64)>,
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    let view = apply_layout(tui, theme, screens, history, room_name, layout)?;
    let resync = apply_screens(
        view,
        screens,
        history,
        next_screens,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
    )?;
    apply_leases(view, leases);
    apply_focus(view, pending_focus, tab_id, pane_id)?;
    let _ = apply_rosters(view, rosters);
    Ok(resync)
}

fn apply_layout<'a>(
    tui: &'a mut Option<MultiPaneTui>,
    theme: crate::config::UiTheme,
    screens: &mut BTreeMap<u64, GuestScreen>,
    history: &mut BTreeMap<u64, HistoryCache>,
    room_name: String,
    layout: crate::layout::LayoutSnapshot,
) -> Result<&'a mut MultiPaneTui, Box<dyn std::error::Error>> {
    let view = match tui {
        Some(view) => {
            view.apply_snapshot(layout)
                .map_err(|error| io::Error::other(format!("invalid layout snapshot: {error:?}")))?;
            view
        }
        None => tui
            .insert(MultiPaneTui::with_theme(layout, theme).map_err(|error| {
                io::Error::other(format!("invalid layout snapshot: {error:?}"))
            })?),
    };
    if !room_name.is_empty() {
        view.set_title(format!("p2pmux ({room_name})"));
    }
    screens.retain(|pane_id, _| view.snapshot().panes.contains_key(pane_id));
    history.retain(|pane_id, _| view.snapshot().panes.contains_key(pane_id));
    Ok(view)
}

fn apply_screens(
    view: &mut MultiPaneTui,
    screens: &mut BTreeMap<u64, GuestScreen>,
    history: &mut BTreeMap<u64, HistoryCache>,
    next_screens: Vec<crate::local_ipc::PaneScreenSnapshot>,
    pending_resync: &mut BTreeSet<u64>,
    _history_refresh: &mut BTreeSet<u64>,
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    let mut resync = Vec::new();
    for frame in next_screens {
        let pane_id = frame.pane_id;
        let history_len = frame.history_len;
        let screen = screens.entry(frame.pane_id).or_default();
        match frame.state {
            ScreenUpdate::Snapshot {
                sequence,
                snapshot,
                kitty_keyboard_active,
            } => {
                screen.apply_snapshot(sequence, &snapshot)?;
                screen.set_kitty_keyboard_active(kitty_keyboard_active);
                pending_resync.remove(&pane_id);
            }
            ScreenUpdate::Delta {
                base_sequence,
                sequence,
                delta,
                kitty_keyboard_active,
            } => match screen.apply_delta(base_sequence, sequence, &delta) {
                Ok(ApplyDelta::Applied) => screen.set_kitty_keyboard_active(kitty_keyboard_active),
                Ok(ApplyDelta::NeedsSnapshot) | Err(_) => resync.push(frame.pane_id),
            },
            ScreenUpdate::Unchanged {
                sequence: _,
                kitty_keyboard_active,
            } => screen.set_kitty_keyboard_active(kitty_keyboard_active),
        }
        if let Some(screen) = screen.screen() {
            let cache = history.entry(pane_id).or_default();
            if cache.grid.is_some_and(|grid| grid != screen.size()) || history_len == 0 {
                *cache = HistoryCache::default();
                view.set_pane_scrollback_offset(pane_id, 0);
            }
        }
    }
    Ok(resync)
}

fn apply_leases(view: &mut MultiPaneTui, leases: Vec<crate::local_ipc::PaneLeaseSnapshot>) {
    for lease in leases {
        view.set_pane_view(
            lease.pane_id,
            PaneViewState::from_chrome(
                lease.ready,
                lease.controller_peer_id,
                lease.controller_active,
            ),
        );
    }
}

/// Record that a pane's agent finished.
///
/// This used to also play a sound. It no longer does — a completion that a hook
/// reported is already unmissable in the overlay and the pane's unread mark, and
/// the chime fired on the inference path that could not tell a finished turn from
/// a quiet one. The log line stays: a completion that arrives at the wrong moment
/// is otherwise invisible after the fact.
fn announce_agent_completions(panes: Vec<u64>) {
    for pane_id in panes {
        crate::tui::ui_debug_log("agent_completion", format_args!("pane={pane_id}"));
    }
}

fn apply_rosters(
    view: &mut MultiPaneTui,
    rosters: Vec<crate::local_ipc::AgentOverlaySnapshotRow>,
) -> Vec<u64> {
    view.update_attached_agent_rows(
        rosters
            .into_iter()
            .map(|row| AgentOverlayRow {
                pane_id: row.pane_id,
                process_pid: row.process_pid,
                tab_ordinal: 0,
                pane_ordinal: 0,
                tab_label: String::new(),
                pane_label: String::new(),
                kind: row.kind,
                cwd: row.cwd,
                state: AgentRosterState::from_wire(row.state),
                working_since_unix_ms: row.working_since_unix_ms,
                host: row.host,
                controller: row.controller,
                message: row.message,
                session: row.session,
            })
            .collect(),
    )
}

fn apply_focus(
    view: &mut MultiPaneTui,
    pending_focus: &mut Option<(u64, u64)>,
    tab_id: u64,
    pane_id: u64,
) -> Result<(), io::Error> {
    if let Some((pending_tab_id, pending_pane_id)) = *pending_focus {
        if (tab_id, pane_id) == (pending_tab_id, pending_pane_id) {
            *pending_focus = None;
        } else if view.set_focus(pending_tab_id, pending_pane_id).is_ok() {
            return Ok(());
        } else {
            *pending_focus = None;
        }
    }
    view.set_focus(tab_id, pane_id)
        .map_err(|error| io::Error::other(format!("invalid node focus: {error:?}")))
}

fn new_resync_requests(pending: &mut BTreeSet<u64>, resync: Vec<u64>) -> Vec<u64> {
    resync
        .into_iter()
        .filter(|pane_id| pending.insert(*pane_id))
        .collect()
}

fn send_intents(
    stream: &mut UnixStream,
    tui: &MultiPaneTui,
    intents: Vec<crate::tui::UiIntent>,
    pending_focus: &mut Option<(u64, u64)>,
) -> io::Result<()> {
    for intent in intents {
        match intent {
            crate::tui::UiIntent::FocusPane { .. } | crate::tui::UiIntent::SwitchTab { .. } => {
                let focus = (tui.current_tab(), tui.focused_pane());
                write_message(
                    stream,
                    &ClientMessage::Focus {
                        tab_id: focus.0,
                        pane_id: focus.1,
                    },
                )?;
                *pending_focus = Some(focus);
            }
            intent => write_message(stream, &ClientMessage::StructuralIntent { intent })?,
        }
    }
    Ok(())
}

fn request_scrollback(
    stream: &mut UnixStream,
    pane_id: u64,
    target: usize,
    history: &BTreeMap<u64, HistoryCache>,
    pending: &mut BTreeMap<u64, PendingScroll>,
    next_request_id: &mut u64,
) -> io::Result<()> {
    let request_id = *next_request_id;
    *next_request_id = (*next_request_id).wrapping_add(1).max(1);
    let history_id = history.get(&pane_id).and_then(|history| history.history_id);
    pending.insert(pane_id, PendingScroll { request_id, target });
    write_message(
        stream,
        &ClientMessage::ScrollbackQuery {
            pane_id,
            history_id,
            offset: target as u64,
            request_id,
        },
    )?;
    Ok(())
}

pub fn shutdown(descriptor: &SessionDescriptor) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(&descriptor.socket_path)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    // Shutdown is a control request, not an interactive attachment.  It must still work while a
    // stale or live client holds the single-attachment gate.
    write_message(&mut stream, &ClientMessage::Shutdown { generation: 0 })?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    loop {
        match read_message(&mut reader)? {
            Some(NodeMessage::ShutdownAck { generation: 0 }) => break,
            Some(_) => continue,
            None => {
                return Err(io::Error::other("node closed before shutdown acknowledgement").into());
            }
        }
    }
    Ok(())
}

fn write_message(stream: &mut UnixStream, message: &ClientMessage) -> io::Result<()> {
    let mut frame = serde_json::to_vec(message).map_err(io::Error::other)?;
    frame.push(b'\n');
    stream.write_all(&frame)?;
    stream.flush()
}

/// Reads one complete newline-delimited local IPC frame.  This deliberately stays blocking: a
/// Snapshot can be much larger than a single Unix-socket read.
pub fn read_message(reader: &mut BufReader<UnixStream>) -> io::Result<Option<NodeMessage>> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => serde_json::from_str(&line).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid node message: {error}"),
            )
        }),
        Err(error) => Err(error),
    }
}

enum ReaderEvent {
    Message(Box<NodeMessage>),
    DecodeError(String),
    ReadError(String),
    Ended,
}

enum WakeEvent {
    Ipc(ReaderEvent),
    Terminal(Event),
}

fn spawn_message_reader(
    mut reader: BufReader<UnixStream>,
    sender: mpsc::Sender<WakeEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        loop {
            match read_message(&mut reader) {
                Ok(Some(message)) => {
                    if sender
                        .send(WakeEvent::Ipc(ReaderEvent::Message(Box::new(message))))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(WakeEvent::Ipc(ReaderEvent::Ended));
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    if sender
                        .send(WakeEvent::Ipc(ReaderEvent::DecodeError(error.to_string())))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(WakeEvent::Ipc(ReaderEvent::ReadError(error.to_string())));
                    return;
                }
            }
        }
    })
}

fn spawn_terminal_reader(
    sender: mpsc::Sender<WakeEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if sender.send(WakeEvent::Terminal(event)).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                },
                Ok(false) => {}
                Err(_) => return,
            }
        }
    })
}

fn client_key_bytes(
    code: KeyCode,
    modifiers: KeyModifiers,
    kitty_keyboard_active: bool,
) -> Option<Vec<u8>> {
    match code {
        KeyCode::Char(character)
            if modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii_alphabetic() =>
        {
            Some(vec![character.to_ascii_lowercase() as u8 - b'a' + 1])
        }
        KeyCode::Char(character) => Some(character.to_string().into_bytes()),
        KeyCode::Enter if modifiers == KeyModifiers::SHIFT => {
            if kitty_keyboard_active {
                Some(b"\x1b[13;2u".to_vec())
            } else {
                Some(b"\n".to_vec())
            }
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        // Shift+Tab, which crossterm reports as its own code rather than as a
        // modified Tab. Matched whatever the modifiers say: terminals disagree
        // about whether to set SHIFT on a key whose whole identity is the
        // shift. Without this arm the key encoded to nothing and never left the
        // client, so the mode switch it drives in a child never happened.
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        _ => None,
    }
}

struct ClientTerminalGuard {
    stdout: io::Stdout,
    raw: bool,
    keyboard_enhancement: bool,
    alternate: bool,
    paste: bool,
    mouse: bool,
}
impl ClientTerminalGuard {
    fn enter(name: &str) -> io::Result<Self> {
        let mut guard = Self {
            stdout: io::stdout(),
            raw: false,
            keyboard_enhancement: false,
            alternate: false,
            paste: false,
            mouse: false,
        };
        terminal::enable_raw_mode()?;
        guard.raw = true;
        execute!(guard.stdout, SetTitle(format!("p2pmux ({name})")))?;
        execute!(guard.stdout, EnterAlternateScreen)?;
        guard.alternate = true;
        execute!(guard.stdout, EnableBracketedPaste)?;
        guard.paste = true;
        execute!(guard.stdout, EnableMouseCapture)?;
        guard.mouse = true;
        execute!(
            guard.stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )?;
        guard.keyboard_enhancement = true;
        Ok(guard)
    }
    fn leave(&mut self) -> io::Result<()> {
        if self.mouse {
            execute!(self.stdout, DisableMouseCapture)?;
            self.mouse = false;
        }
        if self.paste {
            execute!(self.stdout, DisableBracketedPaste)?;
            self.paste = false;
        }
        if self.alternate {
            execute!(self.stdout, LeaveAlternateScreen)?;
            self.alternate = false;
        }
        if self.keyboard_enhancement {
            execute!(self.stdout, PopKeyboardEnhancementFlags)?;
            self.stdout.flush()?;
            self.keyboard_enhancement = false;
        }
        if self.raw {
            terminal::disable_raw_mode()?;
            self.raw = false;
        }
        Ok(())
    }
}
impl Drop for ClientTerminalGuard {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        time::{Duration, Instant},
    };

    use crossterm::event::KeyEvent;

    use crate::{
        layout::{LayoutSnapshot, Member, Node, Pane, Tab},
        local_ipc::{AgentOverlaySnapshotRow, PaneScreenSnapshot},
        screen::HostScreen,
        tui::{HOME_TOGGLE_WINDOW, KeyHandling, UiIntent},
    };

    use super::*;

    /// The one refusal a caller recovers from, told apart from every other.
    ///
    /// Bare `p2pmux` passes over a session that already has a terminal in it and
    /// opens another; it must not do that for a session that is genuinely
    /// broken, because then the fault disappears behind a working session and
    /// nobody ever sees it.
    #[test]
    fn only_a_busy_session_reads_as_already_attached() {
        let busy: Box<dyn std::error::Error> =
            AttachRejected(crate::local_ipc::ALREADY_ATTACHED.into()).into();
        assert!(is_already_attached(&*busy));

        // A rejection with any other reason is the node saying something else.
        let other: Box<dyn std::error::Error> =
            AttachRejected("session is shutting down".into()).into();
        assert!(!is_already_attached(&*other));

        // And a plain I/O failure — a socket that has gone away, say — is not a
        // rejection at all.
        let broken: Box<dyn std::error::Error> = io::Error::other("connection reset").into();
        assert!(!is_already_attached(&*broken));
    }

    fn layout(pane_ids: &[u64]) -> LayoutSnapshot {
        let root = pane_ids
            .iter()
            .copied()
            .map(|pane_id| Node::Leaf { pane_id })
            .reduce(|first, second| Node::Split {
                axis: crate::layout::Axis::LeftRight,
                first_share_bps: 5_000,
                first: Box::new(first),
                second: Box::new(second),
            })
            .expect("at least one pane");
        LayoutSnapshot {
            revision: 1,
            members: vec![Member {
                peer_id: b"host".to_vec(),
                endpoint_addr: b"endpoint".to_vec(),
                display_name: String::from("Host"),
                kind: Default::default(),
                machine_proof: Default::default(),
                machine_id: Default::default(),
            }],
            tabs: vec![Tab {
                tab_id: 1,
                root,
                title: None,
            }],
            panes: pane_ids
                .iter()
                .map(|pane_id| {
                    (
                        *pane_id,
                        Pane {
                            pane_id: *pane_id,
                            host_peer_id: b"host".to_vec(),
                            locked: false,
                            exited: false,
                            grid_rows: 2,
                            grid_cols: 8,
                            title: None,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn roster_row(pane_id: u64, state: i32) -> AgentOverlaySnapshotRow {
        AgentOverlaySnapshotRow {
            message: String::new(),
            pane_id,
            process_pid: 0,
            kind: String::from("codex"),
            cwd: String::from("/repo"),
            state,
            working_since_unix_ms: 1,
            host: String::from("Host"),
            controller: String::from("free"),
            session: String::new(),
        }
    }

    fn apply_rows(
        tui: &mut Option<MultiPaneTui>,
        pane_ids: &[u64],
        rows: Vec<AgentOverlaySnapshotRow>,
    ) {
        let mut pending_focus = None;
        apply_snapshot(
            tui,
            crate::config::UiTheme::default(),
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
            String::from("room"),
            layout(pane_ids),
            vec![],
            vec![],
            rows,
            1,
            pane_ids[0],
            &mut pending_focus,
        )
        .unwrap();
    }

    fn apply_focus_snapshot(
        tui: &mut Option<MultiPaneTui>,
        layout: LayoutSnapshot,
        tab_id: u64,
        pane_id: u64,
        pending_focus: &mut Option<(u64, u64)>,
    ) {
        apply_snapshot(
            tui,
            crate::config::UiTheme::default(),
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
            String::from("room"),
            layout,
            vec![],
            vec![],
            vec![],
            tab_id,
            pane_id,
            pending_focus,
        )
        .unwrap();
    }
    #[test]
    fn ctrl_q_is_reserved_for_detach() {
        assert_eq!(
            client_key_bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, false),
            Some(vec![3])
        );
        assert_eq!(
            client_key_bytes(KeyCode::Char('q'), KeyModifiers::CONTROL, false),
            Some(vec![17])
        );
    }

    /// Claude Code cycles its mode on Shift+Tab, so a pane that never receives
    /// the key is a pane whose mode cannot be changed at all.
    #[test]
    fn shift_tab_reaches_the_pane_as_a_back_tab() {
        assert_eq!(
            client_key_bytes(KeyCode::BackTab, KeyModifiers::SHIFT, false),
            Some(b"\x1b[Z".to_vec())
        );
        // Some terminals report the key without naming the modifier.
        assert_eq!(
            client_key_bytes(KeyCode::BackTab, KeyModifiers::NONE, false),
            Some(b"\x1b[Z".to_vec())
        );
        // And the mux does not claim it on the way past.
        let mut tui = MultiPaneTui::new(layout(&[1])).expect("layout");
        assert!(matches!(
            tui.handle_key(
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                Rect::new(0, 0, 80, 24)
            ),
            KeyHandling::Forward
        ));
    }

    #[test]
    fn encodes_shift_enter_for_the_focused_pane_keyboard_mode() {
        assert_eq!(
            client_key_bytes(KeyCode::Enter, KeyModifiers::NONE, false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            client_key_bytes(KeyCode::Enter, KeyModifiers::SHIFT, false),
            Some(b"\n".to_vec())
        );
        assert_eq!(
            client_key_bytes(KeyCode::Enter, KeyModifiers::SHIFT, true),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            client_key_bytes(KeyCode::Char('j'), KeyModifiers::CONTROL, false),
            Some(b"\n".to_vec())
        );
        assert_eq!(
            client_key_bytes(KeyCode::Enter, KeyModifiers::ALT, true),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            client_key_bytes(KeyCode::Enter, KeyModifiers::CONTROL, true),
            Some(b"\r".to_vec())
        );
    }

    /// A wheel burst leaves older replies in flight behind the newest request. Those
    /// stale replies have to pass through without disturbing the request still waiting,
    /// or the pane stops moving for as long as the user keeps scrolling.
    #[test]
    fn a_stale_scroll_reply_leaves_the_live_request_waiting() {
        let mut pending = BTreeMap::new();
        pending.insert(
            1,
            PendingScroll {
                request_id: 4,
                target: 12,
            },
        );

        assert!(take_matching_pending(&mut pending, 1, 1).is_none());
        assert!(take_matching_pending(&mut pending, 1, 3).is_none());
        assert!(take_matching_pending(&mut pending, 2, 4).is_none());
        assert_eq!(
            pending.get(&1).map(|pending| pending.request_id),
            Some(4),
            "stale and cross-pane replies must not consume the live request"
        );

        let claimed = take_matching_pending(&mut pending, 1, 4).expect("live reply claims");
        assert_eq!(claimed.target, 12);
        assert!(pending.is_empty());
        // The answer is claimed once; a duplicate must not re-apply it.
        assert!(take_matching_pending(&mut pending, 1, 4).is_none());
    }

    /// A burst has to accumulate. Stepping from the visible offset made every notch ask
    /// for the same row, because the visible offset cannot move until a reply lands.
    #[test]
    fn a_wheel_burst_accumulates_instead_of_asking_for_the_same_row() {
        // Nothing outstanding: step from what is on screen.
        assert_eq!(
            next_scroll_target(0, None, 1_000, true, PANE_SCROLL_WHEEL_STEP),
            3
        );
        assert_eq!(
            next_scroll_target(9, None, 1_000, false, PANE_SCROLL_WHEEL_STEP),
            6
        );

        // Mid-burst the visible offset lags, so each notch steps from the request.
        let mut requested = None;
        for expected in [3, 6, 9, 12] {
            let target = next_scroll_target(0, requested, 1_000, true, PANE_SCROLL_WHEEL_STEP);
            assert_eq!(target, expected);
            requested = Some(target);
        }

        // Reversing mid-burst walks the same accumulated position back down.
        assert_eq!(
            next_scroll_target(0, Some(12), 1_000, false, PANE_SCROLL_WHEEL_STEP),
            9
        );

        // Bounds: never past the retained history, never below the live edge.
        assert_eq!(
            next_scroll_target(0, Some(9), 10, true, PANE_SCROLL_WHEEL_STEP),
            10
        );
        assert_eq!(
            next_scroll_target(0, Some(10), 10, true, PANE_SCROLL_WHEEL_STEP),
            10
        );
        assert_eq!(
            next_scroll_target(0, Some(2), 10, false, PANE_SCROLL_WHEEL_STEP),
            0
        );
        assert_eq!(
            next_scroll_target(0, Some(0), 10, false, PANE_SCROLL_WHEEL_STEP),
            0
        );
    }

    #[test]
    fn history_cache_keeps_a_render_ready_scrollback_viewport() {
        let mut host = HostScreen::new(1, 3).unwrap();
        host.process_pty(b"one\r\ntwo").unwrap();
        let mut host_view = host.screen().clone();
        host_view.set_scrollback(1);
        let mut history = HistoryCache::default();
        let payload = crate::screen::snapshot_payload(&host_view).unwrap();
        history.install_viewport(7, 1, 1, payload.as_ref()).unwrap();
        assert!(
            history.viewports[&1]
                .screen()
                .unwrap()
                .contents()
                .contains("one")
        );
    }

    /// The node says "unavailable" for a pane it does not host, for one on the
    /// alternate screen, and for a frozen session it has since evicted. Only the
    /// last of those can reach a pane that is already parked in history -- and
    /// when it does, everything that pane had to show is gone, so it is back at
    /// its live edge whether or not its offset admits it.
    #[test]
    fn losing_a_panes_history_returns_it_to_the_live_edge() {
        let host = HostScreen::new(2, 8).unwrap();
        let mut tui = None;
        let mut screens = BTreeMap::new();
        let mut history = BTreeMap::new();
        let mut pending_focus = None;
        apply_snapshot(
            &mut tui,
            crate::config::UiTheme::default(),
            &mut screens,
            &mut history,
            String::from("room"),
            layout(&[1]),
            vec![PaneScreenSnapshot {
                pane_id: 1,
                state: ScreenUpdate::Snapshot {
                    sequence: 1,
                    snapshot: host.current_frame().snapshot.as_ref().to_vec(),
                    kitty_keyboard_active: false,
                },
                history_len: 40,
                history_end: 40,
            }],
            vec![],
            vec![],
            1,
            1,
            &mut pending_focus,
        )
        .unwrap();
        let view = tui.as_mut().expect("a snapshot builds the view");
        assert!(view.set_pane_scrollback_offset(1, 12));
        let mut desired_scroll = BTreeMap::from([(1_u64, 30_usize)]);

        abandon_history(tui.as_mut(), &mut history, &mut desired_scroll, 1);

        assert_eq!(
            tui.as_ref().expect("view").pane_scrollback_offset(1),
            0,
            "the pane is showing live output, so it has to say so"
        );
        assert!(!history.contains_key(&1), "the dead session goes with it");
        assert!(
            !desired_scroll.contains_key(&1),
            "and so does whatever the burst had queued behind it"
        );
    }

    #[test]
    fn screen_updates_do_not_fill_the_history_cache() {
        let host = HostScreen::new(2, 8).unwrap();
        let mut tui = None;
        let mut screens = BTreeMap::new();
        let mut history = BTreeMap::new();
        let mut pending_focus = None;
        apply_snapshot(
            &mut tui,
            crate::config::UiTheme::default(),
            &mut screens,
            &mut history,
            String::from("room"),
            layout(&[1]),
            vec![PaneScreenSnapshot {
                pane_id: 1,
                state: ScreenUpdate::Snapshot {
                    sequence: 1,
                    snapshot: host.current_frame().snapshot.as_ref().to_vec(),
                    kitty_keyboard_active: false,
                },
                history_len: 1,
                history_end: 1,
            }],
            vec![],
            vec![],
            1,
            1,
            &mut pending_focus,
        )
        .unwrap();
        assert_eq!(history[&1].available_rows(), 0);

        apply_snapshot(
            &mut tui,
            crate::config::UiTheme::default(),
            &mut screens,
            &mut history,
            String::from("room"),
            layout(&[1]),
            vec![PaneScreenSnapshot {
                pane_id: 1,
                state: ScreenUpdate::Unchanged {
                    sequence: 1,
                    kitty_keyboard_active: false,
                },
                history_len: 1,
                history_end: 1,
            }],
            vec![],
            vec![],
            1,
            1,
            &mut pending_focus,
        )
        .unwrap();
        assert_eq!(history[&1].available_rows(), 0);
    }

    #[test]
    fn screens_apply_without_replacing_layout() {
        let host = HostScreen::new(2, 8).unwrap();
        let mut tui = Some(MultiPaneTui::new(layout(&[1])).unwrap());
        let mut screens = BTreeMap::new();
        let mut history = BTreeMap::new();
        let mut pending_resync = BTreeSet::new();

        apply_screens(
            tui.as_mut().unwrap(),
            &mut screens,
            &mut history,
            vec![PaneScreenSnapshot {
                pane_id: 1,
                state: ScreenUpdate::Snapshot {
                    sequence: 1,
                    snapshot: host.current_frame().snapshot.as_ref().to_vec(),
                    kitty_keyboard_active: false,
                },
                history_len: 0,
                history_end: 0,
            }],
            &mut pending_resync,
            &mut BTreeSet::new(),
        )
        .unwrap();

        assert_eq!(tui.unwrap().snapshot().revision, 1);
        assert_eq!(
            screens[&1].screen().unwrap().contents(),
            host.screen().contents()
        );
    }

    #[test]
    fn resync_requests_are_deduplicated_until_a_snapshot_arrives() {
        let mut pending = BTreeSet::new();
        assert_eq!(new_resync_requests(&mut pending, vec![1, 1]), vec![1]);
        assert!(new_resync_requests(&mut pending, vec![1]).is_empty());
        pending.remove(&1);
        assert_eq!(new_resync_requests(&mut pending, vec![1]), vec![1]);
    }

    #[test]
    fn shift_enter_encodes_as_lf_when_kitty_keyboard_is_inactive() {
        assert_eq!(
            client_key_bytes(KeyCode::Enter, KeyModifiers::SHIFT, false),
            Some(b"\n".to_vec())
        );
    }

    #[test]
    fn stale_snapshot_preserves_pending_focus_after_applying_layout() {
        let mut tui = None;
        let mut pending_focus = None;
        apply_focus_snapshot(&mut tui, layout(&[1, 2]), 1, 1, &mut pending_focus);
        pending_focus = Some((1, 2));
        let mut updated_layout = layout(&[1, 2]);
        updated_layout.revision = 2;

        apply_focus_snapshot(&mut tui, updated_layout, 1, 1, &mut pending_focus);

        let tui = tui.unwrap();
        assert_eq!(tui.snapshot().revision, 2);
        assert_eq!((tui.current_tab(), tui.focused_pane()), (1, 2));
        assert_eq!(pending_focus, Some((1, 2)));
    }

    #[test]
    fn matching_snapshot_clears_pending_focus() {
        let mut tui = None;
        let mut pending_focus = Some((1, 2));

        apply_focus_snapshot(&mut tui, layout(&[1, 2]), 1, 2, &mut pending_focus);

        let tui = tui.unwrap();
        assert_eq!((tui.current_tab(), tui.focused_pane()), (1, 2));
        assert_eq!(pending_focus, None);
    }

    #[test]
    fn an_echoed_focus_lets_the_pane_created_next_take_focus() {
        let mut tui = None;
        // The click landed on the pane the node had already focused, so the echo
        // that releases this hold is one the node only sends because it answers
        // every focus request rather than only the ones that moved it.
        let mut pending_focus = Some((1, 2));
        apply_focus_snapshot(&mut tui, layout(&[1, 2]), 1, 2, &mut pending_focus);
        assert_eq!(pending_focus, None);

        let mut created = layout(&[1, 2, 3]);
        created.revision = 2;
        apply_focus_snapshot(&mut tui, created, 1, 3, &mut pending_focus);

        let tui = tui.unwrap();
        assert_eq!((tui.current_tab(), tui.focused_pane()), (1, 3));
    }

    #[test]
    fn snapshot_focus_replaces_pending_focus_when_layout_removes_it() {
        let mut tui = None;
        let mut pending_focus = Some((1, 2));

        apply_focus_snapshot(&mut tui, layout(&[1]), 1, 1, &mut pending_focus);

        let tui = tui.unwrap();
        assert_eq!((tui.current_tab(), tui.focused_pane()), (1, 1));
        assert_eq!(pending_focus, None);
    }

    #[test]
    fn attach_client_timers_leave_home_open_and_a_doubled_ctrl_a_forwards() {
        let mut tui = Some(MultiPaneTui::new(layout(&[1])).unwrap());
        let tui = tui.as_mut().unwrap();
        let area = Rect::new(0, 0, 80, 24);
        let ctrl_a = crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut animation = Instant::now();

        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
        assert!(tui.home_open());
        // A timer tick a second later expires the doubled-press window and
        // nothing else: Home is a screen, not a transient popup.
        assert!(!refresh_tui_timers(
            tui,
            Instant::now() + Duration::from_secs(1),
            &mut animation,
        ));
        assert!(tui.home_open());
        assert_eq!(
            tui.handle_key(
                crossterm::event::KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Forward);
    }

    #[test]
    fn snapshot_rosters_populate_rows_and_ignore_invalid_or_unknown_panes() {
        let mut tui = None;
        apply_rows(
            &mut tui,
            &[1],
            vec![
                roster_row(1, AgentRosterState::Working as i32),
                roster_row(2, AgentRosterState::Working as i32),
                roster_row(1, 99),
            ],
        );
        let area = Rect::new(0, 0, 80, 24);
        {
            let tui = tui.as_mut().unwrap();
            tui.handle_key(
                crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                area,
            );
            let mut animation = Instant::now();
            refresh_tui_timers(tui, Instant::now() + HOME_TOGGLE_WINDOW, &mut animation);
            assert_eq!(
                tui.handle_key(
                    crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    area
                ),
                KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 1 }]),
            );
        }
        apply_rows(&mut tui, &[1], vec![]);
        let tui = tui.as_mut().unwrap();
        tui.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            area,
        );
        let mut animation = Instant::now();
        refresh_tui_timers(tui, Instant::now() + HOME_TOGGLE_WINDOW, &mut animation);
        assert_eq!(
            tui.handle_key(
                crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                area
            ),
            KeyHandling::Consumed(vec![]),
        );
    }

    #[test]
    fn home_blocks_paste_and_a_resize_before_it_opens_still_sets_scroll_bounds() {
        let mut tui = None;
        apply_rows(
            &mut tui,
            &[1, 2, 3],
            vec![
                roster_row(1, AgentRosterState::Working as i32),
                roster_row(2, AgentRosterState::Working as i32),
                roster_row(3, AgentRosterState::Working as i32),
            ],
        );
        let tui = tui.as_mut().unwrap();
        // Short enough that three rows do not fit, so there is something to
        // scroll: two lines of header leave two for a list of three.
        let area = Rect::new(0, 0, 24, 6);
        tui.set_home_viewport_for(area);
        tui.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            area,
        );
        let mut animation = Instant::now();
        refresh_tui_timers(tui, Instant::now() + HOME_TOGGLE_WINDOW, &mut animation);

        assert!(!should_forward_paste(tui, b"host"));
        assert!(tui.scroll_home(area, false));
    }

    #[test]
    fn delete_confirmation_blocks_paste_and_scrolling() {
        let mut tui = MultiPaneTui::new(layout(&[1, 2])).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            area,
        );

        assert!(tui.modal_open());
        assert!(!should_forward_paste(&tui, b"host"));
        assert!(!tui.scroll_mouse_pane(10, 2, area, 10, true));
    }

    /// What somebody dropped back to their shell mid-session gets to read.
    #[test]
    fn a_node_that_left_a_reason_is_quoted_rather_than_summarised() {
        let root = std::path::PathBuf::from(format!("/tmp/p2pmux-why-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture directory");
        let socket = root.join("n.sock");
        let descriptor = crate::session_store::SessionDescriptor::new(
            crate::session_store::generate_id().expect("id"),
            "lisbon".into(),
            socket.clone(),
            42,
            crate::session_store::SessionRole::Coordinator,
        );

        // A node that ended on its own says nothing it was not asked to say.
        assert_eq!(node_exit_reason(&descriptor), None);

        // An error on the way out is reported verbatim, and consumed: it
        // describes one death, and reading it twice would misattribute it.
        std::fs::write(root.join("n.error"), "the coordinator refused this build\n")
            .expect("error file");
        assert_eq!(
            node_exit_reason(&descriptor).as_deref(),
            Some("the coordinator refused this build")
        );
        assert_eq!(node_exit_reason(&descriptor), None);

        // A panic returns no error at all, and is only ever in the log.
        std::fs::write(
            root.join("n.log"),
            "p2pmux node: slow drain\nthread 'main' panicked at src/node.rs:1: assertion failed\n",
        )
        .expect("log file");
        let reason = node_exit_reason(&descriptor).expect("the log's last word");
        assert!(reason.starts_with("thread 'main' panicked"), "{reason}");
        assert!(
            reason.contains("n.log"),
            "the log is worth naming: {reason}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn locked_guest_pane_suppresses_key_and_paste_forwarding() {
        let mut layout = layout(&[1]);
        layout.panes.get_mut(&1).expect("pane").locked = true;
        let tui = MultiPaneTui::new(layout).expect("layout");

        assert!(!input_allowed(&tui, b"guest", tui.focused_pane()));
        assert!(!should_forward_paste(&tui, b"guest"));
        assert!(input_allowed(&tui, b"host", tui.focused_pane()));
        assert!(should_forward_paste(&tui, b"host"));
    }

    #[test]
    fn exited_pane_suppresses_key_and_paste_for_every_peer() {
        let mut layout = layout(&[1]);
        layout.panes.get_mut(&1).expect("pane").exited = true;
        let tui = MultiPaneTui::new(layout).expect("layout");

        assert!(!input_allowed(&tui, b"guest", tui.focused_pane()));
        assert!(!should_forward_paste(&tui, b"guest"));
        assert!(!input_allowed(&tui, b"host", tui.focused_pane()));
        assert!(!should_forward_paste(&tui, b"host"));
    }
}
