//! Pinned local control protocol. Separate from the TUI attach socket types.
//!
//! The attach JSON (`ClientMessage` / `NodeMessage`) stays a private implementation
//! detail. This is the surface a script may pin against. A client on the wrong pin
//! is refused with both numbers named, and never takes the interactive attachment
//! slot.

use std::{
    error::Error,
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::protocol::AgentRosterState;

/// Independent of the peer wire pin. Moving this does not break a session
/// between two p2pmux binaries that still share [`crate::protocol::PROTOCOL_VERSION`].
pub const CTL_PROTOCOL_PIN: u32 = 1;

const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const SPAWN_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_FRAME: usize = 1024 * 1024;

/// What a ctl client sends after connecting.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CtlFromClient {
    CtlHello {
        pin: u32,
    },
    Machines,
    Agents,
    Spawn {
        machine: String,
        #[serde(default)]
        command: Vec<String>,
        /// Skip reuse of a live `chat: {command}` pane. Omitted by older
        /// clients, which keep the inbox behaviour.
        #[serde(default)]
        new: bool,
    },
    Send {
        pane_id: u64,
        keys: String,
    },
    Focus {
        agent: String,
    },
    Events,
}

/// What the node writes back on a ctl connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CtlToClient {
    CtlHelloAck {
        pin: u32,
    },
    CtlHelloRejected {
        ours: u32,
        theirs: u32,
    },
    Machines {
        machines: Vec<CtlMachine>,
    },
    Agents {
        agents: Vec<CtlAgent>,
    },
    Spawned {
        pane_id: u64,
        reused: bool,
        #[serde(default)]
        visible_to_guests: bool,
    },
    Sent {
        pane_id: u64,
    },
    Focused {
        pane_id: u64,
    },
    Event {
        id: String,
        pane_id: u64,
        kind: String,
        machine: String,
        state: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CtlMachine {
    pub name: String,
    pub reachable: bool,
    pub this_machine: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CtlAgent {
    pub id: String,
    pub pane_id: u64,
    pub kind: String,
    pub host: String,
    pub state: String,
    pub needs_you: bool,
    pub cwd: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(default)]
    pub in_another_session: bool,
}

/// The verbs `p2pmux ctl` runs. Kept here so clap in `cli` and the socket
/// client share one mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CtlAction {
    Machines,
    Agents,
    Spawn {
        machine: String,
        command: Vec<String>,
        new: bool,
    },
    Send {
        pane_id: u64,
        keys: String,
    },
    Focus {
        agent: String,
    },
    Events,
}

/// Outcome of asking the node to start a pane.
pub enum CtlSpawn {
    Reused(u64),
    Busy,
    Started(u64),
}

#[derive(Debug)]
pub struct CtlError(pub String);

impl std::fmt::Display for CtlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CtlError {}

/// What to tell somebody whose ctl client and the session node do not share a pin.
pub fn unsupported_ctl_pin_message(theirs: u32) -> String {
    let ours = CTL_PROTOCOL_PIN;
    let older = if theirs < ours {
        "The session node is running an older p2pmux than this one — upgrade it, not this machine."
    } else {
        "This machine is running an older p2pmux than the session node — upgrade this one."
    };
    format!(
        "that session speaks p2pmux ctl pin {theirs} and this p2pmux speaks {ours}, \
         so they cannot talk. {older} \
         Upgrade with `curl -fsSL https://p2pmux.com/install.sh | sh`, or \
         `brew upgrade p2pmux` if you installed it that way, then try again."
    )
}

pub fn agent_id(pane_id: u64, process_pid: u32) -> String {
    if pane_id != 0 {
        pane_id.to_string()
    } else {
        format!("pid:{process_pid}")
    }
}

pub fn agent_state_name(state: AgentRosterState) -> &'static str {
    match state {
        AgentRosterState::Idle => "idle",
        AgentRosterState::Working => "working",
        AgentRosterState::Done => "done",
        AgentRosterState::Pending => "pending",
        AgentRosterState::Error => "error",
        AgentRosterState::Unknown => "unknown",
    }
}

pub fn event_state_name(state: AgentRosterState) -> Option<&'static str> {
    match state {
        AgentRosterState::Pending => Some("needs_you"),
        AgentRosterState::Done => Some("done"),
        AgentRosterState::Error => Some("error"),
        AgentRosterState::Idle | AgentRosterState::Working | AgentRosterState::Unknown => None,
    }
}

/// How much of a `send` payload goes out in one ctl frame.
///
/// JSON escapes a NUL as six bytes, and a frame has to stay under `MAX_FRAME`.
/// Splitting the source string here leaves that margin.
pub const SEND_CHUNK_BYTES: usize = 32_000;

