//! The terminal loop: poll events, apply them, drain panes, draw a frame.

use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    io,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, SetTitle, enable_raw_mode},
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};

use crate::{
    agent_detect::AgentScan,
    layout::PaneId,
    session::GuestPane,
    tui::{
        AGENT_OVERLAY_ANIMATION_INTERVAL, KeyHandling, ShareView, TerminalGuard,
        clear_before_first_frame, enable_keyboard_enhancement,
        input::{
            events::{
                MAX_EVENTS_PER_CYCLE, begin_synchronized_output, collect_pending_events,
                end_synchronized_output, event_poll_timeout, frame_due,
            },
            keys::PendingEscape,
        },
        keep_attributes_through_no_color, missed_resize,
        pane::remote::{RemotePaneDrain, SharedRemotePane},
        render::{
            panes::render_shared_multi_pane,
            vt::{available_scrollback, viewed_screen},
        },
        resize_recheck_due,
        selection::{copy_selection_to_clipboard, selection_text},
        share::share_copy_result,
    },
};

use super::SharedLayoutRuntime;

impl SharedLayoutRuntime {
    pub(in crate::tui) fn handle_key(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> Result<bool, Box<dyn Error>> {
        let previously_focused = self.tui.focused_pane();
        let zoom_before = self.tui.zoomed_pane();
        let quit = match self.tui.handle_key(key, area) {
            // Nothing behind this process to leave running, so both answers
            // mean the same thing here. The prompt that distinguishes them is
            // only offered where the distinction exists — see
            // `MultiPaneTui::set_detachable`.
            KeyHandling::Quit(_) => Ok::<bool, Box<dyn Error>>(true),
            KeyHandling::Consumed(intents) => {
                for intent in intents {
                    self.handle_intent(intent)?;
                }
                if let Some(request) = self.tui.take_share_copy_request() {
                    self.share_notice = Some(share_copy_result(
                        request,
                        self.invite.ticket.as_deref(),
                        self.invite.code.as_deref(),
                    ));
                }
                if let Some(command) = self.tui.take_update_copy_request() {
                    self.tui
                        .set_home_notice(Some(crate::tui::update_copy_result(&command)));
                }
                // A foreground session hands out the same invite an attached one
                // does, so the add-machine panel has to record the ticket here
                // too. Without it the machine that joins is never remembered.
                if self.tui.take_pair_offer()
                    && let Some(ticket) = self.invite.ticket.as_deref()
                    && let Err(error) = crate::pairing::offer(ticket)
                {
                    self.share_notice = Some(format!("could not record the pairing: {error}"));
                }
                // The notice belongs to one visit to the modal, not to the session.
                if !self.tui.share_open() && !self.tui.add_machine_open() {
                    self.share_notice = None;
                }
                Ok(false)
            }
            KeyHandling::Forward => {
                self.forward_key(key)?;
                Ok(false)
            }
        }?;
        // A zoomed pane is alone on the screen and gets all of it, so the PTY
        // behind it has to grow to match -- and shrink back when the zoom ends.
        if self.tui.zoomed_pane() != zoom_before {
            self.reflow_local_panes(area)?;
        }
        self.release_blurred_pane(previously_focused)?;
        Ok(quit)
    }

    pub(in crate::tui) fn release_blurred_pane(
        &mut self,
        previously_focused: PaneId,
    ) -> Result<(), Box<dyn Error>> {
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
        keep_attributes_through_no_color();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;
        clear_before_first_frame(&mut terminal, Rect::new(0, 0, cols, rows))?;
        self.tui.set_home_viewport_for(Rect::new(0, 0, cols, rows));
        let mut dirty = true;
        let mut reset_outer_pending = false;
        let mut seen_outer_resets = BTreeMap::new();
        let mut last_draw: Option<Instant> = None;
        let mut pending_escape = PendingEscape::default();
        // The viewport is fixed, so ratatui never resizes it for us: the drawn
        // size only follows the window because of the call below.
        let mut viewport = (cols, rows);
        let mut last_size_check: Option<Instant> = None;
        loop {
            dirty |= self.drain()?;
            if self.tui.expire_chord_mode(Instant::now()) {
                dirty = true;
            }
            if self.tui.expire_home_toggle(Instant::now()) {
                dirty = true;
            }
            // A pointer parked past a pane's edge sends no more drag events, so
            // this clock is what keeps the rows coming while it sits there.
            if let Some((pane_id, up)) = self.tui.selection_autoscroll_due(Instant::now()) {
                let scrollback_len = self
                    .local
                    .get(&pane_id)
                    .map(|pane| pane.screen.retained_scrollback())
                    .or_else(|| {
                        self.remote
                            .get(&pane_id)
                            .and_then(|pane| pane.screen.screen())
                            .map(available_scrollback)
                    })
                    .unwrap_or(0);
                self.tui.scroll_pane(pane_id, scrollback_len, up);
                dirty |= self
                    .tui
                    .follow_selection_autoscroll(Rect::new(0, 0, cols, rows));
            }
            let now = Instant::now();
            if self.tui.home_has_working_rows()
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
            if viewport != (cols, rows) {
                viewport = (cols, rows);
                terminal.resize(Rect::new(0, 0, cols, rows))?;
                dirty = true;
            }
            // The redraw chord, answered on the same path a pane's own reset
            // takes. See `MultiPaneTui::take_repaint_request`.
            if self.tui.take_repaint_request() {
                reset_outer_pending = true;
                dirty = true;
            }
            if dirty && frame_due(last_draw) {
                for (pane_id, pane) in &self.local {
                    let screen = pane.screen.current_frame();
                    if screen.reset_outer
                        && seen_outer_resets.get(pane_id).copied() != Some(screen.sequence)
                    {
                        seen_outer_resets.insert(*pane_id, screen.sequence);
                        reset_outer_pending = true;
                    }
                }
                if reset_outer_pending {
                    clear_before_first_frame(&mut terminal, Rect::new(0, 0, cols, rows))?;
                    reset_outer_pending = false;
                }
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
                            code: self.invite.code.as_deref(),
                            ticket: self.invite.ticket.as_deref(),
                            notice: self.share_notice.as_deref(),
                        },
                        link.as_deref(),
                    );
                })?;
                end_synchronized_output()?;
                dirty = false;
                last_draw = Some(Instant::now());
            }
            if resize_recheck_due(last_size_check, Instant::now()) {
                last_size_check = Some(Instant::now());
                if let Some((width, height)) = missed_resize((cols, rows), terminal::size()) {
                    self.handle_terminal_event(
                        Event::Resize(width, height),
                        &mut cols,
                        &mut rows,
                        &mut pending_escape,
                        &mut dirty,
                    )?;
                }
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
    pub(in crate::tui) fn handle_terminal_event(
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
                if !self.tui.home_open() && !self.tui.modal_open() {
                    self.tui.exit_chord_mode();
                    self.forward_paste(&text)?;
                }
                *dirty = true;
            }
            Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Moved) => {
                if !self.tui.home_open() && !self.tui.modal_open() {
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
                let zoom_before = self.tui.zoomed_pane();
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
                // Clicking a sibling stands the zoom down, which gives the
                // panes their split rects back.
                if self.tui.zoomed_pane() != zoom_before {
                    self.reflow_local_panes(area)?;
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
                if self.tui.home_open() {
                    *dirty |= self
                        .tui
                        .scroll_home(area, matches!(mouse.kind, MouseEventKind::ScrollUp));
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
                    // A local pane owns a `HostScreen`, which already counts its retained
                    // rows; `available_scrollback` would clone the whole buffer to learn
                    // the same number, once per wheel notch.
                    .map(|pane| pane.screen.retained_scrollback())
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
                    .set_home_viewport_for(Rect::new(0, 0, width, height));
                self.reflow_local_panes(Rect::new(0, 0, width, height))?;
                *dirty = true;
            }
            _ => {}
        }
        Ok(false)
    }

    pub(in crate::tui) fn copy_selection_to_clipboard(&mut self) {
        let Some(selection) = self.tui.selection() else {
            return;
        };
        // A pane this process hosts keeps its whole buffer, so any offset the
        // selection reaches is one `viewed_screen` can produce.
        let text = self
            .local
            .get(&selection.pane_id)
            .and_then(|pane| {
                selection_text(selection, |offset| {
                    Some(viewed_screen(pane.screen.screen(), offset))
                })
            })
            .or_else(|| {
                self.remote
                    .get(&selection.pane_id)
                    .and_then(|pane| pane.screen.screen())
                    .and_then(|screen| {
                        selection_text(selection, |offset| Some(viewed_screen(screen, offset)))
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

    pub(in crate::tui) fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut changed = false;
        self.retry_tick = self.retry_tick.saturating_add(1);
        let mut seen_presence_epoch = self.seen_presence_epoch;
        let mut seen_agent_generations = std::mem::take(&mut self.seen_agent_generations);
        while let Some(event) = self.control.try_event(
            self.tui.snapshot().revision,
            &mut seen_presence_epoch,
            &mut seen_agent_generations,
        ) {
            self.handle_control_event(event)?;
            changed = true;
        }
        self.seen_presence_epoch = seen_presence_epoch;
        self.seen_agent_generations = seen_agent_generations;
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
            let mut pane_roots = Vec::new();
            for pane in self.local.values_mut() {
                if !pane.exited {
                    changed |= pane.apply_agent_snapshot(&scan, now);
                    pane_roots.extend(pane.session_child_pid());
                }
            }
            // The same scan, asked the other question: which agents on this
            // machine are in none of those trees. That is a bot under systemd,
            // which is how both assistant agents are meant to run and the one
            // shape the inbox could not see.
            // The node pids the store already told us about, so a node whose
            // command line the sampler could not read is still recognised as
            // one. Empty on the very first pass, which is the pass
            // `name_their_sessions` then fills in.
            let known_nodes = self.session_records.keys().copied().collect();
            let mut loose = scan.loose_agents(&pane_roots, &known_nodes);
            self.name_their_sessions(&mut loose);
            // …and then what each of them is doing, which the scan cannot know
            // and their hooks have left on this machine for exactly this. An
            // agent with no hooks stays `Unknown` and the row still says so.
            if let Some(directory) = crate::agent_status::default_dir() {
                crate::agent_status::attach(directory, &mut loose);
                // Records outlive nothing: the process that wrote one is the
                // only thing keeping it, and this scan just listed every
                // process on the machine.
                crate::agent_status::sweep(directory, crate::agent_status::SWEEP_GRACE, |pid| {
                    scan.knows_pid(pid)
                });
            }
            if loose != self.loose_agents {
                self.loose_agents = loose;
                changed = true;
            }
        }
        changed |= self.publish_local_agent_roster();
        // Cheap: returns immediately unless the focused pane actually moved since the last
        // drain, and a keypress cannot move focus more than once per drain.
        changed |= self.maybe_publish_presence();
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
        // A rejection this coordinator produced for its own request. Drained on
        // the main loop rather than the membership timer, because it is an
        // answer to a keypress and the person who pressed the key is watching.
        for reject in self.control.take_own_rejects() {
            self.reject_request_with_reason(reject.request_id, reject.reason);
            changed = true;
        }
        changed |= self.tick_failover()?;
        changed |= self.refresh_local_views();
        changed |= self.refresh_agent_rows();
        Ok(changed)
    }

    /// Name the p2pmux session each loose agent is sitting in, and drop the
    /// ones that are not in another session at all.
    ///
    /// The scan can see that an agent's process descends from a node; only the
    /// session store knows that node is called `dakar`. The map is cached and
    /// re-read only when a node turns up that is not in it, because this runs
    /// on a redraw path and the store is on disk — and never through
    /// `list_live`, which probes every socket and deletes the records that miss
    /// their deadline.
    ///
    /// The dropping is the other half, and it is what keeps one agent from
    /// being two rows. This scan asks "which agents on this machine are in none
    /// of *my* panes", and a second p2pmux in the same session — a window that
    /// rejoined on a ticket it still had — hosts panes that answer yes. Its
    /// agents are already on the inbox as panes of this session, put there by
    /// the node hosting them, so publishing them again listed the same process
    /// twice: once where Enter reaches it, and once as an unreachable row whose
    /// way in was `p2pmux attach` naming the session already on screen.
    fn name_their_sessions(&mut self, loose: &mut Vec<crate::agent_detect::LooseAgent>) {
        if session_records_are_worth_rereading(
            loose,
            &self.session_records,
            self.session_records_loaded,
            self.session_records_read_at.map(|read| read.elapsed()),
        ) && let Ok(store) = crate::session_store::SessionStore::for_current_user()
            && let Ok(sessions) = store.sessions_by_node_pid()
        {
            self.session_records = sessions;
            self.session_records_loaded = true;
            self.session_records_read_at = Some(Instant::now());
        }
        let ours = std::process::id();
        // Cloned rather than borrowed: the retain below reads the same map, and
        // there is one of these per node on this machine.
        let here = self.session_records.get(&ours).cloned();
        loose.retain(|agent| {
            if agent.node_pid == ours {
                return false;
            }
            let (Some(here), Some(theirs)) =
                (here.as_ref(), self.session_records.get(&agent.node_pid))
            else {
                return true;
            };
            !theirs.shares_session_with(here)
        });
        for agent in loose {
            agent.session = self
                .session_records
                .get(&agent.node_pid)
                .map(|session| session.name.clone())
                .unwrap_or_default();
        }
    }

    pub(in crate::tui) fn spawn_remote_shutdown(&self, pane: GuestPane) {
        self.runtime.spawn(async move { pane.shutdown().await });
    }
}

/// How long a session map that could not answer stands before it is asked again.
///
/// Long enough that a bot under systemd -- which is permanently unrecognised
/// and permanently correct about it -- does not put a directory read on the
/// redraw path, short enough that a node that appeared a moment ago is named
/// while the person is still looking at the row.
const SESSION_RECORDS_RETRY: Duration = Duration::from_secs(5);

/// Whether the node-pid → session map should be read off disk again.
///
/// It is cached because this runs on a redraw path and the store is on disk.
/// What decides a re-read is a loose agent the map cannot account for, and
/// there are three ways to be one:
///
/// - its node is known and the map has never heard of it. That is a session
///   that started after the last read, and it is asked for immediately: the
///   answer is certainly there.
/// - the map has never been read at all. The first loose agent asks, because an
///   empty map means both "no other sessions" and "never looked", and the two
///   have to be told apart before a row can call anything `running outside
///   p2pmux`.
/// - its node was *not* identified. This one is the reason the function exists.
///   The walk that identifies a node reads a command line out of a process
///   sampler, and a sampler that returns nothing for a process leaves the agent
///   looking like it is in no session at all -- which used to trigger nothing,
///   because "unknown node" was keyed on the node having been identified. So
///   one failed walk was not a flicker, it was a row that stayed wrong for the
///   life of the session. Reading the map is also what supplies the *next*
///   pass's `known_nodes`, which is what lets that walk succeed the second time
///   round -- so this is the retry, and it needs a rate limit rather than a
///   condition, because an agent genuinely outside p2pmux never stops asking.
fn session_records_are_worth_rereading(
    loose: &[crate::agent_detect::LooseAgent],
    records: &HashMap<u32, crate::session_store::LocalSession>,
    loaded: bool,
    since_last_read: Option<Duration>,
) -> bool {
    if loose.is_empty() {
        return false;
    }
    if !loaded {
        return true;
    }
    if loose
        .iter()
        .any(|agent| agent.node_pid != 0 && !records.contains_key(&agent.node_pid))
    {
        return true;
    }
    loose.iter().any(|agent| agent.node_pid == 0)
        && since_last_read.is_none_or(|elapsed| elapsed >= SESSION_RECORDS_RETRY)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use crate::{
        agent_detect::{AgentKind, AgentState, LooseAgent},
        session_store::LocalSession,
    };

    use super::{SESSION_RECORDS_RETRY, session_records_are_worth_rereading};

    fn agent(pid: u32, node_pid: u32) -> LooseAgent {
        LooseAgent {
            kind: AgentKind::Claude,
            pid,
            cwd: String::new(),
            start_time: None,
            node_pid,
            session: String::new(),
            state: AgentState::Unknown,
            message: String::new(),
            working_since_unix_ms: 0,
        }
    }

    fn named(node_pid: u32) -> HashMap<u32, LocalSession> {
        HashMap::from([(
            node_pid,
            LocalSession {
                name: String::from("dakar"),
                tickets: Vec::new(),
            },
        )])
    }

    #[test]
    fn a_machine_with_no_loose_agent_never_touches_the_disk() {
        assert!(!session_records_are_worth_rereading(
            &[],
            &HashMap::new(),
            true,
            None
        ));
        assert!(
            !session_records_are_worth_rereading(&[], &HashMap::new(), false, None),
            "not even the first time: there is nothing to name"
        );
    }

    #[test]
    fn the_first_loose_agent_reads_the_map_once() {
        assert!(session_records_are_worth_rereading(
            &[agent(10, 0)],
            &HashMap::new(),
            false,
            None
        ));
    }

    #[test]
    fn a_node_the_map_has_never_heard_of_is_asked_about_at_once() {
        assert!(
            session_records_are_worth_rereading(
                &[agent(10, 50)],
                &named(60),
                true,
                Some(Duration::from_millis(1))
            ),
            "a session that started since the last read is certainly on disk now"
        );
        assert!(
            !session_records_are_worth_rereading(&[agent(10, 50)], &named(50), true, None),
            "and one the map already names asks for nothing"
        );
    }

    /// Issue #121's other half: an agent whose node was never identified.
    ///
    /// The map is where the identification would come from -- it supplies the
    /// next pass's `known_nodes` -- so refusing to re-read for exactly these
    /// agents is what turned one failed walk into a permanently wrong row. It
    /// is rate-limited rather than conditional because a bot under systemd is
    /// unrecognised forever and correctly so, and it must not put a directory
    /// read on every frame.
    #[test]
    fn an_agent_with_no_node_asks_again_but_not_on_every_frame() {
        let loose = [agent(10, 0)];
        assert!(
            session_records_are_worth_rereading(&loose, &HashMap::new(), true, None),
            "the map has been read, but never since this agent turned up"
        );
        assert!(!session_records_are_worth_rereading(
            &loose,
            &HashMap::new(),
            true,
            Some(SESSION_RECORDS_RETRY / 2)
        ));
        assert!(session_records_are_worth_rereading(
            &loose,
            &HashMap::new(),
            true,
            Some(SESSION_RECORDS_RETRY)
        ));
    }
}
