//! Terminal-facing half of a local session attachment.

use std::{io::{self, BufRead, BufReader, Write}, os::unix::net::UnixStream, time::Duration};

use crossterm::{cursor, event::{self, Event, KeyCode, KeyEventKind, KeyModifiers}, execute, terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, SetTitle}};

use crate::{local_ipc::{ClientMessage, NodeMessage}, session_store::SessionDescriptor};

pub fn run(descriptor: &SessionDescriptor) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(&descriptor.socket_path)?;
    let read_stream = stream.try_clone()?;
    let mut reader = BufReader::new(read_stream);
    write_message(&mut stream, &ClientMessage::Hello { cols: terminal::size()?.0, rows: terminal::size()?.1 })?;
    let generation = match read_message(&mut reader)? {
        Some(NodeMessage::AttachAccepted { generation }) => generation,
        Some(NodeMessage::AttachRejected { reason }) => return Err(io::Error::other(reason).into()),
        _ => return Err(io::Error::other("node did not accept attachment").into()),
    };
    stream.set_nonblocking(true)?;
    let mut guard = ClientTerminalGuard::enter(&descriptor.name)?;
    let mut node_ended = false;
    loop {
        match read_message(&mut reader) {
            Ok(Some(NodeMessage::Snapshot { screens, .. })) => draw(&descriptor.name, screens.as_str().unwrap_or_default())?,
            Ok(Some(NodeMessage::Update { .. })) | Ok(Some(NodeMessage::Error { .. })) => {}
            Ok(None) => { node_ended = true; break; }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => { node_ended = true; break; }
            _ => {}
        }
        if !event::poll(Duration::from_millis(16))? { continue; }
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL {
                    write_message(&mut stream, &ClientMessage::Detach { generation })?;
                    break;
                }
                if let Some(bytes) = client_key_bytes(key.code, key.modifiers) { write_message(&mut stream, &ClientMessage::Input { bytes })?; }
            }
            Event::Paste(text) => write_message(&mut stream, &ClientMessage::Input { bytes: text.into_bytes() })?,
            Event::Resize(cols, rows) => write_message(&mut stream, &ClientMessage::Resize { cols, rows })?,
            _ => {}
        }
    }
    guard.leave()?;
    if node_ended { println!("p2pmux node ended"); }
    else { println!("Detached. Resume: p2pmux --resume  |  Attach: p2pmux attach {}  |  Kill: p2pmux kill {}", descriptor.name, descriptor.name); }
    Ok(())
}

fn draw(name: &str, screen: &str) -> io::Result<()> {
    let mut stdout = io::stdout(); execute!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    writeln!(stdout, "p2pmux ({name})")?; write!(stdout, "{screen}")?; stdout.flush()
}
fn write_message(stream: &mut UnixStream, message: &ClientMessage) -> io::Result<()> { serde_json::to_writer(&mut *stream, message).map_err(io::Error::other)?; stream.write_all(b"\n")?; stream.flush() }
fn read_message(reader: &mut BufReader<UnixStream>) -> io::Result<Option<NodeMessage>> { let mut line = String::new(); match reader.read_line(&mut line) { Ok(0) => Ok(None), Ok(_) => serde_json::from_str(&line).map(Some).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid node message")), Err(error) => Err(error) } }

fn client_key_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    match code {
        KeyCode::Char(character) if modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii_alphabetic() => Some(vec![character.to_ascii_lowercase() as u8 - b'a' + 1]),
        KeyCode::Char(character) => Some(character.to_string().into_bytes()), KeyCode::Enter => Some(b"\r".to_vec()), KeyCode::Backspace => Some(b"\x7f".to_vec()), KeyCode::Tab => Some(b"\t".to_vec()), KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()), KeyCode::Down => Some(b"\x1b[B".to_vec()), KeyCode::Left => Some(b"\x1b[D".to_vec()), KeyCode::Right => Some(b"\x1b[C".to_vec()), _ => None,
    }
}

struct ClientTerminalGuard { raw: bool, alternate: bool }
impl ClientTerminalGuard {
    fn enter(name: &str) -> io::Result<Self> { terminal::enable_raw_mode()?; execute!(io::stdout(), SetTitle(format!("p2pmux ({name})")), EnterAlternateScreen)?; Ok(Self { raw: true, alternate: true }) }
    fn leave(&mut self) -> io::Result<()> { if self.alternate { execute!(io::stdout(), LeaveAlternateScreen)?; self.alternate = false; } if self.raw { terminal::disable_raw_mode()?; self.raw = false; } Ok(()) }
}
impl Drop for ClientTerminalGuard { fn drop(&mut self) { let _ = self.leave(); } }

#[cfg(test)]
mod tests { use super::*; #[test] fn ctrl_q_is_reserved_for_detach() { assert_eq!(client_key_bytes(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(vec![3])); assert_eq!(client_key_bytes(KeyCode::Char('q'), KeyModifiers::CONTROL), Some(vec![17])); } }
