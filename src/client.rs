//! Terminal-facing half of a local session attachment.

use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    local_ipc::{ClientMessage, LocalHistorySnapshot, NodeMessage, ScreenUpdate},
    protocol::AgentRosterState,
    screen::{ApplyDelta, GuestScreen},
    session_store::SessionDescriptor,
    tui::{
        AGENT_OVERLAY_ANIMATION_INTERVAL, AgentOverlayRow, KeyHandling, MultiPaneTui,
        PaneViewState, copy_selection_to_clipboard, render_multi_pane_with_copy_feedback,
    },
};

#[derive(Default)]
struct LocalHistory {
    grid: Option<(u16, u16)>,
    total_rows: usize,
    rows: Vec<Vec<u8>>,
}

impl LocalHistory {
    fn replace(&mut self, snapshot: LocalHistorySnapshot, grid: (u16, u16)) -> usize {
        let grid_changed = self
            .grid
            .replace(grid)
            .is_some_and(|previous| previous != grid);
        let previous_total = if grid_changed { 0 } else { self.total_rows };
        self.rows = snapshot
            .rows
            .into_iter()
            .filter_map(|row| STANDARD.decode(row).ok())
            .collect();
        self.total_rows = snapshot.total_rows;
        if grid_changed || snapshot.total_rows < previous_total {
            0
        } else {
            snapshot.total_rows.saturating_sub(previous_total)
        }
    }

    fn available_rows(&self) -> usize {
        self.rows.len()
    }

    fn viewport(&self, live: &vt100::Screen) -> vt100::Screen {
        let (rows, cols) = live.size();
        let mut parser = vt100::Parser::new(rows, cols, self.rows.len());
        for row in &self.rows {
            parser.process(row);
            parser.process(b"\r\n");
        }
        parser.process(&live.contents_formatted());
        parser.screen().clone()
    }
}

fn copy_attach_selection(
    tui: &MultiPaneTui,
    screens: &BTreeMap<u64, GuestScreen>,
    local_history: &BTreeMap<u64, LocalHistory>,
) -> Option<usize> {
    let pane_id = tui.selection_pane()?;
    let live = screens.get(&pane_id)?.screen()?;
    let mut viewport = local_history
        .get(&pane_id)
        .filter(|history| !history.rows.is_empty())
        .map_or_else(|| live.clone(), |history| history.viewport(live));
    viewport.set_scrollback(tui.pane_scrollback_offset(pane_id));
    let text = tui.selected_text(&viewport)?;
    copy_selection_to_clipboard(&text).ok()
}

