//! Terminal-facing half of a local session attachment.

use std::{
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
        AGENT_OVERLAY_ANIMATION_INTERVAL, AgentOverlayRow, FooterMouseInput, KeyHandling,
        MultiPaneTui, PaneMouseProtocol, PaneViewState, copy_selection_to_clipboard,
        render_multi_pane_with_copy_feedback,
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
    let offset = tui.pane_scrollback_offset(pane_id);
    let viewport = if offset == 0 {
        screens.get(&pane_id)?.screen()?
    } else {
        history
            .get(&pane_id)
            .and_then(|history| history.viewports.get(&offset))
            .and_then(GuestScreen::screen)?
    };
    let text = tui.selected_text(viewport)?;
    copy_selection_to_clipboard(&text).ok()
}

#[derive(Clone, Copy)]
struct PendingScroll {
    request_id: u64,
    target: usize,
}

pub fn run(descriptor: &SessionDescriptor) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = crate::config::config_path()?;
    let config = crate::config::load_config_from(&config_path).map_err(|error| {
        io::Error::other(format!(
            "could not load config {}: {error}",
            config_path.display()
        ))
    })?;
    let theme = config.ui.theme;
    let sound_enabled = config.ui.notifications.sound_enabled;
    let notification_sound =
        crate::notify_sound::NotificationSound::new(config.ui.notifications.sound_path);
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
        Some(NodeMessage::AttachRejected { reason }) => return Err(io::Error::other(reason).into()),
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
    let terminal_thread = spawn_terminal_reader(wake_tx, Arc::clone(&terminal_stop));
    let mut tui = None;
    let mut screens = BTreeMap::new();
    let mut history = BTreeMap::new();
    let mut pending_scroll: BTreeMap<u64, PendingScroll> = BTreeMap::new();
    let mut next_scrollback_request_id = 1_u64;
    let mut copied_lines = None;
    let mut footer_notice = None;
    let mut dirty = false;
    let mut node_ended = false;
    let mut attach_error = None;
    let mut detach_sent = false;
    let mut last_agent_overlay_animation = Instant::now();
    let mut pending_focus = None;
    let mut pending_resync = BTreeSet::new();
    let mut history_refresh = BTreeSet::new();
    let mut local_peer_id = Vec::new();
    let mut join_code = None;
    let mut pending_wake = None;
    let mut next_perf_id = 1_u64;
    let mut draw_perf_id = None;

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
                        local_peer_id: next_local_peer_id,
                        tab_id,
                        pane_id,
                        join_code: next_join_code,
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
                        apply_focus(view, &mut pending_focus, tab_id, pane_id)?;
                        if sound_enabled {
                            for _ in apply_rosters(view, rosters) {
                                notification_sound.play();
                            }
                        } else {
                            let _ = apply_rosters(view, rosters);
                        }
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
                        join_code = next_join_code;
                        if let Some(tui) = tui.as_mut() {
                            tui.set_agent_overlay_viewport(terminal.size()?.into());
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
                        let Some(pending) = pending_scroll.remove(&pane_id) else {
                            continue;
                        };
                        if pending.request_id != request_id {
                            continue;
                        }
                        let Some(snapshot) = snapshot else {
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
                    }
                    NodeMessage::Leases { leases } => {
                        if let Some(view) = tui.as_mut() {
                            apply_leases(view, leases);
                            dirty = true;
                        }
                    }
                    NodeMessage::Rosters { rosters } => {
                        if let Some(view) = tui.as_mut() {
                            if sound_enabled {
                                for _ in apply_rosters(view, rosters) {
                                    notification_sound.play();
                                }
                            } else {
                                let _ = apply_rosters(view, rosters);
                            }
                            dirty = true;
                        }
                    }
                    NodeMessage::Focus { tab_id, pane_id } => {
                        if let Some(view) = tui.as_mut() {
                            apply_focus(view, &mut pending_focus, tab_id, pane_id)?;
                            dirty = true;
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
                        join_code.as_deref(),
                        Some(&local_peer_id),
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
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match tui.handle_key(key, terminal.size()?.into()) {
                    KeyHandling::Quit => {
                        write_message(&mut stream, &ClientMessage::Detach { generation })?;
                        detach_sent = true;
                        break;
                    }
                    KeyHandling::Consumed(intents) => {
                        send_intents(&mut stream, tui, intents, &mut pending_focus)?;
                        dirty = true;
                    }
                    KeyHandling::Forward => {
                        let kitty_keyboard_active = screens
                            .get(&tui.focused_pane())
                            .is_some_and(GuestScreen::kitty_keyboard_active);
                        if input_allowed(tui, &local_peer_id)
                            && let Some(bytes) =
                                client_key_bytes(key.code, key.modifiers, kitty_keyboard_active)
                        {
                            let perf_id = next_perf_id_if_enabled(&mut next_perf_id);
                            if let Some(perf_id) = perf_id {
                                crate::perf::log(&format!("P2PMUX_PERF id={perf_id} client_input"));
                            }
                            write_message(&mut stream, &ClientMessage::Input { bytes, perf_id })?;
                            history.remove(&tui.focused_pane());
                            pending_scroll.remove(&tui.focused_pane());
                            tui.set_pane_scrollback_offset(tui.focused_pane(), 0);
                        }
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
                        &ClientMessage::Input {
                            bytes: text.into_bytes(),
                            perf_id,
                        },
                    )?;
                    history.remove(&tui.focused_pane());
                    pending_scroll.remove(&tui.focused_pane());
                    tui.set_pane_scrollback_offset(tui.focused_pane(), 0);
                }
                dirty = true;
            }
            Event::Mouse(mouse) => {
                let area = terminal.size()?.into();
                if tui.modal_open() {
                    // Blocking dialogs consume mouse input without affecting panes.
                } else if tui.overlay_open() {
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) {
                        tui.scroll_agent_overlay(
                            area,
                            matches!(mouse.kind, MouseEventKind::ScrollUp),
                        );
                    } else if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        let handling = tui.handle_mouse(
                            mouse,
                            area,
                            FooterMouseInput {
                                copied_lines,
                                footer_notice: footer_notice.as_deref(),
                                join_code: join_code.as_deref(),
                                ..FooterMouseInput::default()
                            },
                            PaneMouseProtocol::default(),
                        );
                        send_intents(&mut stream, tui, handling.intents, &mut pending_focus)?;
                    }
                } else if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let pane_id = tui.pane_at_or_focused_for_mouse(mouse.column, mouse.row, area);
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    let scrollback_len = history
                        .get(&pane_id)
                        .map(HistoryCache::available_rows)
                        .unwrap_or(1_000);
                    let current = tui.pane_scrollback_offset(pane_id);
                    let target = if up {
                        current.saturating_add(3)
                    } else {
                        current.saturating_sub(3)
                    };
                    let cached = history
                        .get(&pane_id)
                        .is_some_and(|history| history.viewports.contains_key(&target));
                    if target > 0 && !cached {
                        request_scrollback(
                            &mut stream,
                            pane_id,
                            target,
                            &history,
                            &mut pending_scroll,
                            &mut next_scrollback_request_id,
                        )?;
                    } else {
                        tui.scroll_mouse_pane(mouse.column, mouse.row, area, scrollback_len, up);
                        if !up && tui.pane_scrollback_offset(pane_id) == 0 {
                            history.remove(&pane_id);
                        }
                    }
                } else {
                    let handling = tui.handle_mouse(
                        mouse,
                        area,
                        FooterMouseInput {
                            copied_lines,
                            footer_notice: footer_notice.as_deref(),
                            join_code: join_code.as_deref(),
                            ..FooterMouseInput::default()
                        },
                        focused_pane_mouse_protocol(tui, &screens, &local_peer_id),
                    );
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        copied_lines = None;
                        footer_notice = None;
                    }
                    if let Some(bytes) = handling.forward_bytes {
                        let perf_id = next_perf_id_if_enabled(&mut next_perf_id);
                        write_message(&mut stream, &ClientMessage::Input { bytes, perf_id })?;
                    }
                    send_intents(&mut stream, tui, handling.intents, &mut pending_focus)?;
                    if handling.copy_selection_requested {
                        copied_lines = copy_attach_selection(tui, &screens, &history);
                    }
                    if let Some(command) = handling.join_copy_command
                        && copy_selection_to_clipboard(&command).is_ok()
                    {
                        copied_lines = None;
                        footer_notice = Some(String::from("copied join command"));
                    }
                }
                dirty = true;
            }
            Event::Resize(cols, rows) => {
                terminal.resize(Rect::new(0, 0, cols, rows))?;
                if !tui.modal_open() {
                    tui.set_agent_overlay_viewport(Rect::new(0, 0, cols, rows));
                    history.clear();
                    pending_scroll.clear();
                    write_message(&mut stream, &ClientMessage::Resize { cols, rows })?;
                }
                dirty = true;
            }
            _ => {}
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
    if node_ended {
        println!("p2pmux node ended");
    } else {
        println!(
            "Detached. Resume: p2pmux --resume  |  Attach: p2pmux attach {}  |  Kill: p2pmux kill {}",
            descriptor.name, descriptor.name
        );
    }
    Ok(())
}