/// Turn `p2pmux ctl send` arguments into the bytes the pane should see.
///
/// Named keys (`Enter`, `C-c`, `Up`) become their sequences and are not padded
/// with spaces. Everything else is text: consecutive words keep a single space
/// between them, and `\n`, `\r`, `\t`, `\e`, `\\`, `\xNN` are escapes.
/// `--literal` joins the words with spaces and leaves them alone.
pub fn interpret_send_keys(args: &[String], literal: bool) -> String {
    if literal {
        return args.join(" ");
    }
    let mut out = String::new();
    let mut separated = false;
    for arg in args {
        if let Some(sequence) = named_send_key(arg) {
            separated = false;
            out.push_str(&sequence);
            continue;
        }
        if separated {
            out.push(' ');
        }
        out.push_str(&unescape_send_text(arg));
        separated = true;
    }
    out
}

/// Split a payload on char boundaries so each piece fits in one ctl frame.
pub fn chunk_send(text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = SEND_CHUNK_BYTES.min(rest.len());
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = rest
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(rest.len());
        }
        let (head, tail) = rest.split_at(end);
        chunks.push(head);
        rest = tail;
    }
    chunks
}

fn named_send_key(arg: &str) -> Option<String> {
    let lower = arg.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("c-m-")
        .or_else(|| lower.strip_prefix("m-c-"))
    {
        return modified_send_key(rest, true, true);
    }
    if let Some(rest) = lower.strip_prefix("c-") {
        return modified_send_key(rest, true, false);
    }
    if let Some(rest) = lower.strip_prefix("m-") {
        return modified_send_key(rest, false, true);
    }
    if let Some(rest) = arg.strip_prefix('^')
        && rest.chars().count() == 1
    {
        return modified_send_key(&rest.to_ascii_lowercase(), true, false);
    }
    Some(match lower.as_str() {
        "enter" | "return" | "cr" => "\r".to_string(),
        "lf" | "linefeed" => "\n".to_string(),
        "esc" | "escape" => "\u{1b}".to_string(),
        "tab" => "\t".to_string(),
        "space" | "sp" => " ".to_string(),
        "bspace" | "backspace" => "\u{7f}".to_string(),
        "up" => "\u{1b}[A".to_string(),
        "down" => "\u{1b}[B".to_string(),
        "right" => "\u{1b}[C".to_string(),
        "left" => "\u{1b}[D".to_string(),
        "home" => "\u{1b}[H".to_string(),
        "end" => "\u{1b}[F".to_string(),
        "pgup" | "pageup" => "\u{1b}[5~".to_string(),
        "pgdn" | "pagedown" => "\u{1b}[6~".to_string(),
        "insert" | "ic" => "\u{1b}[2~".to_string(),
        "delete" | "dc" => "\u{1b}[3~".to_string(),
        "f1" => "\u{1b}OP".to_string(),
        "f2" => "\u{1b}OQ".to_string(),
        "f3" => "\u{1b}OR".to_string(),
        "f4" => "\u{1b}OS".to_string(),
        "f5" => "\u{1b}[15~".to_string(),
        "f6" => "\u{1b}[17~".to_string(),
        "f7" => "\u{1b}[18~".to_string(),
        "f8" => "\u{1b}[19~".to_string(),
        "f9" => "\u{1b}[20~".to_string(),
        "f10" => "\u{1b}[21~".to_string(),
        "f11" => "\u{1b}[23~".to_string(),
        "f12" => "\u{1b}[24~".to_string(),
        _ => return None,
    })
}

fn modified_send_key(rest: &str, control: bool, meta: bool) -> Option<String> {
    let mut chars = rest.chars();
    let Some(character) = chars.next() else {
        return None;
    };
    if chars.next().is_some() {
        return None;
    }
    let byte = if control {
        ctrl_send_byte(character)?
    } else {
        let mut encoded = [0; 4];
        let written = character.encode_utf8(&mut encoded);
        if written.len() != 1 {
            return None;
        }
        written.as_bytes()[0]
    };
    let mut sequence = String::new();
    if meta {
        sequence.push('\u{1b}');
    }
    sequence.push(char::from(byte));
    Some(sequence)
}