pub fn run(descriptor: &SessionDescriptor) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = crate::config::config_path()?;
    let theme = crate::config::load_config_from(&config_path)
        .map_err(|error| {
            io::Error::other(format!(
                "could not load config {}: {error}",
                config_path.display()
            ))
        })?
        .ui
        .theme;
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
    let (messages, reader_thread) = spawn_message_reader(reader);
    let mut guard = ClientTerminalGuard::enter(&descriptor.name)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, initial_cols, initial_rows)),
        },
    )?;
    let mut tui = None;
    let mut screens = BTreeMap::new();
    let mut local_history = BTreeMap::new();
    let mut copied_lines = None;
    let mut dirty = false;
    let mut node_ended = false;
    let mut attach_error = None;
    let mut detach_sent = false;
    let mut last_agent_overlay_animation = Instant::now();
    let mut pending_focus = None;
    let mut local_peer_id = Vec::new();

    'attached: loop {
        loop {
            match messages.try_recv() {
                Ok(ReaderEvent::Message(message)) => {
                    if let NodeMessage::Snapshot {
                        room_name,
                        layout,
                        screens: next_screens,
                        leases,
                        rosters,
                        local_peer_id: next_local_peer_id,
                        tab_id,
                        pane_id,
                        ..
                    } = *message
                    {
                        let resync = apply_snapshot(
                            &mut tui,
                            theme,
                            &mut screens,
                            &mut local_history,
                            room_name,
                            *layout,
                            next_screens,
                            leases,
                            rosters,
                            tab_id,
                            pane_id,
                            &mut pending_focus,
                        )?;
                        for pane_id in resync {
                            write_message(&mut stream, &ClientMessage::ResyncScreen { pane_id })?;
                        }
                        local_peer_id = next_local_peer_id;
                        if let Some(tui) = tui.as_mut() {
                            tui.set_agent_overlay_viewport(terminal.size()?.into());
                        }
                        dirty = true;
                    }
                }
                Ok(ReaderEvent::Ended) | Err(TryRecvError::Disconnected) => {
                    node_ended = true;
                    break 'attached;
                }
                Ok(ReaderEvent::DecodeError(error)) => {
                    attach_error = Some(error);
                    break 'attached;
                }
                Ok(ReaderEvent::ReadError(error)) => {
                    attach_error = Some(error);
                    node_ended = true;
                    break 'attached;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(tui) = tui.as_mut() {
            dirty |= refresh_tui_timers(tui, Instant::now(), &mut last_agent_overlay_animation);
        }
        if dirty {
            if let Some(tui) = tui.as_ref() {
                terminal.draw(|frame| {
                    let viewport_screens = screens
                        .iter()
                        .filter_map(|(pane_id, screen)| {
                            screen.screen().map(|screen| {
                                let screen = local_history
                                    .get(pane_id)
                                    .filter(|history| !history.rows.is_empty())
                                    .map_or_else(
                                        || screen.clone(),
                                        |history| history.viewport(screen),
                                    );
                                (*pane_id, screen)
                            })
                        })
                        .collect::<BTreeMap<_, _>>();
                    let visible = viewport_screens
                        .iter()
                        .map(|(pane_id, screen)| (*pane_id, screen))
                        .collect();
                    render_multi_pane_with_copy_feedback(frame, tui, &visible, copied_lines);
                })?;
            }
            dirty = false;
        }
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        let Some(tui) = tui.as_mut() else {
            continue;
        };
        match event::read()? {
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
                            write_message(&mut stream, &ClientMessage::Input { bytes })?;
                        }
                    }
                }
            }
            Event::Paste(text) => {
                if should_forward_paste(tui, &local_peer_id) {
                    write_message(
                        &mut stream,
                        &ClientMessage::Input {
                            bytes: text.into_bytes(),
                        },
                    )?;
                }
                dirty = true;
            }
            Event::Mouse(mouse) => {
                let area = terminal.size()?.into();
                if tui.overlay_open() {
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) {
                        tui.scroll_agent_overlay(
                            area,
                            matches!(mouse.kind, MouseEventKind::ScrollUp),
                        );
                    } else if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        let handling = tui.handle_mouse(mouse, area);
                        send_intents(&mut stream, tui, handling.intents, &mut pending_focus)?;
                    }
                } else if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let pane_id = tui.pane_at_or_focused_for_mouse(mouse.column, mouse.row, area);
                    let scrollback_len = local_history.get(&pane_id).map_or_else(
                        || {
                            screens
                                .get(&pane_id)
                                .and_then(GuestScreen::screen)
                                .map(available_scrollback)
                                .unwrap_or_default()
                        },
                        LocalHistory::available_rows,
                    );
                    tui.scroll_mouse_pane(
                        mouse.column,
                        mouse.row,
                        area,
                        scrollback_len,
                        matches!(mouse.kind, MouseEventKind::ScrollUp),
                    );
                } else {
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        copied_lines = None;
                    }
                    let handling = tui.handle_mouse(mouse, area);
                    send_intents(&mut stream, tui, handling.intents, &mut pending_focus)?;
                    if handling.copy_selection_requested {
                        copied_lines = copy_attach_selection(tui, &screens, &local_history);
                    }
                }
                dirty = true;
            }
            Event::Resize(cols, rows) => {
                terminal.resize(Rect::new(0, 0, cols, rows))?;
                tui.set_agent_overlay_viewport(Rect::new(0, 0, cols, rows));
                write_message(&mut stream, &ClientMessage::Resize { cols, rows })?;
                dirty = true;
            }
            _ => {}
        }
    }
    if !node_ended && !detach_sent {
        let _ = write_message(&mut stream, &ClientMessage::Detach { generation });
    }
    let _ = stream.shutdown(Shutdown::Both);
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

fn input_allowed(tui: &MultiPaneTui, local_peer_id: &[u8]) -> bool {
    tui.snapshot()
        .panes
        .get(&tui.focused_pane())
        .is_none_or(|pane| !pane.locked || pane.host_peer_id == local_peer_id)
}

fn should_forward_paste(tui: &MultiPaneTui, local_peer_id: &[u8]) -> bool {
    !tui.overlay_open() && input_allowed(tui, local_peer_id)
}