fn refresh_tui_timers(
    tui: &mut MultiPaneTui,
    now: Instant,
    last_agent_overlay_animation: &mut Instant,
) -> bool {
    let mut dirty = tui.expire_chord_mode(now) || tui.expire_agent_toggle(now);
    if tui.agent_overlay_has_working_rows()
        && now.duration_since(*last_agent_overlay_animation) >= AGENT_OVERLAY_ANIMATION_INTERVAL
    {
        *last_agent_overlay_animation = now;
        dirty = true;
    }
    dirty
}

/// The mouse reporting the focused pane's child has turned on, if any.
fn focused_pane_mouse_protocol(
    tui: &MultiPaneTui,
    screens: &BTreeMap<u64, GuestScreen>,
    local_peer_id: &[u8],
) -> PaneMouseProtocol {
    if !input_allowed(tui, local_peer_id) {
        return PaneMouseProtocol::default();
    }
    screens
        .get(&tui.focused_pane())
        .and_then(GuestScreen::screen)
        .map(PaneMouseProtocol::from_screen)
        .unwrap_or_default()
}

fn input_allowed(tui: &MultiPaneTui, local_peer_id: &[u8]) -> bool {
    tui.snapshot()
        .panes
        .get(&tui.focused_pane())
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
    !tui.overlay_open() && !tui.modal_open() && input_allowed(tui, local_peer_id)
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

fn apply_rosters(
    view: &mut MultiPaneTui,
    rosters: Vec<crate::local_ipc::AgentOverlaySnapshotRow>,
) -> Vec<u64> {
    view.update_attached_agent_rows(
        rosters
            .into_iter()
            .filter_map(|row| {
                Some(AgentOverlayRow {
                    pane_id: row.pane_id,
                    tab_ordinal: 0,
                    pane_ordinal: 0,
                    tab_label: String::new(),
                    pane_label: String::new(),
                    kind: row.kind,
                    cwd: row.cwd,
                    state: AgentRosterState::try_from(row.state).ok()?,
                    working_since_unix_ms: row.working_since_unix_ms,
                    host: row.host,
                    controller: row.controller,
                })
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

    use crate::{
        layout::{LayoutSnapshot, Member, Node, Pane, Tab},
        local_ipc::{AgentOverlaySnapshotRow, PaneScreenSnapshot},
        screen::HostScreen,
        tui::{AGENT_TOGGLE_WINDOW, UiIntent},
    };

    use super::*;

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
            pane_id,
            kind: String::from("codex"),
            cwd: String::from("/repo"),
            state,
            working_since_unix_ms: 1,
            host: String::from("Host"),
            controller: String::from("free"),
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
    fn snapshot_focus_replaces_pending_focus_when_layout_removes_it() {
        let mut tui = None;
        let mut pending_focus = Some((1, 2));

        apply_focus_snapshot(&mut tui, layout(&[1]), 1, 1, &mut pending_focus);

        let tui = tui.unwrap();
        assert_eq!((tui.current_tab(), tui.focused_pane()), (1, 1));
        assert_eq!(pending_focus, None);
    }

    #[test]
    fn attach_client_timer_opens_overlay_and_double_tap_forwards_ctrl_a() {
        let mut tui = Some(MultiPaneTui::new(layout(&[1])).unwrap());
        let tui = tui.as_mut().unwrap();
        let area = Rect::new(0, 0, 80, 24);
        let ctrl_a = crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut animation = Instant::now();

        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
        assert!(tui.overlay_open());
        assert!(!refresh_tui_timers(
            tui,
            Instant::now() + Duration::from_secs(1),
            &mut animation,
        ));
        assert!(tui.overlay_open());
        assert_eq!(
            tui.handle_key(
                crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
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
            refresh_tui_timers(tui, Instant::now() + AGENT_TOGGLE_WINDOW, &mut animation);
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
        refresh_tui_timers(tui, Instant::now() + AGENT_TOGGLE_WINDOW, &mut animation);
        assert_eq!(
            tui.handle_key(
                crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                area
            ),
            KeyHandling::Consumed(vec![]),
        );
    }

    #[test]
    fn modal_overlay_blocks_paste_and_resize_before_open_sets_scroll_bounds() {
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
        let area = Rect::new(0, 0, 24, 8);
        tui.set_agent_overlay_viewport(area);
        tui.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            area,
        );
        let mut animation = Instant::now();
        refresh_tui_timers(tui, Instant::now() + AGENT_TOGGLE_WINDOW, &mut animation);

        assert!(!should_forward_paste(tui, b"host"));
        assert!(tui.scroll_agent_overlay(area, false));
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

    #[test]
    fn locked_guest_pane_suppresses_key_and_paste_forwarding() {
        let mut layout = layout(&[1]);
        layout.panes.get_mut(&1).expect("pane").locked = true;
        let tui = MultiPaneTui::new(layout).expect("layout");

        assert!(!input_allowed(&tui, b"guest"));
        assert!(!should_forward_paste(&tui, b"guest"));
        assert!(input_allowed(&tui, b"host"));
        assert!(should_forward_paste(&tui, b"host"));
    }

    #[test]
    fn exited_pane_suppresses_key_and_paste_for_every_peer() {
        let mut layout = layout(&[1]);
        layout.panes.get_mut(&1).expect("pane").exited = true;
        let tui = MultiPaneTui::new(layout).expect("layout");

        assert!(!input_allowed(&tui, b"guest"));
        assert!(!should_forward_paste(&tui, b"guest"));
        assert!(!input_allowed(&tui, b"host"));
        assert!(!should_forward_paste(&tui, b"host"));
    }
}
