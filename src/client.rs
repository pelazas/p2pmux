//! Terminal-facing half of a local session attachment.

use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};

use crate::{
    local_ipc::{ClientMessage, NodeMessage},
    screen::GuestScreen,
    session_store::SessionDescriptor,
    tui::{KeyHandling, MultiPaneTui, PaneViewState, render_multi_pane},
};

pub fn run(descriptor: &SessionDescriptor) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut dirty = false;
    let mut node_ended = false;
    let mut attach_error = None;
    let mut detach_sent = false;

    'attached: loop {
        loop {
            match messages.try_recv() {
                Ok(ReaderEvent::Message(message)) => {
                    if let NodeMessage::Snapshot {
                        room_name,
                        layout,
                        screens: next_screens,
                        leases,
                        tab_id,
                        pane_id,
                        ..
                    } = *message
                    {
                        apply_snapshot(
                            &mut tui,
                            &mut screens,
                            room_name,
                            *layout,
                            next_screens,
                            leases,
                            tab_id,
                            pane_id,
                        )?;
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
        if dirty {
            if let Some(tui) = tui.as_ref() {
                terminal.draw(|frame| {
                    let visible = screens
                        .iter()
                        .filter_map(|(pane_id, screen)| {
                            screen.screen().map(|screen| (*pane_id, screen))
                        })
                        .collect();
                    render_multi_pane(frame, tui, &visible);
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
                        send_intents(&mut stream, tui, intents)?;
                        dirty = true;
                    }
                    KeyHandling::Forward => {
                        if let Some(bytes) = client_key_bytes(key.code, key.modifiers) {
                            write_message(&mut stream, &ClientMessage::Input { bytes })?;
                        }
                    }
                }
            }
            Event::Paste(text) => write_message(
                &mut stream,
                &ClientMessage::Input {
                    bytes: text.into_bytes(),
                },
            )?,
            Event::Mouse(mouse) => {
                let area = terminal.size()?.into();
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let pane_id = tui.pane_at_or_focused_for_mouse(mouse.column, mouse.row, area);
                    let scrollback_len = screens
                        .get(&pane_id)
                        .and_then(GuestScreen::screen)
                        .map(available_scrollback)
                        .unwrap_or_default();
                    tui.scroll_mouse_pane(
                        mouse.column,
                        mouse.row,
                        area,
                        scrollback_len,
                        matches!(mouse.kind, MouseEventKind::ScrollUp),
                    );
                } else {
                    let intents = tui.handle_mouse(mouse, area);
                    send_intents(&mut stream, tui, intents)?;
                }
                dirty = true;
            }
            Event::Resize(cols, rows) => {
                terminal.resize(Rect::new(0, 0, cols, rows))?;
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

#[allow(clippy::too_many_arguments)]
fn apply_snapshot(
    tui: &mut Option<MultiPaneTui>,
    screens: &mut BTreeMap<u64, GuestScreen>,
    room_name: String,
    layout: crate::layout::LayoutSnapshot,
    next_screens: Vec<crate::local_ipc::PaneScreenSnapshot>,
    leases: Vec<crate::local_ipc::PaneLeaseSnapshot>,
    tab_id: u64,
    pane_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let view = match tui {
        Some(view) => {
            view.apply_snapshot(layout)
                .map_err(|error| io::Error::other(format!("invalid layout snapshot: {error:?}")))?;
            view
        }
        None => tui
            .insert(MultiPaneTui::new(layout).map_err(|error| {
                io::Error::other(format!("invalid layout snapshot: {error:?}"))
            })?),
    };
    view.set_title(format!("p2pmux ({room_name})"));
    view.set_focus(tab_id, pane_id)
        .map_err(|error| io::Error::other(format!("invalid node focus: {error:?}")))?;
    screens.retain(|pane_id, _| view.snapshot().panes.contains_key(pane_id));
    for frame in next_screens {
        let screen = screens.entry(frame.pane_id).or_default();
        screen.apply_snapshot(frame.sequence, &frame.snapshot)?;
        screen.set_kitty_keyboard_active(frame.kitty_keyboard_active);
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
    Ok(())
}

fn send_intents(
    stream: &mut UnixStream,
    tui: &MultiPaneTui,
    intents: Vec<crate::tui::UiIntent>,
) -> io::Result<()> {
    for intent in intents {
        match intent {
            crate::tui::UiIntent::FocusPane { .. } | crate::tui::UiIntent::SwitchTab { .. } => {
                write_message(
                    stream,
                    &ClientMessage::Focus {
                        tab_id: tui.current_tab(),
                        pane_id: tui.focused_pane(),
                    },
                )?;
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
    serde_json::to_writer(&mut *stream, message).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
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

fn client_key_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    match code {
        KeyCode::Char(character)
            if modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii_alphabetic() =>
        {
            Some(vec![character.to_ascii_lowercase() as u8 - b'a' + 1])
        }
        KeyCode::Char(character) => Some(character.to_string().into_bytes()),
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
    raw: bool,
    alternate: bool,
    paste: bool,
    mouse: bool,
}
impl ClientTerminalGuard {
    fn enter(name: &str) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            SetTitle(format!("p2pmux ({name})")),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
        )?;
        Ok(Self {
            raw: true,
            alternate: true,
            paste: true,
            mouse: true,
        })
    }
    fn leave(&mut self) -> io::Result<()> {
        if self.mouse {
            execute!(io::stdout(), DisableMouseCapture)?;
            self.mouse = false;
        }
        if self.paste {
            execute!(io::stdout(), DisableBracketedPaste)?;
            self.paste = false;
        }
        if self.alternate {
            execute!(io::stdout(), LeaveAlternateScreen)?;
            self.alternate = false;
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
    use super::*;
    #[test]
    fn ctrl_q_is_reserved_for_detach() {
        assert_eq!(
            client_key_bytes(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec![3])
        );
        assert_eq!(
            client_key_bytes(KeyCode::Char('q'), KeyModifiers::CONTROL),
            Some(vec![17])
        );
    }
}