#[allow(clippy::too_many_arguments)]
fn apply_snapshot(
    tui: &mut Option<MultiPaneTui>,
    theme: crate::config::UiTheme,
    screens: &mut BTreeMap<u64, GuestScreen>,
    local_history: &mut BTreeMap<u64, LocalHistory>,
    room_name: String,
    layout: crate::layout::LayoutSnapshot,
    next_screens: Vec<crate::local_ipc::PaneScreenSnapshot>,
    leases: Vec<crate::local_ipc::PaneLeaseSnapshot>,
    rosters: Vec<crate::local_ipc::AgentOverlaySnapshotRow>,
    tab_id: u64,
    pane_id: u64,
    pending_focus: &mut Option<(u64, u64)>,
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
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
    view.set_title(format!("p2pmux ({room_name})"));
    screens.retain(|pane_id, _| view.snapshot().panes.contains_key(pane_id));
    local_history.retain(|pane_id, _| view.snapshot().panes.contains_key(pane_id));
    let mut resync = Vec::new();
    for frame in next_screens {
        let pane_id = frame.pane_id;
        let screen = screens.entry(frame.pane_id).or_default();
        match frame.state {
            ScreenUpdate::Snapshot {
                sequence,
                snapshot,
                kitty_keyboard_active,
            } => {
                screen.apply_snapshot(sequence, &snapshot)?;
                screen.set_kitty_keyboard_active(kitty_keyboard_active);
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
        if let Some(snapshot) = frame.local_history
            && let Some(screen) = screen.screen()
        {
            let local = local_history.entry(pane_id).or_default();
            let appended = local.replace(snapshot, screen.size());
            if appended > 0 {
                view.pin_scrollback_after_output(pane_id, appended, local.available_rows());
            }
        }
    }
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
    view.set_agent_rows(
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
    );
    reconcile_snapshot_focus(view, pending_focus, tab_id, pane_id)?;
    Ok(resync)
}

fn reconcile_snapshot_focus(
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

fn spawn_message_reader(
    mut reader: BufReader<UnixStream>,
) -> (Receiver<ReaderEvent>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let thread = thread::spawn(move || {
        loop {
            match read_message(&mut reader) {
                Ok(Some(message)) => {
                    if sender
                        .send(ReaderEvent::Message(Box::new(message)))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(ReaderEvent::Ended);
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    if sender
                        .send(ReaderEvent::DecodeError(error.to_string()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(ReaderEvent::ReadError(error.to_string()));
                    return;
                }
            }
        }
    });
    (receiver, thread)
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

fn available_scrollback(screen: &vt100::Screen) -> usize {
    let mut screen = screen.clone();
    screen.set_scrollback(crate::screen::SCROLLBACK_LINES);
    screen.scrollback()
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
        thread,
        time::{Duration, Instant},
    };

    use portable_pty::{CommandBuilder, PtySize};

    use crate::{
        layout::{LayoutSnapshot, Member, Node, Pane, Tab},
        local_ipc::{AgentOverlaySnapshotRow, LocalHistorySnapshot},
        pty_host::PtyHost,
        screen::HostScreen,
        tui::{AGENT_TOGGLE_WINDOW, UiIntent},
    };

    use super::*;

    fn read_until(host: &mut PtyHost, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = String::new();
        while Instant::now() < deadline {
            while let Some(bytes) = host
                .try_read_output()
                .expect("PTY reader should stay healthy")
            {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
            if output.contains(expected) {
                return output;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("did not receive {expected:?}; received {output:?}");
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
    fn local_history_rebuilds_a_scrollback_viewport() {
        let mut host = HostScreen::new(1, 3).unwrap();
        host.process_pty(b"one\r\ntwo").unwrap();
        let (total_rows, rows) = host.visual_scrollback(1_000, 256 * 1024);
        let mut history = LocalHistory::default();
        assert!(
            history.replace(
                LocalHistorySnapshot {
                    total_rows,
                    rows: rows.into_iter().map(|row| STANDARD.encode(row)).collect(),
                },
                host.screen().size(),
            ) > 0
        );

        let mut viewport = history.viewport(host.screen());
        viewport.set_scrollback(history.available_rows());
        assert!(viewport.contents().contains("one"));
    }

    #[test]
    fn shift_enter_encodes_as_lf_and_agent_treats_it_as_newline() {
        let bytes = client_key_bytes(KeyCode::Enter, KeyModifiers::SHIFT, false)
            .expect("Shift+Enter should encode");
        assert_eq!(bytes, b"\n");

        let probe = format!(
            "{}/tests/fixtures/agent_newline_probe.js",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut command = CommandBuilder::new("node");
        command.arg(probe);
        let mut host = PtyHost::spawn(
            command,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("Node probe PTY should spawn");

        assert!(read_until(&mut host, "READY").contains("READY"));
        host.write_input(&bytes).expect("PTY should accept LF");
        assert!(read_until(&mut host, "NEWLINE").contains("NEWLINE"));
        host.shutdown().expect("PTY should shut down cleanly");
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
    fn locked_guest_pane_suppresses_key_and_paste_forwarding() {
        let mut layout = layout(&[1]);
        layout.panes.get_mut(&1).expect("pane").locked = true;
        let tui = MultiPaneTui::new(layout).expect("layout");

        assert!(!input_allowed(&tui, b"guest"));
        assert!(!should_forward_paste(&tui, b"guest"));
        assert!(input_allowed(&tui, b"host"));
        assert!(should_forward_paste(&tui, b"host"));
    }
}
