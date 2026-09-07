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
        CtlAction::Spawn { machine, command } => CtlFromClient::Spawn {
            machine: machine.clone(),
            command: command.clone(),
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
            match receive_json::<CtlToClient>(&mut reader)? {
                Some(event @ CtlToClient::Event { .. }) => {
                    println!("{}", serde_json::to_string(&event)?);
                }
                Some(CtlToClient::Error { message }) => return Err(CtlError(message).into()),
                Some(_) => {}
                None => return Ok(()),
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
        Some(CtlToClient::Spawned { pane_id, reused }) => {
            println!(
                "{}",
                serde_json::to_string(
                    &serde_json::json!({ "pane_id": pane_id, "reused": reused })
                )?
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
        return Ok(None);
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
    fn ctl_pin_does_not_move_the_wire_pin() {
        assert_eq!(CTL_PROTOCOL_PIN, 1);
        assert_eq!(crate::protocol::PROTOCOL_VERSION, 12);
    }
}