fn ctrl_send_byte(character: char) -> Option<u8> {
    match character {
        'a'..='z' => Some(character as u8 - b'a' + 1),
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn unescape_send_text(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\u{1b}'),
            Some('\\') => out.push('\\'),
            Some('x') => {
                let high = chars.next();
                let low = chars.next();
                match (
                    high.and_then(|digit| digit.to_digit(16)),
                    low.and_then(|digit| digit.to_digit(16)),
                ) {
                    (Some(high), Some(low)) => {
                        out.push(char::from_u32(high * 16 + low).unwrap_or('\u{fffd}'));
                    }
                    _ => {
                        out.push('\\');
                        out.push('x');
                        if let Some(high) = high {
                            out.push(high);
                        }
                        if let Some(low) = low {
                            out.push(low);
                        }
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub fn is_nested_p2pmux(command: &[String]) -> bool {
    let Some(first) = command.first() else {
        return false;
    };
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first.as_str());
    name == "p2pmux"
}

/// Talk to a live node. Stdout is JSON; errors are sentences.
pub fn run(socket: &Path, action: CtlAction) -> Result<(), Box<dyn Error>> {
    let stream = UnixStream::connect(socket).map_err(|_| {
        CtlError(String::from(
            "could not reach the session node; is it still running?",
        ))
    })?;
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    write_json(
        &mut writer,
        &CtlFromClient::CtlHello {
            pin: CTL_PROTOCOL_PIN,
        },
    )?;
    match receive_json::<CtlToClient>(&mut reader) {
        Ok(Some(CtlToClient::CtlHelloAck { pin })) if pin == CTL_PROTOCOL_PIN => {}
        Ok(Some(CtlToClient::CtlHelloRejected { theirs, .. })) => {
            return Err(CtlError(unsupported_ctl_pin_message(theirs)).into());
        }
        Ok(Some(CtlToClient::Error { message })) => return Err(CtlError(message).into()),
        Ok(Some(_)) => {
            return Err(CtlError(String::from(
                "the session node answered ctl without a hello ack",
            ))
            .into());
        }
        Ok(None) | Err(_) => {
            return Err(CtlError(String::from(
                "this session's p2pmux is too old for ctl — upgrade the node",
            ))
            .into());
        }
    }

    let request = match &action {
        CtlAction::Machines => CtlFromClient::Machines,
        CtlAction::Agents => CtlFromClient::Agents,
        CtlAction::Spawn {
            machine,
            command,
            new,
        } => CtlFromClient::Spawn {
            machine: machine.clone(),
            command: command.clone(),
            new: *new,
        },
        CtlAction::Send { pane_id, keys } => CtlFromClient::Send {
            pane_id: *pane_id,
            keys: keys.clone(),
        },
        CtlAction::Focus { agent } => CtlFromClient::Focus {
            agent: agent.clone(),
        },
        CtlAction::Events => CtlFromClient::Events,
    };
    write_json(&mut writer, &request)?;

    if matches!(action, CtlAction::Events) {
        reader.get_mut().set_read_timeout(None)?;
        loop {
            match receive_json::<CtlToClient>(&mut reader) {
                Ok(Some(event @ CtlToClient::Event { .. })) => {
                    println!("{}", serde_json::to_string(&event)?);
                }
                Ok(Some(CtlToClient::Error { message })) => return Err(CtlError(message).into()),
                Ok(Some(_)) | Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    if matches!(action, CtlAction::Spawn { .. }) {
        reader.get_mut().set_read_timeout(Some(SPAWN_TIMEOUT))?;
    }
    match receive_json::<CtlToClient>(&mut reader)? {
        Some(CtlToClient::Machines { machines }) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({ "machines": machines }))?
            );
        }
        Some(CtlToClient::Agents { agents }) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({ "agents": agents }))?
            );
        }
        Some(CtlToClient::Spawned {
            pane_id,
            reused,
            visible_to_guests,
        }) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "pane_id": pane_id,
                    "reused": reused,
                    "visible_to_guests": visible_to_guests,
                }))?
            );
        }
        Some(CtlToClient::Sent { pane_id }) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({ "pane_id": pane_id }))?
            );
        }
        Some(CtlToClient::Focused { pane_id }) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({ "pane_id": pane_id }))?
            );
        }
        Some(CtlToClient::Error { message }) => return Err(CtlError(message).into()),
        Some(_) => return Err(CtlError(String::from("unexpected ctl reply")).into()),
        None => {
            let message = if matches!(action, CtlAction::Spawn { .. }) {
                "the machine did not answer in time"
            } else {
                "the session node closed the ctl connection"
            };
            return Err(CtlError(String::from(message)).into());
        }
    }
    Ok(())
}

pub fn write_json<T: Serialize>(stream: &mut UnixStream, message: &T) -> io::Result<()> {
    let mut frame = serde_json::to_vec(message).map_err(io::Error::other)?;
    if frame.len() + 1 > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ctl frame too large",
        ));
    }
    frame.push(b'\n');
    stream.write_all(&frame)?;
    stream.flush()
}

