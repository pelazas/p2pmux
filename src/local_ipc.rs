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
    /// A status pushed by a producer running inside a pane — an agent hook.
    ///
    /// Unlike every other message here, the sender is not the attached client:
    /// it is a short-lived process that connects, writes this one line, and
    /// exits. It is handled like `Probe` — as a one-shot control request that
    /// never touches the single interactive attachment slot.
    ///
    /// `pane_id` is a claim, not an authority. The node accepts it only for a
    /// pane it hosts itself, which is what confines a producer to the machine
    /// it runs on; the peer-facing half of that check already lives in
    /// `Coordinator::accept_agent_roster`.
    AgentStatus {
        pane_id: u64,
        kind: String,
        status: String,
        #[serde(default)]
        cwd: String,
        /// What the agent is doing, in its own words.
        ///
        /// Stops at the node that owns the pane: the roster published to peers
        /// has no field for it. `#[serde(default)]` so a producer from an older
        /// build, which sends no message at all, still reports its status.
        #[serde(default)]
        message: String,
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
        #[serde(default)]
        presence: Vec<PresenceRow>,
        local_peer_id: Vec<u8>,
        tab_id: u64,
        pane_id: u64,
        /// The coordinator's printable join ticket. Members receive `None` and the share
        /// modal says so rather than offering an invite the client cannot make.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<String>,
        /// The short code the ticket was published under, when the rendezvous accepted it.
        /// `None` on a member, and also on a coordinator that could not reach the service.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
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
    /// Where the other members are looking. Only the node holds the session's control
    /// stream, so an attached client cannot learn this any other way.
    Presence {
        presence: Vec<PresenceRow>,
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
    /// The agent's own words, for panes this machine hosts.
    ///
    /// This channel is a Unix socket to the local client, so it carries what the
    /// peer-facing `AgentRosterEntry` deliberately will not. Empty for a pane
    /// hosted by another member: their node never sent it, and never should.
    #[serde(default)]
    pub message: String,
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
            message: row.message.clone(),
        }
    }
}

/// Where one other member is looking, ready to draw.
///
/// Only attached members appear, so the renderer never has to reason about what a
/// detached member's location means -- they are simply absent. Colors and labels are
/// derived from the layout's member list that the client already holds.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PresenceRow {
    pub peer_id: Vec<u8>,
    pub tab_id: u64,
    pub pane_id: u64,
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
    fn snapshot_ticket_serde_is_optional_and_backwards_compatible() {
        let snapshot = |ticket: Option<String>| NodeMessage::Snapshot {
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
            presence: vec![],
            local_peer_id: vec![],
            tab_id: 1,
            pane_id: 1,
            code: ticket.as_ref().map(|_| "4KP7Q-M2XRW".to_owned()),
            ticket,
        };

        let with_ticket = serde_json::to_value(snapshot(Some("p2pmux-v3:TEST".into()))).unwrap();
        assert_eq!(with_ticket["ticket"], "p2pmux-v3:TEST");

        let without_ticket = serde_json::to_value(snapshot(None)).unwrap();
        assert!(without_ticket.get("ticket").is_none());

        let parsed: NodeMessage = serde_json::from_value(without_ticket).unwrap();
        let NodeMessage::Snapshot { ticket, .. } = parsed else {
            panic!("expected snapshot");
        };
        assert_eq!(ticket, None);
    }
}
