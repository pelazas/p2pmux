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
    Hello {
        cols: u16,
        rows: u16,
    },
    Input {
        bytes: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        perf_id: Option<u64>,
    },
    StructuralIntent {
        intent: UiIntent,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    ResyncScreen {
        pane_id: u64,
    },
    ScrollbackQuery {
        pane_id: u64,
        /// Omitted when starting a new frozen local-history browsing session.
        history_id: Option<u64>,
        offset: u64,
        request_id: u64,
    },
    Focus {
        tab_id: u64,
        pane_id: u64,
    },
    Detach {
        generation: u64,
    },
    Rename {
        name: String,
    },
    Shutdown {
        generation: u64,
    },
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
        local_peer_id: Vec<u8>,
        tab_id: u64,
        pane_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        join_code: Option<String>,
    },
    Screens {
        screens: Vec<PaneScreenSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        perf_id: Option<u64>,
    },
    /// A frozen, render-ready host viewport.  This deliberately uses the normal screen codec:
    /// history is never replayed through a second client-side VT parser.
    ScrollbackWindow {
        pane_id: u64,
        request_id: u64,
        history_id: u64,
        total_rows: u64,
        offset: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<Vec<u8>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unavailable: Option<String>,
    },
    Layout {
        layout: Box<LayoutSnapshot>,
    },
    Leases {
        leases: Vec<PaneLeaseSnapshot>,
    },
    Rosters {
        rosters: Vec<AgentOverlaySnapshotRow>,
    },
    /// Operator-facing runtime status, e.g. a lost coordinator or a pane that is retrying.
    /// The node owns the session, so this is the only way an attached client can learn
    /// that something went wrong.
    Status {
        message: String,
    },
    /// Which network path each connected peer is on, and its round-trip time.
    ///
    /// Only the node can observe this -- it owns the Iroh transport -- and only the
    /// client draws, so a session that silently fell back to a relay is invisible
    /// without this message.
    Paths {
        paths: Vec<crate::transport::PeerPath>,
    },
    /// Whether the coordinator is refusing new peers. Every attached client draws it, but
    /// only the coordinator's node can know it, so it has to be published rather than
    /// derived from the layout.
    SessionLock {
        locked: bool,
    },
    Focus {
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PaneScreenSnapshot {
    pub pane_id: u64,
    pub state: ScreenUpdate,
    /// Local-host history metadata. Remote panes publish zeroes.
    pub history_len: u64,
    pub history_end: u64,
}

/// The screen state carried inside every local attachment snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenUpdate {
    Snapshot {
        sequence: u64,
        snapshot: Vec<u8>,
        kitty_keyboard_active: bool,
    },
    Delta {
        base_sequence: u64,
        sequence: u64,
        delta: Vec<u8>,
        kitty_keyboard_active: bool,
    },
    Unchanged {
        sequence: u64,
        kitty_keyboard_active: bool,
    },
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

    #[test]
    fn snapshot_join_code_serde_is_optional_and_backwards_compatible() {
        let snapshot = |join_code| NodeMessage::Snapshot {
            room_name: "test".into(),
            role: "coordinator".into(),
            summary: SessionSummary::default(),
            layout: Box::new(crate::layout::LayoutSnapshot {
                revision: 0,
                members: vec![],
                tabs: vec![],
                panes: Default::default(),
            }),
            screens: vec![],
            leases: vec![],
            rosters: vec![],
            local_peer_id: vec![],
            tab_id: 1,
            pane_id: 1,
            join_code,
        };

        let with_join_code = serde_json::to_value(snapshot(Some("TESTCODE".into()))).unwrap();
        assert_eq!(with_join_code["join_code"], "TESTCODE");

        let without_join_code = serde_json::to_value(snapshot(None)).unwrap();
        assert!(without_join_code.get("join_code").is_none());

        let parsed: NodeMessage = serde_json::from_value(without_join_code).unwrap();
        let NodeMessage::Snapshot { join_code, .. } = parsed else {
            panic!("expected snapshot");
        };
        assert_eq!(join_code, None);
    }
}
