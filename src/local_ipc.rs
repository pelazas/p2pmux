//! Local node/client protocol. This is deliberately separate from the peer Iroh protocol.

use std::{
    io,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

use crate::{layout::LayoutSnapshot, tui::UiIntent};

const MAX_FRAME: usize = 1024 * 1024;
pub const OUTBOUND_QUEUE: usize = 64;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Probe,
    Hello { cols: u16, rows: u16 },
    Input { bytes: Vec<u8> },
    StructuralIntent { intent: UiIntent },
    Resize { cols: u16, rows: u16 },
    Focus { tab_id: u64, pane_id: u64 },
    Detach { generation: u64 },
    Rename { name: String },
    Shutdown { generation: u64 },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeMessage {
    ProbeAck,
    AttachAccepted {
        generation: u64,
    },
    AttachRejected {
        reason: String,
    },
    Snapshot {
        room_name: String,
        role: String,
        summary: SessionSummary,
        layout: Box<LayoutSnapshot>,
        screens: Vec<PaneScreenSnapshot>,
        leases: Vec<PaneLeaseSnapshot>,
        rosters: Vec<AgentOverlaySnapshotRow>,
        tab_id: u64,
        pane_id: u64,
    },
    Update {
        state: serde_json::Value,
    },
    DetachAck {
        generation: u64,
    },
    ShutdownAck {
        generation: u64,
    },
    Error {
        message: String,
    },
}

/// Render-ready agent data included atomically with an attachment snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentOverlaySnapshotRow {
    pub pane_id: u64,
    pub kind: String,
    pub cwd: String,
    pub state: i32,
    pub working_since_unix_ms: u64,
    pub host: String,
    pub controller: String,
}

impl From<&crate::tui::AgentOverlayRow> for AgentOverlaySnapshotRow {
    fn from(row: &crate::tui::AgentOverlayRow) -> Self {
        Self {
            pane_id: row.pane_id,
            kind: row.kind.clone(),
            cwd: row.cwd.clone(),
            state: row.state as i32,
            working_since_unix_ms: row.working_since_unix_ms,
            host: row.host.clone(),
            controller: row.controller.clone(),
        }
    }
}

/// Complete screen state for one pane. Attachments replace their local `GuestScreen` from this
/// payload, so reconnects do not depend on a delta history.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PaneScreenSnapshot {
    pub pane_id: u64,
    pub sequence: u64,
    pub snapshot: Vec<u8>,
    pub kitty_keyboard_active: bool,
}

/// The subset of a pane lease needed by render chrome; lease authority stays in the node.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PaneLeaseSnapshot {
    pub pane_id: u64,
    pub ready: bool,
    pub controller_peer_id: Option<Vec<u8>>,
    pub controller_active: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SessionSummary {
    pub tabs: u32,
    pub panes: u32,
    pub hosts: u32,
    pub coordinator_name: String,
}

/// Single-client gate. A generation makes delayed disconnects harmless after a reattach.
#[derive(Clone, Default)]
pub struct AttachmentGate(Arc<Mutex<AttachmentState>>);
#[derive(Default)]
struct AttachmentState {
    generation: u64,
    attached: bool,
}

impl AttachmentGate {
    pub fn attach(&self) -> Result<u64, &'static str> {
        let mut state = self.0.lock().expect("attachment gate poisoned");
        if state.attached {
            return Err("already attached");
        }
        state.generation = state.generation.saturating_add(1).max(1);
        state.attached = true;
        Ok(state.generation)
    }
    pub fn detach(&self, generation: u64) -> bool {
        let mut state = self.0.lock().expect("attachment gate poisoned");
        if state.attached && state.generation == generation {
            state.attached = false;
            true
        } else {
            false
        }
    }
}

pub async fn send<T: Serialize>(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    message: &T,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await
}

pub async fn receive<T: DeserializeOwned>(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    let count = reader.read_until(b'\n', &mut bytes).await?;
    if count == 0 {
        return Ok(None);
    }
    if count > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local IPC frame too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid local IPC message"))
}

pub fn bounded_outbound() -> (mpsc::Sender<NodeMessage>, mpsc::Receiver<NodeMessage>) {
    mpsc::channel(OUTBOUND_QUEUE)
}

pub async fn split(
    stream: UnixStream,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let (read, write) = stream.into_split();
    (BufReader::new(read), write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        net::UnixListener,
        time::{Duration, timeout},
    };

    #[tokio::test]
    async fn messages_round_trip_and_gate_refuses_second_client() {
        let path = std::env::temp_dir().join(format!("p2pmux-ipc-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let gate = AttachmentGate::default();
        let gate_for_task = gate.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = split(stream).await;
            assert_eq!(
                receive::<ClientMessage>(&mut reader).await.unwrap(),
                Some(ClientMessage::Hello { cols: 80, rows: 24 })
            );
            send(
                &mut writer,
                &NodeMessage::AttachAccepted {
                    generation: gate_for_task.attach().unwrap(),
                },
            )
            .await
            .unwrap();
        });
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut reader, mut writer) = split(stream).await;
        send(&mut writer, &ClientMessage::Hello { cols: 80, rows: 24 })
            .await
            .unwrap();
        assert_eq!(
            receive::<NodeMessage>(&mut reader).await.unwrap(),
            Some(NodeMessage::AttachAccepted { generation: 1 })
        );
        timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gate.attach(), Err("already attached"));
        assert!(gate.detach(1));
        assert!(gate.attach().is_ok());
        let _ = std::fs::remove_file(path);
    }
}