pub fn receive_json<T: for<'de> Deserialize<'de>>(
    reader: &mut BufReader<UnixStream>,
) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    let count = match reader.read_until(b'\n', &mut bytes) {
        Ok(count) => count,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(error)
            if error.kind() == io::ErrorKind::TimedOut
                || error.kind() == io::ErrorKind::Interrupted =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "ctl connection closed",
        ));
    }
    if count > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ctl frame too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid ctl message"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_mismatch_names_both_numbers_and_which_end_is_old() {
        let older = unsupported_ctl_pin_message(CTL_PROTOCOL_PIN - 1);
        assert!(
            older.contains(&format!("ctl pin {}", CTL_PROTOCOL_PIN - 1)),
            "{older}"
        );
        assert!(
            older.contains(&format!("speaks {CTL_PROTOCOL_PIN}")),
            "{older}"
        );
        assert!(older.contains("upgrade it, not this machine"), "{older}");

        let newer = unsupported_ctl_pin_message(CTL_PROTOCOL_PIN + 1);
        assert!(newer.contains("upgrade this one"), "{newer}");
        assert!(!newer.contains("could not reach"), "{newer}");
    }

    #[test]
    fn hello_round_trips_as_tagged_json() {
        let hello = CtlFromClient::CtlHello {
            pin: CTL_PROTOCOL_PIN,
        };
        let value = serde_json::to_value(&hello).unwrap();
        assert_eq!(value["type"], "ctl_hello");
        assert_eq!(value["pin"], CTL_PROTOCOL_PIN);
        assert_eq!(
            serde_json::from_value::<CtlFromClient>(value).unwrap(),
            hello
        );
    }

    #[test]
    fn a_tui_focus_frame_is_not_a_ctl_focus() {
        let tui = serde_json::json!({"type":"focus","tab_id":1,"pane_id":2});
        assert!(serde_json::from_value::<CtlFromClient>(tui).is_err());
    }

    #[test]
    fn nested_p2pmux_is_refused_by_basename() {
        assert!(is_nested_p2pmux(&[String::from("p2pmux")]));
        assert!(is_nested_p2pmux(&[String::from("/usr/local/bin/p2pmux")]));
        assert!(!is_nested_p2pmux(&[String::from("claude")]));
        assert!(!is_nested_p2pmux(&[]));
    }

    #[test]
    fn spawn_without_new_keeps_reusing() {
        let spawn: CtlFromClient = serde_json::from_value(serde_json::json!({
            "type": "spawn",
            "machine": "droplet",
            "command": ["claude"]
        }))
        .unwrap();
        assert_eq!(
            spawn,
            CtlFromClient::Spawn {
                machine: String::from("droplet"),
                command: vec![String::from("claude")],
                new: false,
            }
        );

        let forced = serde_json::to_value(CtlFromClient::Spawn {
            machine: String::from("droplet"),
            command: vec![String::from("claude")],
            new: true,
        })
        .unwrap();
        assert_eq!(forced["new"], true);
    }

    #[test]
    fn spawned_without_visible_to_guests_defaults_false() {
        let spawned: CtlToClient = serde_json::from_value(serde_json::json!({
            "type": "spawned",
            "pane_id": 3,
            "reused": true
        }))
        .unwrap();
        assert_eq!(
            spawned,
            CtlToClient::Spawned {
                pane_id: 3,
                reused: true,
                visible_to_guests: false,
            }
        );
    }

    #[test]
    fn ctl_pin_does_not_move_the_wire_pin() {
        assert_eq!(CTL_PROTOCOL_PIN, 1);
        assert_eq!(crate::protocol::PROTOCOL_VERSION, 12);
    }

    #[test]
    fn send_keys_become_the_bytes_a_pane_would_see() {
        let echo = interpret_send_keys(
            &[
                String::from("echo"),
                String::from("hi"),
                String::from("Enter"),
            ],
            false,
        );
        assert_eq!(echo, "echo hi\r");
        assert_eq!(
            interpret_send_keys(&[String::from("C-c")], false).as_bytes(),
            &[3]
        );
        assert_eq!(
            interpret_send_keys(&[String::from("echo C_10\\n")], false),
            "echo C_10\n"
        );
        assert_eq!(
            interpret_send_keys(&[String::from("echo"), String::from("Enter")], true),
            "echo Enter"
        );
    }

    #[test]
    fn a_long_send_splits_on_a_char_boundary() {
        let text = "é".repeat(SEND_CHUNK_BYTES);
        let chunks = chunk_send(&text);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| chunk.len() <= SEND_CHUNK_BYTES));
    }
}
