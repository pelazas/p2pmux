//! The terminal loop: poll events, apply them, drain panes, draw a frame.

use std::{collections::BTreeMap, error::Error, io, time::Instant};

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
        missed_resize,
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
        let scrollback = self.tui.scrollback_offset(selection.pane_id);
        let text = self
            .local
            .get(&selection.pane_id)
            .and_then(|pane| {
                selection_text(
                    viewed_screen(pane.screen.screen(), scrollback).as_ref(),
                    selection,
                )
            })
            .or_else(|| {
                self.remote
                    .get(&selection.pane_id)
                    .and_then(|pane| pane.screen.screen())
                    .and_then(|screen| {
                        selection_text(viewed_screen(screen, scrollback).as_ref(), selection)
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
            for pane in self.local.values_mut() {
                if !pane.exited {
                    changed |= pane.apply_agent_snapshot(&scan, now);
                }
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
        changed |= self.tick_failover()?;
        changed |= self.refresh_local_views();
        changed |= self.refresh_agent_rows();
        Ok(changed)
    }

    pub(in crate::tui) fn spawn_remote_shutdown(&self, pane: GuestPane) {
        self.runtime.spawn(async move { pane.shutdown().await });
    }
}
