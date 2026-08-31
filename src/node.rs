//! Headless owner of a shared layout.
//!
//! `SharedLayoutRuntime` remains the temporary foreground adapter during the split.  The node
//! owns it without terminal I/O and exposes only node operations; the local socket client is the
//! only component allowed to render a terminal.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::{
        fs::OpenOptionsExt,
        net::{UnixListener, UnixStream},
    },
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    hosted_rendezvous::PublishedCode,
    local_ipc::{
        AgentOverlaySnapshotRow, AttachmentGate, ClientMessage, NodeMessage, PaneLeaseSnapshot,
        PaneScreenSnapshot, PresenceRow, ScreenUpdate, SessionSummary,
    },
    session::{
        HostSession, LayoutControlEvent, SharedLayoutHost,
        join_layout_with_display_name_and_timeout, layout_snapshot_from_state,
    },
    session_store::{SessionDescriptor, SessionRole, SessionStore},
    ticket::JoinTicket,
    transport::Transport,
    tui::SharedLocalPane,
};

use crate::tui::SharedLayoutRuntime;

const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_millis(5);
const ATTACHED_IDLE_BACKOFF: Duration = Duration::from_millis(1);
const DETACHED_IDLE_BACKOFF: Duration = Duration::from_millis(16);
// A node with no local client still drains for remote guests, so it cannot simply sleep.
// It can slow down once nothing has happened for a while: the cost is that the wake-up
// after a quiet spell (a guest's first keystroke, or a local attach) is noticed up to
// `DETACHED_QUIET_BACKOFF` late, after which `did_work` puts the loop back to full rate.
const DETACHED_QUIET_BACKOFF: Duration = Duration::from_millis(100);
const DETACHED_QUIET_AFTER: Duration = Duration::from_secs(2);
const SCREEN_PUBLISH_INTERVAL: Duration = Duration::from_millis(16);
const TARGET_SCREEN_PUBLISH_INTERVAL: Duration = Duration::from_millis(8);
const TARGET_SCREEN_URGENCY_TTL: Duration = Duration::from_millis(64);
// The client socket read blocks for a millisecond at most, so the loop turns about a
// thousand times a second while a pane is producing output. Every turn used to call
// `node.drain()`, which parses and diffs each pane's screen, but `screens_due` only lets a
// frame reach the client once per `TARGET_SCREEN_PUBLISH_INTERVAL`. The extra drains built
// frames that were immediately superseded, so pace the periodic drain to the publish
// cadence. Input still drains immediately, which is what keystroke echo depends on.
const PERIODIC_DRAIN_INTERVAL: Duration = TARGET_SCREEN_PUBLISH_INTERVAL;
// How often the node re-reads which machines are in the session and how many
// agents each is running, for `p2pmux machines` and the pairing record. Both
// are answers a human reads at human speed, and the sampler that produces the
// agent counts only runs every five seconds anyway.
const PEER_SCAN_INTERVAL: Duration = Duration::from_secs(2);
// How often a node looks at itself: whether the process that started it is still
// there, and how much memory it is holding. Matches the fleet agent's own
// supervision tick -- there is nothing to be gained by noticing sooner, and a
// process-table lookup on a timer is exactly the kind of background cost this
// whole change exists to keep small.
const SELF_CHECK_INTERVAL: Duration = Duration::from_secs(15);
// The most memory a node the fleet agent started may hold before it stops
// itself.
//
// Deliberately *below* the `MemoryMax` on the Linux unit, so that on Linux the
// node notices first and says why, and the kernel's cgroup killer is only ever
// the backstop. On macOS there are no cgroups and this is the only ceiling there
// is -- which is the entire reason it lives in the process rather than in the
// unit file.
//
// Generous against what the work costs: scrollback is capped at 10,000 lines a
// pane, so even a node hosting a dozen busy panes lands an order of magnitude
// under this. The nodes that made this necessary were holding 598MB while
// hosting nothing at all.
const TETHERED_MEMORY_CEILING: u64 = 384 * 1024 * 1024;
// Where any node, however it was started, says out loud how big it has become.
//
// Only tethered nodes are stopped at a ceiling. A session somebody is working in
// is worth more than the memory it is holding, and killing it to save a machine
// that is not actually short of memory would be the tool deciding it knows
// better. Saying so is not.
const MEMORY_WARN_AT: u64 = 256 * 1024 * 1024;
// The node has to notice before the cgroup does, or it never gets to say why it
// stopped and the kernel's killer becomes the whole explanation. Checked at
// compile time, because the two numbers live in different files and the one that
// would catch this at runtime is a log nobody reads.
const _: () =
    assert!(TETHERED_MEMORY_CEILING < (crate::daemon::MEMORY_MAX_MB as u64) * 1024 * 1024);
const _: () = assert!(MEMORY_WARN_AT < TETHERED_MEMORY_CEILING);
const MAX_FROZEN_SCROLLBACK_SESSIONS: usize = 8;

/// Why a scrollback query came back with nothing, in the words the footer will
/// show — one per cause, because the person reading it is looking at exactly
/// one pane and only one of these is true of it.
///
/// There is deliberately no string for a pane that simply has no history yet.
/// Nothing has scrolled off a shell that has just started, the wheel has
/// nowhere to go, and an error about remote panes and expired sessions is three
/// guesses at a pane the node can see perfectly well.
const SCROLLBACK_EXPIRED: &str = "that scrollback expired — scroll again";
const SCROLLBACK_ALTERNATE_SCREEN: &str = "a full-screen program owns this pane";
const SCROLLBACK_NOT_OURS: &str = "that pane's history is on another machine";

/// The longest a footer notice may be and still leave room for the keys.
///
/// The footer places a notice first and fits the keybinding hints into the
/// columns it leaves, so a notice is not free: past a length it takes the whole
/// bar. Eighty columns, less the seventeen the narrowest tier needs — `Ctrl+
/// <p> <t> <q>`, the chords without which nothing else in the mux is reachable
/// — less the two spaces between them.
///
/// The string these replaced was ninety characters and did exactly that, on a
/// ninety-nine-column terminal, to report that a one-second-old shell had no
/// scrollback.
const MAX_FOOTER_NOTICE_CHARS: usize = 80 - 17 - 2;
// Bytes rather than characters, because `chars().count()` is not const. It is
// the conservative direction: a multi-byte dash counts for three.
const _: () = assert!(SCROLLBACK_EXPIRED.len() <= MAX_FOOTER_NOTICE_CHARS);
const _: () = assert!(SCROLLBACK_ALTERNATE_SCREEN.len() <= MAX_FOOTER_NOTICE_CHARS);
const _: () = assert!(SCROLLBACK_NOT_OURS.len() <= MAX_FOOTER_NOTICE_CHARS);
const OUTBOUND_QUEUE: usize = 64;

fn selection_copy_reply(request_id: u64, selection: Result<Option<String>, ()>) -> NodeMessage {
    let (text, unavailable) = match selection {
        Ok(Some(text)) if crate::local_ipc::selection_copy_fits_frame(request_id, &text) => {
            (Some(text), None)
        }
        Ok(Some(_)) => (None, Some("selection too large to copy".into())),
        Ok(None) => (None, None),
        Err(()) => (None, Some(SCROLLBACK_NOT_OURS.into())),
    };
    NodeMessage::SelectionCopy {
        request_id,
        text,
        unavailable,
    }
}

pub struct SharedLayoutNode {
    runtime: SharedLayoutRuntime,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum NodeBootstrapKind {
    Create {
        display_name: String,
        cols: u16,
        rows: u16,
    },
    Join {
        ticket: String,
        display_name: String,
        cols: u16,
        rows: u16,
        /// Local launch policy only. Old launchers omit it and keep the normal
        /// thirty-second join window.
        #[serde(default)]
        connect_timeout_ms: Option<u64>,
    },
}

/// The lifetime a launched node has relative to the process that started it.
///
/// The fleet agent's node is the machine's presence in
/// the fleet, rebuilt within a tick of the agent coming back, and nobody is
/// sitting in front of it. A node that survives its agent is not a rescued
/// session — it is a process nothing is watching, which is precisely what nine
/// of them at PPID 1 were on 2026-08-16, after the out-of-memory killer took the
/// agent and left its children running for another twelve hours.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Tether {
    /// Outlives its launcher. The session belongs to whoever asked for it.
    #[default]
    Detached,
    /// Stops when its launcher does, however the launcher goes.
    ToLauncher,
    /// Stops if its launching interactive client goes away before the first
    /// successful attachment. Afterwards the client protocol owns its lifetime.
    UntilFirstAttach,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeBootstrap {
    pub descriptor: SessionDescriptor,
    pub kind: NodeBootstrapKind,
    /// A supervisor alone cannot distinguish a persistent fleet node from an
    /// interactive launch that is only waiting for its first client.
    #[serde(default)]
    pub tether: Tether,
    /// The process this node is tethered to, if it is tethered to one.
    ///
    /// Checked by the node rather than enforced by the launcher, because the
    /// mechanisms a launcher has are all worse. `PR_SET_PDEATHSIG` is Linux-only
    /// and fires on the death of the parent *thread*, which under a Tokio
    /// runtime is not a thing anybody controls; killing from a `Drop` cannot run
    /// when the launcher is the one being killed, which is the whole scenario.
    /// A node asking "is the process that started me still there" works on both
    /// platforms, survives its parent being SIGKILLed, and needs nothing unsafe.
    ///
    /// Missing in a bootstrap written by an older p2pmux, which is what
    /// `Option` and `#[serde(default)]` are for: a node from before this existed
    /// is simply untethered.
    #[serde(default)]
    pub supervisor: Option<Supervisor>,
}

/// The process a tethered node watches, and enough about it to be sure.
///
/// Pids are reused — quickly, on a machine building software — so a bare pid
/// would eventually name something else entirely and the node would either
/// outlive its agent anyway or shut down under a healthy one. The start time
/// pins it: the pair is unique for as long as the operating system is up.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Supervisor {
    pub pid: u32,
    pub started_at: u64,
}

impl Supervisor {
    /// This process, for a node it is about to launch on a tether.
    pub fn current() -> Option<Self> {
        let pid = std::process::id();
        Some(Self {
            pid,
            started_at: crate::agent_detect::process_start_time(pid)?,
        })
    }

    /// Whether the process this node is tethered to is still the one that
    /// started it.
    pub fn is_alive(&self) -> bool {
        crate::agent_detect::process_start_time(self.pid) == Some(self.started_at)
    }
}

/// Join a session one of your machines invited you to, in a node of its own.
///
/// A node hosts one session, so following an invitation means starting another
/// node rather than teaching this one to be in two places. That is also what
/// `p2pmux join` does, which is the point: an invited machine ends up in
/// exactly the state it would have been in had somebody typed the code on it.
///
/// Returns whether anything was started. Already being in that session is the
/// ordinary case, not a failure: invitations are re-announced on a timer so
/// that a machine which was asleep still hears about a session started while
/// it was, and every announcement after the first is a no-op.
pub fn follow_fleet_invite(ticket: &str, tether: Tether) -> Result<bool, Box<dyn Error>> {
    let parsed = ticket
        .parse::<crate::ticket::JoinTicket>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid invitation ticket"))?;
    let ticket = parsed.to_string();
    let store = crate::session_store::SessionStore::for_current_user()?;
    // Both sides of the same question: the session this machine coordinates,
    // and any it has already been invited into. Without the first, a machine
    // would follow its own invitation back into the session it just started.
    //
    // Read without probing. This runs on the node's own loop, every time a
    // machine of yours re-announces a session — which is every couple of
    // seconds, for as long as both are up — and `list_live` would spend a
    // quarter of a second there waiting for this very node to answer a probe it
    // cannot answer while it is blocked sending it.
    //
    // A record is only evidence of membership while the node that wrote it is
    // still running. It outlives one: a machine that was rebooted, or whose
    // p2pmux was killed, keeps the file. Believing it meant a machine that had
    // been away decided it was already in its home session, never rejoined, and
    // so never heard any of the invitations that travel over that session --
    // including the one to the session started while it was away. "Your trusted
    // machines join immediately" quietly became "unless one of them has been
    // switched off since", which is the case it exists for.
    //
    // The liveness test is `node_process_is_alive`, which reads the process
    // table. Still not a socket probe, so the reasoning above about not
    // blocking this loop holds.
    if store.list_recorded()?.iter().any(|session| {
        let names_it = session.ticket.as_deref() == Some(ticket.as_str())
            || session.joined_ticket.as_deref() == Some(ticket.as_str());
        names_it && crate::agent_detect::node_process_is_alive(session.node_pid)
    }) {
        return Ok(false);
    }
    let display_name = crate::cli::display_name_or_hostname()?;
    crate::cli::launch_background_node(
        NodeBootstrapKind::Join {
            ticket,
            display_name,
            // No terminal is attached to a machine following an invitation, so
            // the grid is the one a client will reflow the moment somebody
            // attaches. Guessing small would make the first frame a resize.
            cols: 80,
            rows: 24,
            connect_timeout_ms: None,
        },
        crate::session_store::generate_name()?,
        crate::session_store::SessionRole::Member,
        tether,
        // An invitation from one of your own machines is the fleet saying where
        // it moved to, so following it lands in the fleet's home session.
        crate::cli::FleetRole::Home {
            stands_in_for: None,
        },
    )?;
    Ok(true)
}

pub fn write_bootstrap(path: &std::path::Path, bootstrap: &NodeBootstrap) -> io::Result<()> {
    let bytes = serde_json::to_vec(bootstrap).map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

pub fn read_bootstrap(path: &std::path::Path) -> io::Result<NodeBootstrap> {
    let bootstrap = serde_json::from_slice(&fs::read(path)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid node bootstrap"))?;
    let _ = fs::remove_file(path);
    Ok(bootstrap)
}

/// Say what a failed join means, rather than which library reported it.
///
/// This message is what someone sees after pasting an invite that does not work, so it
/// has to name the likely causes rather than the transport. Only a failure to *reach*
/// the coordinator is reworded: a session that answered and then refused -- full,
/// locked, a bad ticket -- already says so, and burying that under "could not reach"
/// would be a worse message, not a better one.
fn describe_join_failure(error: crate::session::SessionError) -> Box<dyn Error> {
    use crate::session::SessionError;
    use crate::{protocol::ProtocolError, transport::TransportError};
    match error {
        // A peer that answered in a protocol this one does not speak is not a
        // peer that could not be reached, and saying so sent somebody looking at
        // their network for an hour. This is the one transport failure that has
        // a diagnosis: the two machines are on different versions of p2pmux, and
        // the older one is the one to upgrade.
        SessionError::Transport(TransportError::Protocol(ProtocolError::UnsupportedVersion(
            theirs,
        ))) => io::Error::other(unsupported_protocol_message(theirs)).into(),
        SessionError::Transport(_) | SessionError::TimedOut(_) => io::Error::other(
            "could not reach the session host: they may be offline or on a different network, \
             or the invite may be out of date. Ask for a fresh join code.",
        )
        .into(),
        other => Box::<dyn Error>::from(other.to_string()),
    }
}

/// What to tell somebody whose p2pmux and the session's do not speak the same
/// protocol, in the terms they can act on: which end is old, and how to fix it.
///
/// The version numbers are what the wire carries, and nobody installs a protocol
/// number — so they are named as the evidence and the instruction is about
/// p2pmux itself. Which end to upgrade follows from which number is lower.
pub(crate) fn unsupported_protocol_message(theirs: u32) -> String {
    let ours = crate::protocol::PROTOCOL_VERSION;
    let older = if theirs < ours {
        "The machine hosting that session is running an older p2pmux than this one — \
         upgrade it, not this machine."
    } else {
        "This machine is running an older p2pmux than the session's — upgrade this one."
    };
    format!(
        "that session speaks p2pmux protocol {theirs} and this p2pmux speaks {ours}, \
         so they cannot share a session. {older} \
         Upgrade with `curl -fsSL https://p2pmux.com/install.sh | sh`, or \
         `brew upgrade p2pmux` if you installed it that way, then try again."
    )
}

/// Private child entrypoint. It owns the descriptor, socket and every PTY.
pub async fn run_background(bootstrap: NodeBootstrap) -> Result<(), Box<dyn Error>> {
    let mut descriptor = bootstrap.descriptor.clone();
    descriptor.node_pid = std::process::id();
    // Before the first pane spawns: every PTY this node opens inherits the
    // socket path from here, and pane 1 is created a few lines below.
    crate::pty_host::set_agent_socket_path(descriptor.socket_path.clone());
    // Before anything joins or hosts, because this is what the member list will
    // carry. A box that has been through `p2pmux pair` belongs to a fleet and
    // says so; one that has not says nothing. It is not a claim about *whose*
    // fleet — every peer answers that against its own pairing record — so a
    // machine of someone else's saying it here wins them nothing.
    crate::session::set_local_member_kind(if crate::pairing::load_or_empty().can_rejoin() {
        crate::layout::MemberKind::Machine
    } else {
        // Not `Person`: this box may well be paired a moment from now, by the
        // `p2pmux pair` that is about to print a code against the session this
        // node is starting. Claiming to be a person would be a claim that
        // outlives the moment it was true, and would cost this machine its
        // place in its own fleet.
        crate::layout::MemberKind::Unspecified
    });
    let (mut node, published_code) = match bootstrap.kind {
        NodeBootstrapKind::Create {
            display_name,
            cols,
            rows,
        } => {
            let (shell_rows, shell_cols) = crate::tui::initial_root_pane_grid(cols, rows);
            let host = SharedLayoutHost::with_display_name(
                HostSession::create_with_session_name(descriptor.name.clone()).await?,
                display_name,
                shell_rows,
                shell_cols,
            )?;
            let host_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
            let initial = SharedLocalPane::spawn(1, shell_rows, shell_cols, host_peer_id.clone())?;
            let panes = host.pane_server();
            panes.register_local_pane(
                crate::protocol::PaneDescriptor {
                    pane_id: 1,
                    host_peer_id,
                    grid_rows: u32::from(shell_rows),
                    grid_cols: u32::from(shell_cols),
                    title: None,
                    locked: false,
                    exited: false,
                },
                initial.channels(),
            )?;
            let snapshot = host.session_snapshot()?;
            let layout =
                layout_snapshot_from_state(snapshot.state.as_ref().ok_or("missing host layout")?)
                    .map_err(|error| io::Error::other(format!("invalid host layout: {error:?}")))?;
            let dispatcher = host.incoming_dispatcher(panes.clone())?;
            let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });
            // The descriptor is the only out-of-process copy, so `p2pmux ticket <name>` and
            // the attaching client both read it from there rather than minting their own.
            let ticket = host.ticket().to_string();
            descriptor.ticket = Some(ticket.clone());
            // The short code is a convenience layered on the ticket, so a rendezvous outage
            // degrades the invite rather than failing the session: the ticket still works,
            // and the share panel says there is no code instead of showing a dead one.
            let published_code = PublishedCode::publish(ticket.clone()).await.ok();
            let code = published_code
                .as_ref()
                .map(|published| published.code().printable());
            descriptor.join_code = code.clone();
            let session_id = host.ticket().session_id().to_vec();
            let handle = tokio::runtime::Handle::current();
            let mut runtime = crate::tui::SharedLayoutRuntime::host(
                host, panes, layout, initial, ticket, code, handle,
            )?;
            runtime.set_session_id(session_id);
            // The runtime owns the accept loop from here: losing every member is one of the
            // shapes a coordinator's own failover takes, and stepping down means this
            // endpoint has to stop answering joins and start behaving like a member.
            runtime.set_accept_task(dispatcher_task);
            (SharedLayoutNode::new(runtime), published_code)
        }
        NodeBootstrapKind::Join {
            ticket,
            display_name,
            cols: _,
            rows: _,
            connect_timeout_ms,
        } => {
            let ticket = ticket
                .parse::<JoinTicket>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid ticket"))?;
            let transport = Transport::bind().await?;
            let join_timeout = Duration::from_millis(connect_timeout_ms.unwrap_or(30_000));
            // The first snapshot is the join becoming usable. Keeping handshake and that
            // snapshot inside this deadline prevents a five-second dial plus another
            // five-second wait when a sleeping paired machine does answer late.
            let (member, state) = tokio::time::timeout(join_timeout, async {
                let mut member = join_layout_with_display_name_and_timeout(
                    transport,
                    ticket.clone(),
                    display_name,
                    join_timeout,
                )
                .await
                .map_err(describe_join_failure)?;
                let state = match member.events.recv().await {
                    Some(LayoutControlEvent::Snapshot(snapshot)) => {
                        snapshot.state.ok_or("missing layout snapshot")?
                    }
                    _ => {
                        return Err(io::Error::other(
                            "layout coordinator disconnected during join",
                        )
                        .into());
                    }
                };
                Ok::<_, Box<dyn Error>>((member, state))
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "join timed out"))??;
            // Recorded, not probed, for the same reason naming a session never
            // probes: this node is mid-join and its own record is already on
            // disk, so `list_live` would spend the ack timeout waiting for a
            // socket this very thread is the one that would answer on.
            let live_names = SessionStore::for_current_user()?
                .list_recorded()?
                .into_iter()
                .map(|session| session.name)
                .collect();
            if let Some(name) =
                crate::session_store::unique_local_name(&member.session_name, &live_names)
            {
                descriptor.name = name;
            }
            let panes = member.pane_server(ticket.session_id().to_vec())?;
            panes.replace_roster_from_layout(&state)?;
            let acceptor = panes.clone();
            let dispatcher_task = tokio::spawn(async move { acceptor.accept_loop().await });
            let mut runtime = crate::tui::SharedLayoutRuntime::member_from_state(
                member,
                panes,
                ticket.session_id().to_vec(),
                state,
                tokio::runtime::Handle::current(),
            )?;
            // Handed over for the same reason as on the coordinator, in the other direction:
            // a member that gets promoted has to start answering joins on this endpoint.
            runtime.set_accept_task(dispatcher_task);
            (SharedLayoutNode::new(runtime), None)
        }
    };
    let store = SessionStore::for_current_user()?;
    let _ = fs::remove_file(&descriptor.socket_path);
    let listener = UnixListener::bind(&descriptor.socket_path)?;
    listener.set_nonblocking(true)?;
    store.write(&descriptor)?;
    let result = run_socket_loop(
        &mut node,
        listener,
        &mut descriptor,
        &store,
        bootstrap.supervisor,
        bootstrap.tether,
    );
    // `SharedLayoutRuntime` owns a Tokio handle for its asynchronous pane/control cleanup.
    // The node itself runs on this runtime, so perform that blocking teardown outside its worker.
    tokio::task::block_in_place(|| node.shutdown());
    if let Some(published) = published_code {
        published.retire().await;
    }
    let _ = fs::remove_file(&descriptor.socket_path);
    let _ = store.remove(&descriptor.id);
    result.map_err(Into::into)
}

/// Apply this node's timeouts to a freshly accepted connection.
///
/// `None` means the connection is not worth keeping, never that the node is in trouble.
/// That distinction is the whole point: `SessionStore::sweep_dead_sockets` probes liveness by
/// opening a connection and dropping it at once, and on macOS these calls answer EINVAL for a
/// peer that is already gone. Propagating that ended the socket loop and tore down the
/// session, so any `p2pmux attach`, `ls`, `ticket` or `--resume` — every caller of
/// `list_live` — killed every live session on the machine.
fn configure_accepted(stream: UnixStream) -> Option<UnixStream> {
    if stream.set_nonblocking(false).is_err() {
        return None;
    }
    // A producer writes one line and exits — `p2pmux notify` is a hook on the
    // agent's critical path and has nothing to wait around for — so by the time
    // the node accepts, its peer is usually already gone. macOS answers EINVAL
    // to `setsockopt` on a socket in that state, and treating that as "not worth
    // keeping" threw away the payload sitting readable in the socket's own
    // buffer. Every agent status pushed by a hook was lost this way, which is
    // why wiring the hooks up appeared to do nothing at all.
    //
    // A peer that has gone cannot make us wait: the read returns what was
    // buffered and then EOF. Switching to non-blocking makes that a guarantee
    // rather than an assumption — the timeouts exist to stop a *live* client
    // wedging the loop, and there is no live client here to wedge it.
    if stream
        .set_read_timeout(Some(FIRST_MESSAGE_TIMEOUT))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        stream.set_nonblocking(true).ok()?;
    }
    Some(stream)
}

fn run_socket_loop(
    node: &mut SharedLayoutNode,
    listener: UnixListener,
    descriptor: &mut SessionDescriptor,
    store: &SessionStore,
    supervisor: Option<Supervisor>,
    mut tether: Tether,
) -> io::Result<()> {
    let gate = AttachmentGate::default();
    let mut client: Option<AttachedClient> = None;
    let mut frozen_scrollback = BTreeMap::<u64, FrozenScrollback>::new();
    let mut next_history_id = 1_u64;
    let mut last_periodic_drain: Option<Instant> = None;
    // The member names this node has already written to the pairing record.
    // Compared rather than rewritten, so a membership that has not changed
    // costs one vector compare per drain and no file write at all.
    let mut last_known_peers: Vec<crate::session_store::SessionPeer> = Vec::new();
    let mut last_peer_scan: Option<Instant> = None;
    let mut last_self_check: Option<Instant> = None;
    // Built here rather than passed in because it is this loop's own state, and
    // because the answer it publishes is read off `descriptor` — which this loop
    // is the thing that keeps current across a failover.
    let fleet_host = crate::fleet::FleetHost::new(tokio::runtime::Handle::current());
    let mut warned_about_size = false;
    let mut last_work = Instant::now();
    loop {
        let mut shutdown = false;
        let mut did_work = false;
        // This belongs on the socket loop rather than the slow self-check: an
        // interactive launcher that dies before attaching has no session to
        // leave behind.
        if client.is_none()
            && tether == Tether::UntilFirstAttach
            && let Some(supervisor) = supervisor
            && !supervisor.is_alive()
        {
            eprintln!(
                "p2pmux node: the interactive launcher left before its first attachment — stopping"
            );
            shutdown = true;
        }
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let Some(stream) = configure_accepted(stream) else {
                        continue;
                    };
                    let mut reader = BufReader::new(stream);
                    match read_message(&mut reader) {
                        // Probes and shutdowns are control requests. They must not consume or
                        // contend with the single interactive attachment slot.
                        Ok(Some(ClientMessage::Probe)) => {
                            let _ = write_message(reader.get_mut(), &NodeMessage::ProbeAck);
                        }
                        // Like a probe: a one-shot request from a short-lived
                        // process, not the interactive client, so it must not
                        // contend for the single attachment slot. The producer
                        // gets no reply — it has already exited by the time one
                        // could be written, and a hook that blocks on the mux
                        // is a hook that stalls the agent it is reporting on.
                        Ok(Some(ClientMessage::AgentStatus {
                            pane_id,
                            kind,
                            status,
                            cwd,
                            message,
                        })) => {
                            // Counted here rather than in `p2pmux notify`,
                            // which is a separate process spawned by an agent
                            // hook on every tool call: a file write there would
                            // be a file write on the agent's critical path, and
                            // this side already has the message.
                            crate::telemetry::bump(crate::telemetry::Counter::Agents, 1);
                            did_work |=
                                node.apply_agent_status(pane_id, &kind, &status, &cwd, &message);
                        }
                        Ok(Some(ClientMessage::Shutdown { generation })) => {
                            let _ = write_message(
                                reader.get_mut(),
                                &NodeMessage::ShutdownAck { generation },
                            );
                            shutdown = true;
                            break;
                        }
                        Ok(Some(ClientMessage::Hello { cols, rows })) => {
                            let Ok(generation) = gate.attach() else {
                                let _ = write_message(
                                    reader.get_mut(),
                                    &NodeMessage::AttachRejected {
                                        reason: crate::local_ipc::ALREADY_ATTACHED.into(),
                                    },
                                );
                                continue;
                            };
                            match attach_client(reader, generation, cols, rows, descriptor, node) {
                                Ok(attached) => {
                                    client = Some(attached);
                                    if tether == Tether::UntilFirstAttach {
                                        tether = Tether::Detached;
                                    }
                                    // Only now is anybody looking. Until this,
                                    // this node had no location to broadcast.
                                    node.runtime.set_client_attached(true);
                                    did_work = true;
                                }
                                // Losing one client is not losing the session.
                                // Everything this can fail on belongs to the
                                // connection, not to the node: a peer that
                                // hung up mid-handshake -- for which macOS
                                // answers EINVAL to `setsockopt` rather than
                                // anything about the peer -- or a descriptor
                                // limit reached while duplicating its socket.
                                // Ending the socket loop over any of them took
                                // every pane down with it.
                                Err(error) => {
                                    eprintln!("p2pmux node: could not attach that client: {error}");
                                    let _ = gate.detach(generation);
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::BrokenPipe
                            ) => {}
                        Err(_) | Ok(Some(_)) => {}
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    continue;
                }
                // Stop accepting for this pass rather than ending the session.
                // The realistic causes are a descriptor limit reached by
                // something else on the machine, which passes; and a listener
                // that is genuinely broken costs one line per pass and leaves
                // the panes and the attached client alive, which is strictly
                // better than taking them down.
                Err(error) => {
                    eprintln!("p2pmux node: could not accept a local connection: {error}");
                    break;
                }
            }
        }
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drain_elapsed = Duration::ZERO;
        if periodic_drain_due(last_periodic_drain, drain_started) {
            last_periodic_drain = Some(drain_started);
            changed = node
                .drain()
                .map_err(|error| io::Error::other(error.to_string()))?;
            drain_elapsed = drain_started.elapsed();
        }
        // A takeover or a step-down changes what this machine is advertising about itself.
        // `p2pmux ls` and `p2pmux ticket <name>` read the record out of process, so until it
        // is rewritten they hand out a ticket for an endpoint that stopped answering.
        if let Some(role) = node.take_role_persist() {
            descriptor.role = if role.coordinating {
                SessionRole::Coordinator
            } else {
                SessionRole::Member
            };
            descriptor.ticket = role.ticket;
            descriptor.join_code = role.join_code;
            if let Err(error) = store.write(descriptor) {
                eprintln!("p2pmux node: failed to record the new session role: {error}");
            }
            did_work = true;
        }
        // Both machines remember each other, and neither of them is necessarily
        // being looked at when the other arrives. The node is the only part that
        // is always running, so it is what writes the pairing record: a machine
        // that joins while nobody is at the keyboard is still remembered, and
        // still reads as `asleep` rather than vanishing once it switches off.
        //
        // Only ever in a session pairing already knows about. A guest who joined
        // with a code you handed out is a collaborator, not a machine you own,
        // and must never end up in your fleet.
        //
        // Sampled on a timer, and on the timer alone. Recomputing member labels
        // and agent counts on every drain would put string allocation on the
        // path keystroke echo runs down, a hundred times a second, to keep a
        // status column fresher than any human could read it.
        //
        // It deliberately does *not* also require `drain` to have reported a
        // change. A machine joining is exactly the event this has to notice,
        // and it produces no pane output at all — so a session sitting quiet,
        // which is the normal state of a machine you are not looking at, would
        // never record the peer that just arrived.
        // What the node knows about itself: who is watching it, and how big it
        // has become. On its own timer rather than the peer scan's, because both
        // are questions about the process table whose answers change slowly.
        if self_check_due(last_self_check, drain_started) {
            last_self_check = Some(drain_started);
            // A tethered node outlives nothing.
            if tether == Tether::ToLauncher
                && let Some(supervisor) = supervisor
                && !supervisor.is_alive()
            {
                // Said out loud. A node that vanished without a word is how the
                // last set of these went unexplained for twelve hours.
                eprintln!("p2pmux node: the fleet agent that started this node is gone — stopping");
                shutdown = true;
            }
            if let Some(held) = crate::agent_detect::process_memory(std::process::id()) {
                let megabytes = held / (1024 * 1024);
                if tether == Tether::ToLauncher && held > TETHERED_MEMORY_CEILING {
                    eprintln!(
                        "p2pmux node: holding {megabytes}MB, past the {}MB a fleet node is \
                         allowed — stopping. The agent starts a fresh one.",
                        TETHERED_MEMORY_CEILING / (1024 * 1024),
                    );
                    shutdown = true;
                } else if held > MEMORY_WARN_AT && !warned_about_size {
                    // Once. The point is that somebody reading a log can see it
                    // coming, not that the log fills up with it.
                    warned_about_size = true;
                    eprintln!(
                        "p2pmux node: holding {megabytes}MB, which is more than this should need"
                    );
                }
            }
        }
        if peer_scan_due(last_peer_scan, drain_started) {
            last_peer_scan = Some(drain_started);
            let peers = node.session_peers();
            if peers != last_known_peers {
                last_known_peers.clone_from(&peers);
                // Out of process, so `p2pmux machines` can say whether a
                // machine is awake without attaching to the session and
                // bumping whatever client is already on it.
                descriptor.peers.clone_from(&peers);
                if let Err(error) = store.write(descriptor) {
                    eprintln!("p2pmux node: failed to record the session's machines: {error}");
                }
                let seen = peers
                    .iter()
                    .filter(|peer| !peer.this_machine)
                    .map(|peer| crate::pairing::SeenMachine {
                        name: peer.name.clone(),
                        machine_id: peer.machine_id.clone(),
                        kind: peer.kind,
                    })
                    .collect::<Vec<_>>();
                // Both outcomes are written down. A failure is obvious enough;
                // a refusal is not, and it is the one that gets reported as
                // "my other machine paired and never appeared here". Every
                // line of this is a rule doing its job, and the log is where
                // somebody can find out which rule, on a machine nobody was
                // sitting at when it happened.
                match crate::pairing::pin_peers(&seen) {
                    Ok(refused) => {
                        for refusal in refused {
                            eprintln!("p2pmux node: not added to this fleet — {refusal}");
                        }
                    }
                    Err(error) => {
                        eprintln!("p2pmux node: failed to record paired machines: {error}")
                    }
                }
            }
            // On the same timer, and deliberately not only when the membership
            // changed: a session started on this machine is not a membership
            // change here, and it is exactly the thing the fleet has to hear
            // about. See `exchange_fleet_invites`.
            node.runtime.exchange_fleet_invites();
            // The out-of-session half of the same sentence. `exchange_fleet_invites`
            // can only tell machines that are already here, which is why a machine
            // that was switched off used to be told nothing, forever. This writes
            // it where one can read it on waking. See `crate::fleet`.
            fleet_host.tick(descriptor);
        }
        did_work |= changed;
        let mut detached = false;
        let mut full_snapshot = false;
        if !shutdown && let Some(client) = client.as_mut() {
            match read_message(&mut client.reader) {
                Ok(Some(ClientMessage::Input {
                    bytes,
                    pane_id,
                    perf_id,
                })) => {
                    let target = node
                        .input(pane_id, bytes)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    if let Some(target) = target {
                        client.publish.arm_target_urgency(target, Instant::now());
                    }
                    client.publish.perf_id = perf_id;
                    if let Some(perf_id) = perf_id {
                        crate::perf::log(&format!("P2PMUX_PERF id={perf_id} node_input"));
                    }
                    let drain_started = Instant::now();
                    last_periodic_drain = Some(drain_started);
                    changed |= node
                        .drain()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    drain_elapsed += drain_started.elapsed();
                }
                Ok(Some(ClientMessage::StructuralIntent { intent })) => {
                    node.intent(intent)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    changed = true;
                }
                Ok(Some(ClientMessage::Resize { cols, rows })) => {
                    node.resize(cols, rows)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    frozen_scrollback.clear();
                    changed = true;
                }
                Ok(Some(ClientMessage::ResyncScreen { pane_id })) => {
                    client.publish.screen_sequences.remove(&pane_id);
                    client.publish.force_screens = true;
                    changed = true;
                    full_snapshot = true;
                }
                Ok(Some(ClientMessage::ScrollbackQuery {
                    pane_id,
                    history_id,
                    offset,
                    request_id,
                })) => {
                    // `None` alongside no snapshot means "nothing to report",
                    // not "nothing went wrong to report": the client shows this
                    // string on the footer, and a pane that simply has no
                    // history yet is not something to interrupt anybody about.
                    let mut unavailable: Option<&'static str> = None;
                    let (history_id, result) = if let Some(history_id) = history_id {
                        let result = frozen_scrollback
                            .get_mut(&history_id)
                            .filter(|frozen| frozen.pane_id == pane_id)
                            .map(|frozen| frozen.viewport(offset));
                        if result.is_none() {
                            unavailable = Some(SCROLLBACK_EXPIRED);
                        }
                        (history_id, result)
                    } else {
                        if frozen_scrollback.len() >= MAX_FROZEN_SCROLLBACK_SESSIONS {
                            // History browsing is local UI state; evicting old sessions is safe
                            // because a stale id receives an explicit unavailable response.
                            frozen_scrollback.clear();
                        }
                        frozen_scrollback.retain(|_, frozen| frozen.pane_id != pane_id);
                        let history_id = next_history_id;
                        next_history_id = next_history_id.wrapping_add(1).max(1);
                        let result = match node.node_local_scrollback(pane_id) {
                            crate::tui::LocalScrollback::Window(window) => {
                                // Freeze first, then read through the map: `viewport` needs `&mut`
                                // now that it moves the stored screen instead of copying it.
                                frozen_scrollback.insert(
                                    history_id,
                                    FrozenScrollback {
                                        pane_id,
                                        total_rows: window.total_rows,
                                        screen: window.screen,
                                    },
                                );
                                Some(
                                    frozen_scrollback
                                        .get_mut(&history_id)
                                        .expect("the frozen session was just inserted")
                                        .viewport(offset),
                                )
                            }
                            // Nothing has scrolled off it yet. The wheel has
                            // nowhere to go and there is nothing to say about
                            // that, so the client is told to stay where it is
                            // and the footer keeps whatever was on it.
                            crate::tui::LocalScrollback::Empty => None,
                            crate::tui::LocalScrollback::AlternateScreen => {
                                unavailable = Some(SCROLLBACK_ALTERNATE_SCREEN);
                                None
                            }
                            crate::tui::LocalScrollback::NotOurs => {
                                unavailable = Some(SCROLLBACK_NOT_OURS);
                                None
                            }
                        };
                        (history_id, result)
                    };
                    let (total_rows, snapshot) = match result {
                        Some((total_rows, snapshot)) => (total_rows, Some(snapshot)),
                        None => (0, None),
                    };
                    let unavailable = unavailable.map(String::from);
                    let _ = client.writer.enqueue(NodeMessage::ScrollbackWindow {
                        pane_id,
                        request_id,
                        history_id,
                        total_rows,
                        offset: offset.min(total_rows),
                        snapshot,
                        unavailable,
                    });
                    did_work = true;
                }
                Ok(Some(ClientMessage::SelectionCopy {
                    pane_id,
                    request_id,
                    anchor_scrollback,
                    anchor_row,
                    anchor_col,
                    cursor_scrollback,
                    cursor_row,
                    cursor_col,
                })) => {
                    let selection = crate::tui::selection_from_coordinates(
                        pane_id,
                        anchor_scrollback,
                        anchor_row,
                        anchor_col,
                        cursor_scrollback,
                        cursor_row,
                        cursor_col,
                    );
                    let _ = client.writer.enqueue(selection_copy_reply(
                        request_id,
                        node.runtime.node_selection_text(selection),
                    ));
                    did_work = true;
                }
                Ok(Some(ClientMessage::Focus { tab_id, pane_id })) => {
                    node.focus(tab_id, pane_id)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    // Answer the request even when it asked for the focus this node
                    // already had. The client holds its own optimistic focus until it
                    // sees the node say the same thing back, and focus is otherwise
                    // published only when it changes -- so a request that changed
                    // nothing would never be answered, and that client would pin
                    // itself to that pane and refuse every later focus, including the
                    // one that follows a freshly created pane.
                    client.publish.reannounce_focus();
                    changed = true;
                }
                Ok(Some(ClientMessage::Zoom {
                    pane_id,
                    cols,
                    rows,
                })) => {
                    node.zoom(pane_id, cols, rows)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    // A reflow rewrites every pane it touches, so a frozen
                    // history window taken against the old grid no longer
                    // describes the pane it came from -- the same reason a
                    // resize clears them.
                    frozen_scrollback.clear();
                    changed = true;
                }
                Ok(Some(ClientMessage::Detach {
                    generation: requested,
                })) if requested == client.generation => {
                    node.release_all_local_control()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    let _ = client.writer.enqueue_control(NodeMessage::DetachAck {
                        generation: client.generation,
                    });
                    client.close_after_ack = true;
                }
                Ok(Some(ClientMessage::Shutdown {
                    generation: requested,
                })) if requested == client.generation => {
                    let _ = client.writer.enqueue_control(NodeMessage::ShutdownAck {
                        generation: client.generation,
                    });
                    client.shutdown_after_ack = true;
                }
                Ok(Some(ClientMessage::Rename { name })) => {
                    if crate::session_store::valid_name(&name) {
                        let _ = client.writer.enqueue_control(NodeMessage::Update {
                            state: serde_json::json!({"name": name}),
                        });
                    } else {
                        let _ = client.writer.enqueue_control(NodeMessage::Error {
                            message: "invalid session name".into(),
                        });
                    }
                }
                Ok(Some(_)) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut => {}
                Ok(None) => detached = true,
                Err(_) => detached = true,
            }
            did_work |= changed;
            if !detached && !client.close_after_ack && !client.shutdown_after_ack {
                let result = if full_snapshot {
                    queue_snapshot(
                        descriptor,
                        node,
                        &mut client.publish,
                        drain_elapsed,
                        &client.writer,
                    )
                    .map(|()| true)
                } else {
                    queue_updates(
                        node,
                        &mut client.publish,
                        drain_elapsed,
                        Instant::now(),
                        &client.writer,
                    )
                };
                match result {
                    Ok(published) => did_work |= published,
                    Err(error) => {
                        eprintln!("p2pmux node: failed to write local update: {error}");
                        detached = true;
                    }
                }
            }
            if client.close_after_ack && client.writer.is_idle() {
                detached = true;
            }
            if client.shutdown_after_ack && client.writer.is_idle() {
                shutdown = true;
            }
        }
        if !changed {
            log_slow_drain(drain_elapsed);
        }
        if (detached || shutdown)
            && let Some(client) = client.take()
        {
            let _ = client.reader.get_ref().shutdown(Shutdown::Both);
            client.writer.close();
            let _ = gate.detach(client.generation);
            // The panes stay up and keep taking input; the person watching them
            // does not. Presence has to say the second without touching the
            // first, or a detached session leaves a dot on a pane forever.
            node.runtime.set_client_attached(false);
            node.release_all_local_control()
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        if shutdown {
            return Ok(());
        }
        let now = Instant::now();
        if did_work {
            last_work = now;
        }
        match socket_loop_backoff(client.is_some(), did_work, now.duration_since(last_work)) {
            Some(backoff) => std::thread::sleep(backoff),
            None => std::thread::yield_now(),
        }
    }
}

struct FrozenScrollback {
    pane_id: u64,
    total_rows: u64,
    screen: vt100::Screen,
}

impl FrozenScrollback {
    /// Moves the frozen screen's own offset rather than cloning it.
    ///
    /// The clone this replaces copied every retained row — up to `SCROLLBACK_LINES`
    /// separate `Vec<Cell>` allocations — on a screen the session already owns and
    /// nothing else can observe. That is 1.4ms at 24x80 and 5.7ms at 50x200 once
    /// history fills, paid on the socket loop for every wheel notch, while
    /// `set_scrollback` itself is a field write. Offsets are absolute, so
    /// successive calls need no restore.
    fn viewport(&mut self, offset: u64) -> (u64, Vec<u8>) {
        self.screen
            .set_scrollback(offset.min(self.total_rows) as usize);
        let snapshot = crate::screen::snapshot_payload(&self.screen)
            .expect("host scrollback viewport must fit the screen codec")
            .as_ref()
            .to_vec();
        (self.total_rows, snapshot)
    }
}

/// Whether the loop should run the periodic (non-input) drain this turn.
///
/// Client input drains straight away and stamps `last_drain` itself, so a burst of
/// keystrokes never waits on this.
fn periodic_drain_due(last_drain: Option<Instant>, now: Instant) -> bool {
    last_drain.is_none_or(|last| now.duration_since(last) >= PERIODIC_DRAIN_INTERVAL)
}

fn peer_scan_due(last_scan: Option<Instant>, now: Instant) -> bool {
    last_scan.is_none_or(|last| now.duration_since(last) >= PEER_SCAN_INTERVAL)
}

fn self_check_due(last_check: Option<Instant>, now: Instant) -> bool {
    last_check.is_none_or(|last| now.duration_since(last) >= SELF_CHECK_INTERVAL)
}

fn socket_loop_backoff(
    client_attached: bool,
    did_work: bool,
    quiet_for: Duration,
) -> Option<Duration> {
    match (client_attached, did_work) {
        (true, true) => None,
        (true, false) => Some(ATTACHED_IDLE_BACKOFF),
        (false, false) if quiet_for >= DETACHED_QUIET_AFTER => Some(DETACHED_QUIET_BACKOFF),
        (false, _) => Some(DETACHED_IDLE_BACKOFF),
    }
}

fn read_message(reader: &mut BufReader<UnixStream>) -> io::Result<Option<ClientMessage>> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid local IPC message")),
        Err(error) => Err(error),
    }
}
fn write_message(stream: &mut UnixStream, message: &NodeMessage) -> io::Result<()> {
    let mut frame = serde_json::to_vec(message).map_err(io::Error::other)?;
    frame.push(b'\n');
    stream.write_all(&frame)?;
    stream.flush()
}

struct AttachedClient {
    reader: BufReader<UnixStream>,
    generation: u64,
    publish: AttachmentPublishState,
    writer: AttachmentWriter,
    close_after_ack: bool,
    shutdown_after_ack: bool,
}

/// Take a client that said hello onto the single attachment.
///
/// Separate from the socket loop so that it can fail without the loop failing:
/// every step here is about one connection, and the loop has a session's panes
/// behind it. The connection is dropped on the way out of the error, which is
/// what tells a client that is still there to stop waiting.
fn attach_client(
    mut reader: BufReader<UnixStream>,
    generation: u64,
    cols: u16,
    rows: u16,
    descriptor: &SessionDescriptor,
    node: &mut SharedLayoutNode,
) -> io::Result<AttachedClient> {
    node.resize(cols, rows)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut publish = AttachmentPublishState::default();
    write_message(
        reader.get_mut(),
        &NodeMessage::AttachAccepted {
            generation,
            selection_copy: true,
        },
    )?;
    write_snapshot(
        reader.get_mut(),
        descriptor,
        node,
        &mut publish,
        Duration::ZERO,
    )?;
    reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(1)))?;
    let writer = AttachmentWriter::start(reader.get_mut().try_clone()?)?;
    Ok(AttachedClient {
        reader,
        generation,
        publish,
        writer,
        close_after_ack: false,
        shutdown_after_ack: false,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueResult {
    Queued,
    Dropped,
    CoalesceScreens,
    Disconnected,
}

struct OutboundState {
    messages: VecDeque<NodeMessage>,
    writing: bool,
    closed: bool,
}

struct AttachmentWriter {
    state: Arc<(Mutex<OutboundState>, Condvar)>,
    shutdown: UnixStream,
    worker: thread::JoinHandle<()>,
}

impl AttachmentWriter {
    fn start(mut stream: UnixStream) -> io::Result<Self> {
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let shutdown = stream.try_clone()?;
        let state = Arc::new((
            Mutex::new(OutboundState {
                messages: VecDeque::with_capacity(OUTBOUND_QUEUE),
                writing: false,
                closed: false,
            }),
            Condvar::new(),
        ));
        let worker_state = Arc::clone(&state);
        let worker = thread::spawn(move || {
            loop {
                let message = {
                    let (lock, ready) = &*worker_state;
                    let mut state = lock.lock().expect("outbound queue poisoned");
                    while state.messages.is_empty() && !state.closed {
                        state = ready.wait(state).expect("outbound queue poisoned");
                    }
                    let message = state.messages.pop_front();
                    state.writing = message.is_some();
                    ready.notify_all();
                    match message {
                        Some(message) => message,
                        None => return,
                    }
                };
                if write_message(&mut stream, &message).is_err() {
                    let (lock, ready) = &*worker_state;
                    lock.lock().expect("outbound queue poisoned").closed = true;
                    ready.notify_all();
                    return;
                }
                let (lock, ready) = &*worker_state;
                lock.lock().expect("outbound queue poisoned").writing = false;
                ready.notify_all();
            }
        });
        Ok(Self {
            state,
            shutdown,
            worker,
        })
    }

    fn enqueue(&self, message: NodeMessage) -> QueueResult {
        self.push(message, false)
    }

    fn enqueue_control(&self, message: NodeMessage) -> QueueResult {
        self.push(message, true)
    }

    fn push(&self, message: NodeMessage, control: bool) -> QueueResult {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().expect("outbound queue poisoned");
        let result = push_outbound(&mut state, message, control);
        if result == QueueResult::Queued {
            ready.notify_one();
        }
        result
    }

    fn is_idle(&self) -> bool {
        let (lock, _) = &*self.state;
        let state = lock.lock().expect("outbound queue poisoned");
        (state.messages.is_empty() && !state.writing) || state.closed
    }

    fn close(self) {
        let (lock, ready) = &*self.state;
        lock.lock().expect("outbound queue poisoned").closed = true;
        ready.notify_all();
        let _ = self.shutdown.shutdown(Shutdown::Both);
        let _ = self.worker.join();
    }
}

fn push_outbound(state: &mut OutboundState, message: NodeMessage, control: bool) -> QueueResult {
    if state.closed {
        return QueueResult::Disconnected;
    }
    if state.messages.len() == OUTBOUND_QUEUE {
        if matches!(message, NodeMessage::Screens { .. }) {
            return QueueResult::CoalesceScreens;
        }
        if matches!(message, NodeMessage::ScrollbackWindow { .. }) {
            return QueueResult::Dropped;
        }
        state
            .messages
            .retain(|queued| !matches!(queued, NodeMessage::ScrollbackWindow { .. }));
        if control && state.messages.len() == OUTBOUND_QUEUE {
            state
                .messages
                .retain(|queued| !matches!(queued, NodeMessage::Screens { .. }));
        }
        if state.messages.len() == OUTBOUND_QUEUE {
            return QueueResult::Dropped;
        }
    }
    state.messages.push_back(message);
    QueueResult::Queued
}
fn write_snapshot(
    stream: &mut UnixStream,
    descriptor: &SessionDescriptor,
    node: &SharedLayoutNode,
    publish: &mut AttachmentPublishState,
    drain_elapsed: Duration,
) -> io::Result<()> {
    let (message, layout, leases, rosters, screens, stats) =
        snapshot_message(descriptor, node, publish)?;
    let json_started = Instant::now();
    let mut frame = serde_json::to_vec(&message).map_err(io::Error::other)?;
    frame.push(b'\n');
    let json_serialize = json_started.elapsed();
    let write_started = Instant::now();
    stream.write_all(&frame)?;
    stream.flush()?;
    let json_write = write_started.elapsed();
    publish.layout = Some(layout);
    publish.leases = Some(leases);
    publish.rosters = Some(rosters);
    publish.focus = Some(node.local_focus());
    publish.presence = Some(node.presence_rows());
    update_screen_sequences(&mut publish.screen_sequences, &screens);
    publish.last_screen_publish = Some(Instant::now());
    publish.force_screens = false;
    if crate::perf::enabled()
        && [drain_elapsed, json_serialize, json_write]
            .into_iter()
            .any(|elapsed| elapsed >= Duration::from_millis(5))
    {
        crate::perf::log(&format!(
            "P2PMUX_PERF node drain_ms={} snapshot_json_ms={} snapshot_write_ms={} write_bytes={} updates_snapshot={}({}B) updates_delta={}({}B) updates_unchanged={}",
            drain_elapsed.as_millis(),
            json_serialize.as_millis(),
            json_write.as_millis(),
            frame.len(),
            stats.snapshots,
            stats.snapshot_bytes,
            stats.deltas,
            stats.delta_bytes,
            stats.unchanged,
        ));
    }
    Ok(())
}

type SnapshotMessage = (
    NodeMessage,
    crate::layout::LayoutSnapshot,
    Vec<PaneLeaseSnapshot>,
    Vec<AgentOverlaySnapshotRow>,
    Vec<PaneScreenSnapshot>,
    ScreenUpdateStats,
);

fn snapshot_message(
    descriptor: &SessionDescriptor,
    node: &SharedLayoutNode,
    publish: &AttachmentPublishState,
) -> io::Result<SnapshotMessage> {
    let snapshot_started = Instant::now();
    let (tab_id, pane_id) = node.local_focus();
    let local_peer_id = node.local_peer_id();
    let (layout, screens, leases, rosters) = node.snapshot();
    let _snapshot_build = snapshot_started.elapsed();
    let hosts = layout
        .panes
        .values()
        .map(|pane| pane.host_peer_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u32;
    let coordinator_name = layout
        .members
        .first()
        .map(|member| member.display_name.clone())
        .unwrap_or_default();
    let screens = pane_screen_updates(screens, &publish.screen_sequences, |pane_id| {
        node.node_remote_snapshot(pane_id)
    });
    let updates = ScreenUpdateStats::from_screens(&screens);
    let leases = leases
        .into_iter()
        .map(
            |(pane_id, (ready, controller_peer_id, controller_active))| PaneLeaseSnapshot {
                pane_id,
                ready,
                controller_peer_id,
                controller_active,
            },
        )
        .collect::<Vec<_>>();
    let message = NodeMessage::Snapshot {
        room_name: descriptor.name.clone(),
        // Read live rather than from the record this node started with: the role can change
        // under a running session, and the attached client draws it every frame.
        role: if node.is_coordinating() {
            "coordinator"
        } else {
            "member"
        }
        .into(),
        summary: SessionSummary {
            tabs: layout.tabs.len() as u32,
            panes: layout.panes.len() as u32,
            hosts,
            coordinator_name,
        },
        layout: Box::new(layout.clone()),
        screens: screens.clone(),
        leases: leases.clone(),
        rosters: rosters.clone(),
        presence: node.presence_rows(),
        local_peer_id,
        tab_id,
        pane_id,
        ticket: node.runtime.share_ticket().map(str::to_owned),
        code: node.runtime.share_code().map(str::to_owned),
    };
    Ok((message, layout, leases, rosters, screens, updates))
}

fn queue_snapshot(
    descriptor: &SessionDescriptor,
    node: &SharedLayoutNode,
    publish: &mut AttachmentPublishState,
    _drain_elapsed: Duration,
    writer: &AttachmentWriter,
) -> io::Result<()> {
    let (message, layout, leases, rosters, screens, _) =
        snapshot_message(descriptor, node, publish)?;
    match writer.enqueue(message) {
        QueueResult::Queued => {
            publish.layout = Some(layout);
            publish.leases = Some(leases);
            publish.rosters = Some(rosters);
            publish.focus = Some(node.local_focus());
            update_screen_sequences(&mut publish.screen_sequences, &screens);
            publish.last_screen_publish = Some(Instant::now());
            publish.force_screens = false;
        }
        QueueResult::Dropped | QueueResult::CoalesceScreens => {
            publish.reset_for_snapshot();
        }
        QueueResult::Disconnected => {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local attachment disconnected",
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct AttachmentPublishState {
    layout: Option<crate::layout::LayoutSnapshot>,
    leases: Option<Vec<PaneLeaseSnapshot>>,
    rosters: Option<Vec<AgentOverlaySnapshotRow>>,
    status: Option<String>,
    paths: Option<Vec<crate::transport::PeerPath>>,
    session_locked: Option<bool>,
    /// The held remote-work request last told to the client. The outer `Option`
    /// is "have we ever said"; the inner one is the answer itself.
    remote_work: Option<Option<Vec<String>>>,
    focus: Option<(u64, u64)>,
    presence: Option<Vec<PresenceRow>>,
    screen_sequences: BTreeMap<u64, u64>,
    last_screen_publish: Option<Instant>,
    pending_screens: bool,
    force_screens: bool,
    target_urgency: Option<(u64, Instant)>,
    perf_id: Option<u64>,
}

impl AttachmentPublishState {
    fn arm_target_urgency(&mut self, pane_id: u64, now: Instant) {
        self.target_urgency = Some((pane_id, now + TARGET_SCREEN_URGENCY_TTL));
    }

    /// Whether this client still has to be told where the focus is.
    fn focus_due(&self, focus: (u64, u64)) -> bool {
        self.focus != Some(focus)
    }

    /// Forget what this client was last told about focus, so the next publish
    /// says it again even though nothing moved.
    fn reannounce_focus(&mut self) {
        self.focus = None;
    }

    fn reset_for_snapshot(&mut self) {
        self.layout = None;
        self.leases = None;
        self.rosters = None;
        self.status = None;
        self.paths = None;
        self.session_locked = None;
        self.focus = None;
        self.presence = None;
        self.screen_sequences.clear();
        self.pending_screens = true;
        self.force_screens = true;
    }
}

fn queue_updates(
    node: &SharedLayoutNode,
    publish: &mut AttachmentPublishState,
    _drain_elapsed: Duration,
    now: Instant,
    writer: &AttachmentWriter,
) -> io::Result<bool> {
    let (layout, screens, leases, rosters) = node.snapshot();
    let leases = leases
        .into_iter()
        .map(
            |(pane_id, (ready, controller_peer_id, controller_active))| PaneLeaseSnapshot {
                pane_id,
                ready,
                controller_peer_id,
                controller_active,
            },
        )
        .collect::<Vec<_>>();
    let focus = node.local_focus();
    let layout_changed = publish.layout.as_ref() != Some(&layout);
    let mut published = false;
    if layout_changed {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Layout {
                layout: Box::new(layout.clone()),
            },
        )? {
            return Ok(published);
        }
        publish.layout = Some(layout.clone());
        publish
            .screen_sequences
            .retain(|pane_id, _| layout.panes.contains_key(pane_id));
        published = true;
    }
    if publish.leases.as_ref() != Some(&leases) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Leases {
                leases: leases.clone(),
            },
        )? {
            return Ok(published);
        }
        publish.leases = Some(leases);
        published = true;
    }
    let status = node.status();
    if publish.status.as_deref() != Some(status.as_str()) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Status {
                message: status.clone(),
            },
        )? {
            return Ok(published);
        }
        publish.status = Some(status);
        published = true;
    }
    let paths = node.peer_paths();
    if publish.paths.as_ref() != Some(&paths) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Paths {
                paths: paths.clone(),
            },
        )? {
            return Ok(published);
        }
        publish.paths = Some(paths);
        published = true;
    }
    let session_locked = node.session_locked();
    if publish.session_locked != Some(session_locked) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::SessionLock {
                locked: session_locked,
            },
        )? {
            return Ok(published);
        }
        publish.session_locked = Some(session_locked);
        published = true;
    }
    // The question belongs on the machine the terminal would run on, and the
    // client is the part of that machine a human is looking at. The node keeps
    // holding the request either way; this only moves the asking.
    let remote_work = node.runtime.pending_remote_work();
    if publish.remote_work != Some(remote_work.clone()) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::RemoteWork {
                command: remote_work.clone(),
            },
        )? {
            return Ok(published);
        }
        publish.remote_work = Some(remote_work);
        published = true;
    }
    if publish.rosters.as_ref() != Some(&rosters) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Rosters {
                rosters: rosters.clone(),
            },
        )? {
            return Ok(published);
        }
        publish.rosters = Some(rosters);
        published = true;
    }
    let presence = node.presence_rows();
    if publish.presence.as_ref() != Some(&presence) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Presence {
                presence: presence.clone(),
            },
        )? {
            return Ok(published);
        }
        publish.presence = Some(presence);
        published = true;
    }
    if publish.focus_due(focus) {
        if !queue_update(
            writer,
            publish,
            NodeMessage::Focus {
                tab_id: focus.0,
                pane_id: focus.1,
            },
        )? {
            return Ok(published);
        }
        publish.focus = Some(focus);
        published = true;
    }
    let frames = pane_screen_updates(screens, &publish.screen_sequences, |pane_id| {
        node.node_remote_snapshot(pane_id)
    });
    if frames.is_empty() {
        publish.pending_screens = false;
        publish.force_screens = false;
        return Ok(published);
    }
    if !screens_due(publish, published, now, &frames) {
        publish.pending_screens = true;
        return Ok(published);
    }
    let perf_id = publish.perf_id;
    if !queue_update(
        writer,
        publish,
        NodeMessage::Screens {
            screens: frames.clone(),
            perf_id,
        },
    )? {
        return Ok(published);
    }
    if let Some(perf_id) = perf_id {
        crate::perf::log(&format!("P2PMUX_PERF id={perf_id} node_publish"));
        publish.perf_id = None;
    }
    update_screen_sequences(&mut publish.screen_sequences, &frames);
    publish.last_screen_publish = Some(now);
    publish.pending_screens = false;
    publish.force_screens = false;
    Ok(true)
}

fn queue_update(
    writer: &AttachmentWriter,
    publish: &mut AttachmentPublishState,
    message: NodeMessage,
) -> io::Result<bool> {
    match writer.enqueue(message) {
        QueueResult::Queued => Ok(true),
        QueueResult::Dropped | QueueResult::CoalesceScreens => {
            publish.reset_for_snapshot();
            Ok(false)
        }
        QueueResult::Disconnected => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "local attachment disconnected",
        )),
    }
}

fn screens_due(
    publish: &AttachmentPublishState,
    structural: bool,
    now: Instant,
    frames: &[PaneScreenSnapshot],
) -> bool {
    let target_due = publish.target_urgency.is_some_and(|(pane_id, expires)| {
        now <= expires
            && frames.iter().any(|frame| frame.pane_id == pane_id)
            && publish
                .last_screen_publish
                .is_none_or(|last| now.duration_since(last) >= TARGET_SCREEN_PUBLISH_INTERVAL)
    });
    structural
        || publish.force_screens
        || target_due
        || publish
            .last_screen_publish
            .is_none_or(|last| now.duration_since(last) >= SCREEN_PUBLISH_INTERVAL)
}

fn pane_screen_updates<RemoteSnapshot>(
    screens: crate::tui::NodeScreenSnapshots,
    sequences: &BTreeMap<u64, u64>,
    mut remote_snapshot: RemoteSnapshot,
) -> Vec<PaneScreenSnapshot>
where
    RemoteSnapshot: FnMut(u64) -> Option<Vec<u8>>,
{
    screens
        .into_iter()
        .filter_map(|(pane_id, screen)| {
            let update = match screen {
                crate::tui::NodeScreenSnapshot::Local {
                    frame,
                    history_len,
                    history_end,
                } => {
                    if sequences.get(&pane_id) == Some(&frame.sequence) {
                        None
                    } else if sequences.get(&pane_id) == Some(&frame.base_sequence) {
                        let state = ScreenUpdate::Delta {
                            base_sequence: frame.base_sequence,
                            sequence: frame.sequence,
                            delta: frame.delta.as_ref().to_vec(),
                            kitty_keyboard_active: frame.kitty_keyboard_active,
                        };
                        Some((
                            frame.sequence,
                            state,
                            history_len,
                            history_end,
                            frame.reset_outer,
                        ))
                    } else {
                        let state = ScreenUpdate::Snapshot {
                            sequence: frame.sequence,
                            snapshot: frame.snapshot.as_ref().to_vec(),
                            kitty_keyboard_active: frame.kitty_keyboard_active,
                        };
                        Some((
                            frame.sequence,
                            state,
                            history_len,
                            history_end,
                            frame.reset_outer,
                        ))
                    }
                }
                crate::tui::NodeScreenSnapshot::Remote {
                    sequence,
                    kitty_keyboard_active,
                } => {
                    if sequences.get(&pane_id) == Some(&sequence) {
                        None
                    } else {
                        Some((
                            sequence,
                            ScreenUpdate::Snapshot {
                                sequence,
                                snapshot: remote_snapshot(pane_id)?,
                                kitty_keyboard_active,
                            },
                            0,
                            0,
                            false,
                        ))
                    }
                }
            };
            let (_, state, history_len, history_end, reset_outer) = update?;
            Some(PaneScreenSnapshot {
                pane_id,
                state,
                reset_outer,
                history_len,
                history_end,
            })
        })
        .collect()
}

fn update_screen_sequences(sequences: &mut BTreeMap<u64, u64>, screens: &[PaneScreenSnapshot]) {
    for screen in screens {
        let sequence = match screen.state {
            ScreenUpdate::Snapshot { sequence, .. }
            | ScreenUpdate::Delta { sequence, .. }
            | ScreenUpdate::Unchanged { sequence, .. } => sequence,
        };
        sequences.insert(screen.pane_id, sequence);
    }
}

#[derive(Default)]
struct ScreenUpdateStats {
    snapshots: usize,
    snapshot_bytes: usize,
    deltas: usize,
    delta_bytes: usize,
    unchanged: usize,
}

impl ScreenUpdateStats {
    fn from_screens(screens: &[PaneScreenSnapshot]) -> Self {
        let mut stats = Self::default();
        for screen in screens {
            match &screen.state {
                ScreenUpdate::Snapshot { snapshot, .. } => {
                    stats.snapshots += 1;
                    stats.snapshot_bytes += snapshot.len();
                }
                ScreenUpdate::Delta { delta, .. } => {
                    stats.deltas += 1;
                    stats.delta_bytes += delta.len();
                }
                ScreenUpdate::Unchanged { .. } => stats.unchanged += 1,
            }
        }
        stats
    }
}

fn log_slow_drain(elapsed: Duration) {
    if crate::perf::enabled() && elapsed >= Duration::from_millis(5) {
        crate::perf::log(&format!(
            "P2PMUX_PERF node drain_ms={}",
            elapsed.as_millis()
        ));
    }
}

impl SharedLayoutNode {
    pub fn new(runtime: SharedLayoutRuntime) -> Self {
        Self { runtime }
    }

    /// Advances Iroh, pane servers, PTYs, leases, subscriptions and agent sampling without ever
    /// touching terminal state.
    pub fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        self.runtime.drain_node()
    }

    /// Deliver a client's bytes to the pane it named, or to this node's focus
    /// when the client named none. Returns the pane they reached, if any.
    pub fn input(
        &mut self,
        pane_id: Option<u64>,
        bytes: Vec<u8>,
    ) -> Result<Option<u64>, Box<dyn Error>> {
        self.runtime.node_input(pane_id, bytes)
    }
    pub fn local_peer_id(&self) -> Vec<u8> {
        self.runtime.local_peer_id()
    }
    pub(crate) fn session_peers(&self) -> Vec<crate::session_store::SessionPeer> {
        self.runtime.session_peers()
    }
    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> {
        self.runtime.release_all_local_control()
    }
    pub fn local_focus(&self) -> (u64, u64) {
        self.runtime.local_focus()
    }

    /// What this node last told the session about where it is looking.
    #[cfg(test)]
    pub(crate) fn local_presence(&self) -> Option<crate::protocol::Presence> {
        self.runtime.local_presence().cloned()
    }

    /// Where the other members are looking, for the attached client to draw.
    pub fn presence_rows(&self) -> Vec<PresenceRow> {
        self.runtime.presence_rows()
    }
    pub(crate) fn status(&self) -> String {
        self.runtime.status().to_owned()
    }
    pub(crate) fn peer_paths(&self) -> Vec<crate::transport::PeerPath> {
        self.runtime.peer_paths()
    }
    pub(crate) fn session_locked(&self) -> bool {
        self.runtime.session_locked()
    }
    pub(crate) fn snapshot(
        &self,
    ) -> (
        crate::layout::LayoutSnapshot,
        crate::tui::NodeScreenSnapshots,
        crate::tui::NodeLeaseSnapshots,
        Vec<AgentOverlaySnapshotRow>,
    ) {
        self.runtime.node_snapshot()
    }
    pub(crate) fn node_local_scrollback(&self, pane_id: u64) -> crate::tui::LocalScrollback {
        self.runtime.node_local_scrollback(pane_id)
    }
    pub(crate) fn node_remote_snapshot(&self, pane_id: u64) -> Option<Vec<u8>> {
        self.runtime.node_remote_snapshot(pane_id)
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), Box<dyn Error>> {
        self.runtime.node_resize(cols, rows)
    }
    pub fn zoom(
        &mut self,
        pane_id: Option<u64>,
        cols: u16,
        rows: u16,
    ) -> Result<(), Box<dyn Error>> {
        self.runtime.node_zoom(pane_id, cols, rows)
    }
    pub fn focus(&mut self, tab_id: u64, pane_id: u64) -> Result<(), Box<dyn Error>> {
        self.runtime.node_focus(tab_id, pane_id)
    }
    pub fn intent(&mut self, intent: crate::tui::UiIntent) -> Result<(), Box<dyn Error>> {
        self.runtime.node_intent(intent)
    }
    pub fn apply_agent_status(
        &mut self,
        pane_id: u64,
        kind: &str,
        status: &str,
        cwd: &str,
        message: &str,
    ) -> bool {
        self.runtime
            .apply_agent_status(pane_id, kind, status, cwd, message)
    }
    /// Whether this node currently serializes the session, for the attached client's header.
    pub fn is_coordinating(&self) -> bool {
        self.runtime.is_coordinating()
    }

    /// A role change the durable session record still has to be told about, once.
    pub fn take_role_persist(&mut self) -> Option<crate::tui::RolePersist> {
        self.runtime.take_role_persist()
    }

    pub fn shutdown(self) {
        self.runtime.shutdown_node();
    }
}

#[cfg(test)]
mod tests {
    /// Where cgroups do not exist, the ceiling has to live in the process.
    ///
    /// macOS has no memory controller for launchd to enforce, so the plist
    /// cannot promise what the systemd unit does. A node that polices itself is
    /// the only thing that works on both platforms -- and on Linux it is still
    /// the better half of the pair, because it notices first and says why,
    /// leaving the kernel's killer as a backstop rather than the explanation.
    #[test]
    fn a_fleet_node_stops_itself_before_the_kernel_has_to() {
        // That the ceiling sits under the cgroup's, and the warning under the
        // ceiling, is asserted at compile time where the constants are. What is
        // left to check here is that the rule is applied to the right nodes.
        let loop_source = include_str!("node.rs")
            .split_once("fn run_socket_loop(")
            .expect("the socket loop")
            .1
            .split_once("#[cfg(test)]")
            .expect("the tests, which are not the loop")
            .0;
        assert!(
            loop_source.contains("tether == Tether::ToLauncher && held > TETHERED_MEMORY_CEILING"),
            "only a node nobody is sitting in front of may be stopped for its size"
        );

        // And the flag has to be raised before the pass that acts on it, or
        // every one of these decisions is discarded at the top of the next
        // iteration and the node runs on regardless.
        let raised = loop_source
            .find("if self_check_due(")
            .expect("the self check");
        let acted_on = loop_source
            .rfind("if shutdown {")
            .expect("the pass that stops the node");
        assert!(
            raised < acted_on,
            "a node that decides to stop after the loop has already checked never stops"
        );
    }

    /// A session somebody is working in is worth more than the memory it holds.
    ///
    /// Stopping one to save a machine that is not actually short of memory would
    /// be the tool deciding it knows better than the person using it. Saying how
    /// big it has got is not.
    #[test]
    fn a_session_somebody_started_is_told_about_rather_than_stopped() {
        let loop_source = include_str!("node.rs")
            .split_once("fn run_socket_loop(")
            .expect("the socket loop")
            .1
            .split_once("#[cfg(test)]")
            .expect("the tests")
            .0;
        let warn_arm = loop_source
            .split_once("} else if held > MEMORY_WARN_AT")
            .expect("the warning arm")
            .1
            .split_once('}')
            .expect("the end of it")
            .0;
        assert!(
            !warn_arm.contains("shutdown = true"),
            "warning about a size must not also act on it: {warn_arm}"
        );
    }

    /// A node the fleet agent started must not outlive it.
    ///
    /// The nine that did on 2026-08-16 were reparented to PID 1 when the
    /// out-of-memory killer took their agent, and ran for another twelve hours
    /// with nothing watching them and nothing able to find them.
    #[test]
    fn a_tethered_node_stops_when_its_launcher_does() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a stand-in launcher");
        let supervisor = super::Supervisor {
            pid: child.id(),
            started_at: crate::agent_detect::process_start_time(child.id())
                .expect("a running process has a start time"),
        };
        assert!(supervisor.is_alive(), "the launcher is running");

        child.kill().expect("stop the stand-in launcher");
        child.wait().expect("reap it");
        assert!(
            !supervisor.is_alive(),
            "the node would have outlived the process that started it"
        );
    }

    /// Pids are reused, quickly, on a machine that builds software. A tether
    /// that trusted the number alone would eventually be watching somebody
    /// else's process -- and would either keep a node alive under a dead agent
    /// or shut one down under a healthy one.
    #[test]
    fn a_recycled_pid_is_not_the_process_the_node_was_tethered_to() {
        let pid = std::process::id();
        let real = super::Supervisor::current().expect("this process has a start time");
        assert_eq!(real.pid, pid);
        assert!(real.is_alive());

        let impostor = super::Supervisor {
            pid,
            started_at: real.started_at.wrapping_add(1),
        };
        assert!(
            !impostor.is_alive(),
            "the same pid at a different start time is a different process"
        );
    }

    /// An interactive node belongs to the launcher's first client, not merely
    /// to the launcher process that gave it a socket.
    #[test]
    fn an_interactive_session_waits_only_for_its_first_client() {
        let source = include_str!("cli.rs");
        let interactive = source
            .split_once("Some(Command::Create {")
            .expect("the create arm")
            .1
            .split_once("Tether::")
            .expect("the create arm's tether")
            .1;
        assert!(
            interactive.starts_with("UntilFirstAttach"),
            "`create` must not survive a terminal that never attached"
        );

        let agent = include_str!("daemon.rs");
        assert!(
            agent.contains("follow_fleet_invite(ticket, crate::node::Tether::ToLauncher)"),
            "the fleet agent's node is the one that must not outlive its launcher"
        );
    }

    use super::*;
    use crate::{
        layout::{Axis, Node, Pane},
        screen::HostScreen,
        session::{HostSession, SharedLayoutHost, layout_snapshot_from_state},
        tui::SharedLocalPane,
    };
    use std::{
        cell::Cell,
        io::Write,
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
        thread,
    };

    #[test]
    fn a_peer_that_hung_up_before_being_accepted_costs_only_its_connection() {
        // `SessionStore::sweep_dead_sockets` tests liveness by connecting and dropping at
        // once, so this is what every `p2pmux attach`, `ticket` or `--resume` does to a live
        // node. macOS answers EINVAL when the timeouts are applied to a peer that is already
        // gone; propagating that ended the socket loop, and the node tore its own session
        // down. One dead connection must never be able to do that.
        let directory = std::env::temp_dir().join(format!("p2pmux-accept-{}", std::process::id()));
        let _ = fs::create_dir_all(&directory);
        let path = directory.join("node.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("listener should bind");

        drop(UnixStream::connect(&path).expect("sweep-style probe should connect"));
        let (accepted, _) = listener.accept().expect("accept should succeed");
        // Whether the platform refuses this one is its business; the signature is the fix,
        // because an `Option` cannot be `?`-propagated into ending the session.
        let _: Option<UnixStream> = configure_accepted(accepted);

        // What must hold is that the dead peer left nothing behind: the next real client
        // still gets a usable stream from the same listener.
        let live = UnixStream::connect(&path).expect("live peer should connect");
        let (accepted, _) = listener.accept().expect("accept should succeed");
        assert!(
            configure_accepted(accepted).is_some(),
            "a hung-up peer poisoned the listener for the next client"
        );
        drop(live);

        let _ = fs::remove_dir_all(&directory);
    }

    /// The other half of the same rule, where the type system cannot state it.
    ///
    /// `configure_accepted` returns an `Option` precisely so that one dead
    /// connection cannot be `?`-propagated into ending the session. The arms
    /// that run *after* it — the ones that take a client onto the attachment —
    /// are ordinary fallible calls, and the loop kept its own escape hatch: a
    /// `?` on the socket options, on the descriptor duplicated for the writer,
    /// and a `return Err` for anything `accept` answered that was not already
    /// named. Each of those is about one connection and none of them is worth
    /// a session, so the accept loop no longer has a way out.
    #[test]
    fn nothing_about_one_connection_leaves_the_accept_loop() {
        let source = include_str!("node.rs");
        let loop_body = source
            .split_once("match listener.accept() {")
            .expect("the accept loop")
            .1
            .split_once("let drain_started = Instant::now();")
            .expect("the end of the accept loop")
            .0;

        assert!(
            !loop_body.contains("?;"),
            "a per-connection failure can end the socket loop again"
        );
        assert!(
            !loop_body.contains("return Err("),
            "an accept error can end the socket loop again"
        );
        assert!(
            loop_body.contains("attach_client("),
            "the hello arm should hand off to the fallible helper"
        );
    }

    /// The shape of every agent hook: write one line, exit immediately.
    ///
    /// `p2pmux notify` runs on the agent's critical path and has nothing to wait
    /// for, so its socket is almost always closed before the node gets round to
    /// accepting it. macOS answers EINVAL to `setsockopt` on such a socket, and
    /// refusing the connection on that basis discarded the payload already
    /// sitting in its buffer — silently, on every single hook. Wiring the hooks
    /// up looked like it did nothing, because nothing is what it did.
    #[test]
    fn a_producer_that_exits_before_being_accepted_still_delivers_its_status() {
        let directory =
            std::env::temp_dir().join(format!("p2pmux-producer-{}", std::process::id()));
        let _ = fs::create_dir_all(&directory);
        let path = directory.join("node.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("listener should bind");

        {
            let mut producer = UnixStream::connect(&path).expect("producer should connect");
            let mut line = serde_json::to_vec(&ClientMessage::AgentStatus {
                pane_id: 3,
                kind: "claude".into(),
                status: "pending".into(),
                cwd: "/repo".into(),
                message: "shall I force-push?".into(),
            })
            .expect("encodes");
            line.push(b'\n');
            producer.write_all(&line).expect("producer writes one line");
            producer.flush().expect("producer flushes");
        } // and exits, exactly as the hook binary does

        let (accepted, _) = listener.accept().expect("accept should succeed");
        let stream = configure_accepted(accepted)
            .expect("a producer that already exited is still worth reading");
        let mut reader = BufReader::new(stream);
        assert!(
            matches!(
                read_message(&mut reader).expect("the buffered line is still readable"),
                Some(ClientMessage::AgentStatus { pane_id, status, .. })
                    if pane_id == 3 && status == "pending"
            ),
            "the hook's status was dropped with its connection"
        );

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reads_concatenated_client_frames_from_one_reader() {
        let (mut writer, stream) = UnixStream::pair().unwrap();
        let mut frames = serde_json::to_vec(&ClientMessage::Input {
            bytes: b"first".to_vec(),
            pane_id: Some(1),
            perf_id: None,
        })
        .unwrap();
        frames.push(b'\n');
        frames.extend_from_slice(
            &serde_json::to_vec(&ClientMessage::Detach { generation: 7 }).unwrap(),
        );
        frames.push(b'\n');
        writer.write_all(&frames).unwrap();

        let mut reader = BufReader::new(stream);
        assert!(matches!(
            read_message(&mut reader).unwrap(),
            Some(ClientMessage::Input { bytes, .. }) if bytes == b"first"
        ));
        assert!(matches!(
            read_message(&mut reader).unwrap(),
            Some(ClientMessage::Detach { generation: 7 })
        ));
    }

    #[test]
    fn socket_loop_uses_short_attached_backoff() {
        assert_eq!(socket_loop_backoff(true, true, Duration::ZERO), None);
        assert_eq!(
            socket_loop_backoff(true, false, Duration::ZERO),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            socket_loop_backoff(false, false, Duration::ZERO),
            Some(Duration::from_millis(16))
        );
        assert!(FIRST_MESSAGE_TIMEOUT <= Duration::from_millis(5));
    }

    #[test]
    fn a_detached_node_slows_down_only_after_a_quiet_spell() {
        // Still serving remote guests, so recent work keeps it at the fast rate.
        assert_eq!(
            socket_loop_backoff(false, true, DETACHED_QUIET_AFTER * 10),
            Some(DETACHED_IDLE_BACKOFF)
        );
        assert_eq!(
            socket_loop_backoff(
                false,
                false,
                DETACHED_QUIET_AFTER - Duration::from_millis(1)
            ),
            Some(DETACHED_IDLE_BACKOFF)
        );
        assert_eq!(
            socket_loop_backoff(false, false, DETACHED_QUIET_AFTER),
            Some(DETACHED_QUIET_BACKOFF)
        );
        // An attached client is never slowed, however long it has sat idle.
        assert_eq!(
            socket_loop_backoff(true, false, DETACHED_QUIET_AFTER * 10),
            Some(ATTACHED_IDLE_BACKOFF)
        );
    }

    #[test]
    fn periodic_drain_is_paced_to_the_publish_cadence() {
        let now = Instant::now();
        assert!(periodic_drain_due(None, now));
        assert!(!periodic_drain_due(
            Some(now),
            now + PERIODIC_DRAIN_INTERVAL - Duration::from_millis(1),
        ));
        assert!(periodic_drain_due(Some(now), now + PERIODIC_DRAIN_INTERVAL));
        // Draining faster than the client can be sent frames is wasted parse/diff work.
        assert!(PERIODIC_DRAIN_INTERVAL >= TARGET_SCREEN_PUBLISH_INTERVAL);
    }

    #[test]
    fn a_focus_request_is_answered_even_when_it_asks_for_the_focus_we_have() {
        let mut publish = AttachmentPublishState {
            focus: Some((1, 2)),
            ..Default::default()
        };
        // Nothing moved, so ordinarily there is nothing to say.
        assert!(!publish.focus_due((1, 2)));
        // A client that asked for this pane is holding its own optimistic focus
        // until it hears the node agree. Left unanswered it holds it forever and
        // rejects every later focus, so the answer goes out even though the focus
        // did not change.
        publish.reannounce_focus();
        assert!(publish.focus_due((1, 2)));
    }

    #[test]
    fn screen_publishes_are_throttled_but_structural_and_resync_are_immediate() {
        let now = Instant::now();
        let mut publish = AttachmentPublishState {
            last_screen_publish: Some(now),
            ..Default::default()
        };
        assert!(!screens_due(
            &publish,
            false,
            now + SCREEN_PUBLISH_INTERVAL - Duration::from_millis(1),
            &[],
        ));
        assert!(screens_due(
            &publish,
            false,
            now + SCREEN_PUBLISH_INTERVAL,
            &[],
        ));
        assert!(screens_due(
            &publish,
            true,
            now + Duration::from_millis(1),
            &[]
        ));
        publish.force_screens = true;
        assert!(screens_due(
            &publish,
            false,
            now + Duration::from_millis(1),
            &[]
        ));
    }

    #[test]
    fn focused_input_urgency_beats_background_screen_throttle_only_for_target() {
        let now = Instant::now();
        let mut publish = AttachmentPublishState {
            last_screen_publish: Some(now),
            ..Default::default()
        };
        let target = vec![PaneScreenSnapshot {
            pane_id: 7,
            state: ScreenUpdate::Unchanged {
                sequence: 1,
                kitty_keyboard_active: false,
            },
            reset_outer: false,
            history_len: 0,
            history_end: 0,
        }];
        publish.arm_target_urgency(7, now);
        assert!(screens_due(
            &publish,
            false,
            now + TARGET_SCREEN_PUBLISH_INTERVAL,
            &target,
        ));
        assert!(!screens_due(
            &publish,
            false,
            now + TARGET_SCREEN_PUBLISH_INTERVAL,
            &[],
        ));
    }

    #[test]
    fn full_outbound_queue_coalesces_screens_and_drops_scrollback() {
        let mut state = OutboundState {
            messages: VecDeque::with_capacity(OUTBOUND_QUEUE),
            writing: false,
            closed: false,
        };
        state
            .messages
            .extend(std::iter::repeat_n(NodeMessage::ProbeAck, OUTBOUND_QUEUE));
        assert_eq!(
            push_outbound(
                &mut state,
                NodeMessage::Screens {
                    screens: vec![],
                    perf_id: None,
                },
                false,
            ),
            QueueResult::CoalesceScreens,
        );
        assert_eq!(
            push_outbound(
                &mut state,
                NodeMessage::ScrollbackWindow {
                    pane_id: 1,
                    request_id: 1,
                    history_id: 1,
                    total_rows: 0,
                    offset: 0,
                    snapshot: None,
                    unavailable: None,
                },
                false,
            ),
            QueueResult::Dropped,
        );
        assert_eq!(state.messages.len(), OUTBOUND_QUEUE);
    }

    #[test]
    fn local_screen_updates_use_snapshot_delta_and_unchanged() {
        let mut screen = HostScreen::new(1, 3).unwrap();
        let mut sequences = BTreeMap::new();
        let local = |screen: &HostScreen| crate::tui::NodeScreenSnapshot::Local {
            frame: screen.current_frame().clone(),
            history_len: screen.history_metadata().0,
            history_end: screen.history_metadata().1,
        };
        let initial = BTreeMap::from([(1, local(&screen))]);
        let updates = pane_screen_updates(initial, &sequences, |_| None);
        assert!(matches!(updates[0].state, ScreenUpdate::Snapshot { .. }));
        update_screen_sequences(&mut sequences, &updates);

        screen.process_pty(b"a").unwrap();
        let changed = BTreeMap::from([(1, local(&screen))]);
        let updates = pane_screen_updates(changed, &sequences, |_| None);
        assert!(matches!(updates[0].state, ScreenUpdate::Delta { .. }));
        update_screen_sequences(&mut sequences, &updates);

        let unchanged = BTreeMap::from([(1, local(&screen))]);
        let updates = pane_screen_updates(unchanged, &sequences, |_| None);
        assert!(updates.is_empty());

        screen.resize(2, 3).unwrap();
        let resized = BTreeMap::from([(1, local(&screen))]);
        let updates = pane_screen_updates(resized, &sequences, |_| None);
        assert!(matches!(updates[0].state, ScreenUpdate::Snapshot { .. }));
    }

    #[test]
    fn local_screen_updates_publish_metadata_without_rows() {
        let screen = HostScreen::new(1, 3).unwrap();
        let mut sequences = BTreeMap::new();
        let initial = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
                history_len: 3,
                history_end: 7,
            },
        )]);
        let updates = pane_screen_updates(initial, &sequences, |_| None);
        assert_eq!((updates[0].history_len, updates[0].history_end), (3, 7));
        update_screen_sequences(&mut sequences, &updates);

        let unchanged = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
                history_len: 3,
                history_end: 7,
            },
        )]);
        let updates = pane_screen_updates(unchanged, &sequences, |_| None);
        assert!(updates.is_empty());
    }

    #[test]
    fn frozen_scrollback_viewport_uses_the_host_screen_codec() {
        let mut host = HostScreen::new(2, 6).unwrap();
        host.process_pty(b"one\r\ntwo\r\nthree\r\nfour").unwrap();
        let (total_rows, _) = host.history_metadata();
        let mut frozen = FrozenScrollback {
            pane_id: 1,
            total_rows,
            screen: host.screen().clone(),
        };
        let (_, payload) = frozen.viewport(1);
        let mut guest = crate::screen::GuestScreen::new();
        guest.apply_snapshot(1, &payload).unwrap();
        let mut expected = host.screen().clone();
        expected.set_scrollback(1);
        assert_eq!(
            guest.screen().unwrap().state_formatted(),
            expected.state_formatted()
        );
    }

    /// `viewport` moves the frozen screen's own offset instead of cloning it, so the
    /// session has to stay reusable: a wheel burst asks the same `FrozenScrollback`
    /// for many offsets, out of order, and every answer must still match a screen
    /// parked at that offset.
    #[test]
    fn frozen_scrollback_viewport_is_reusable_across_offsets() {
        let mut host = HostScreen::new(2, 6).unwrap();
        host.process_pty(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix")
            .unwrap();
        let (total_rows, _) = host.history_metadata();
        assert!(
            total_rows >= 3,
            "expected retained history, got {total_rows}"
        );
        let mut frozen = FrozenScrollback {
            pane_id: 1,
            total_rows,
            screen: host.screen().clone(),
        };
        let expected = |offset: usize| {
            let mut screen = host.screen().clone();
            screen.set_scrollback(offset);
            screen.state_formatted()
        };
        // Ascend, descend, then repeat an offset already served.
        for offset in [1_u64, 2, 3, 2, 0, 3, 3] {
            let (rows, payload) = frozen.viewport(offset);
            assert_eq!(rows, total_rows);
            let mut guest = crate::screen::GuestScreen::new();
            guest.apply_snapshot(1, &payload).unwrap();
            assert_eq!(
                guest.screen().unwrap().state_formatted(),
                expected(offset as usize),
                "viewport at offset {offset} drifted"
            );
        }
        // An offset past the retained history clamps rather than wrapping.
        let (_, clamped) = frozen.viewport(total_rows + 50);
        let mut guest = crate::screen::GuestScreen::new();
        guest.apply_snapshot(1, &clamped).unwrap();
        assert_eq!(
            guest.screen().unwrap().state_formatted(),
            expected(total_rows as usize)
        );
    }

    #[test]
    fn unchanged_remote_screen_updates_skip_snapshot_encoding() {
        let mut sequences = BTreeMap::new();
        let calls = Cell::new(0);
        let remote = || crate::tui::NodeScreenSnapshot::Remote {
            sequence: 1,
            kitty_keyboard_active: false,
        };
        let initial = BTreeMap::from([(1, remote())]);
        let updates = pane_screen_updates(initial, &sequences, |_| {
            calls.set(calls.get() + 1);
            Some(vec![1])
        });
        assert!(matches!(updates[0].state, ScreenUpdate::Snapshot { .. }));
        assert_eq!(calls.get(), 1);
        update_screen_sequences(&mut sequences, &updates);

        let unchanged = BTreeMap::from([(1, remote())]);
        let updates = pane_screen_updates(unchanged, &sequences, |_| {
            panic!("unchanged remote panes must not encode a snapshot")
        });
        assert!(updates.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_reads_runtime_state_and_input_lands_on_the_pane_the_client_named() {
        let host = SharedLayoutHost::new(HostSession::create().await.unwrap(), 2, 8).unwrap();
        let panes = host.pane_server();
        let host_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let initial = SharedLocalPane::spawn(1, 2, 8, host_peer_id.clone()).unwrap();
        panes
            .register_local_pane(
                crate::protocol::PaneDescriptor {
                    pane_id: 1,
                    host_peer_id: host_peer_id.clone(),
                    grid_rows: 2,
                    grid_cols: 8,
                    title: None,
                    locked: false,
                    exited: false,
                },
                initial.channels(),
            )
            .unwrap();
        let state = host.session_snapshot().unwrap().state.unwrap();
        let mut snapshot = layout_snapshot_from_state(&state).unwrap();
        let first = snapshot.tabs[0].root.clone();
        snapshot.tabs[0].root = Node::Split {
            axis: Axis::LeftRight,
            first_share_bps: 5_000,
            first: Box::new(first),
            second: Box::new(Node::Leaf { pane_id: 2 }),
        };
        snapshot.panes.insert(
            2,
            Pane {
                pane_id: 2,
                host_peer_id,
                locked: false,
                exited: false,
                grid_rows: 2,
                grid_cols: 8,
                title: None,
            },
        );
        let mut node = SharedLayoutNode::new(
            SharedLayoutRuntime::host(
                host,
                panes,
                snapshot,
                initial,
                String::from("TESTCODE"),
                None,
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );

        node.focus(1, 2).unwrap();

        assert_eq!(node.local_focus(), (1, 2));

        // Input goes where the client aimed it, not where this node happens to
        // be looking -- the two disagree for one round trip after every new
        // pane, and that is exactly when a mouse report encoded for one pane
        // would otherwise be typed into another.
        assert_eq!(
            node.input(Some(1), b"\x1b[<35;1;1M".to_vec()).unwrap(),
            Some(1)
        );
        // A client too old to name a pane still gets the node's focus.
        assert_eq!(node.input(None, b"x".to_vec()).unwrap(), Some(2));

        tokio::task::block_in_place(|| node.shutdown());
    }

    /// The failure that reads as a network problem and is not one.
    ///
    /// A droplet on an older p2pmux, dialling a session on a newer one, was told
    /// "could not reach the session host: they may be offline or on a different
    /// network" — while both machines were up, on the same network, and reaching
    /// each other fine. The peer answered; it answered in a protocol this one
    /// does not speak, and that is a diagnosis, not a reachability failure.
    #[test]
    fn a_version_mismatch_is_not_reported_as_an_unreachable_host() {
        use crate::{protocol::ProtocolError, transport::TransportError};

        let older = describe_join_failure(crate::session::SessionError::Transport(
            TransportError::Protocol(ProtocolError::UnsupportedVersion(
                crate::protocol::PROTOCOL_VERSION - 1,
            )),
        ))
        .to_string();
        assert!(
            !older.contains("could not reach"),
            "a version mismatch must not be dressed up as a network failure: {older}"
        );
        assert!(older.contains("p2pmux.com/install.sh"), "{older}");
        assert!(
            older.contains("upgrade it, not this machine"),
            "the older end is the one to upgrade, and it is the other one here: {older}"
        );

        let newer = describe_join_failure(crate::session::SessionError::Transport(
            TransportError::Protocol(ProtocolError::UnsupportedVersion(
                crate::protocol::PROTOCOL_VERSION + 1,
            )),
        ))
        .to_string();
        assert!(
            newer.contains("upgrade this one"),
            "here it is this machine that is behind: {newer}"
        );

        // Everything else about a transport is still a reachability question,
        // and burying a genuine one under a version story would be the same
        // mistake pointing the other way.
        let unreachable =
            describe_join_failure(crate::session::SessionError::TimedOut("join")).to_string();
        assert!(unreachable.contains("could not reach"), "{unreachable}");
    }

    /// A machine that followed a fleet invitation runs a node nobody has ever
    /// attached to. It used to announce that it was watching whatever pane its
    /// layout started on — so a droplet that joined on its own put a member dot
    /// on the tab bar and lit a pane border as watched, on a laptop, for a
    /// session no human had opened there. Presence has to be about a terminal
    /// somebody is sitting at, and this is where that is decided.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_with_no_terminal_on_it_is_looking_at_nothing() {
        let host = SharedLayoutHost::new(HostSession::create().await.unwrap(), 2, 8).unwrap();
        let panes = host.pane_server();
        let host_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let initial = SharedLocalPane::spawn(1, 2, 8, host_peer_id.clone()).unwrap();
        panes
            .register_local_pane(
                crate::protocol::PaneDescriptor {
                    pane_id: 1,
                    host_peer_id,
                    grid_rows: 2,
                    grid_cols: 8,
                    title: None,
                    locked: false,
                    exited: false,
                },
                initial.channels(),
            )
            .unwrap();
        let state = host.session_snapshot().unwrap().state.unwrap();
        let snapshot = layout_snapshot_from_state(&state).unwrap();
        let mut node = SharedLayoutNode::new(
            SharedLayoutRuntime::host(
                host,
                panes,
                snapshot,
                initial,
                String::from("TESTCODE"),
                None,
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );

        // Focus moving is what publishes presence, and on an unattended node it
        // moves for reasons that have nothing to do with a person: a pane
        // opening, a layout arriving from the coordinator.
        node.focus(1, 1).unwrap();
        let unattended = node.local_presence();
        assert!(
            unattended
                .as_ref()
                .is_none_or(|presence| !presence.attached),
            "a node nobody attached to claimed to be attached: {unattended:?}"
        );
        assert!(
            unattended
                .as_ref()
                .is_none_or(|presence| presence.pane_id == 0 && presence.tab_id == 0),
            "a node nobody attached to claimed a location: {unattended:?}"
        );

        // Somebody opens it. Now there is a person, and a pane they are on.
        node.runtime.set_client_attached(true);
        let attached = node.local_presence().expect("attaching publishes presence");
        assert!(attached.attached, "{attached:?}");
        assert_eq!((attached.tab_id, attached.pane_id), (1, 1), "{attached:?}");

        // And when they detach, the panes stay up but the dot goes away.
        node.runtime.set_client_attached(false);
        let detached = node.local_presence().expect("detaching publishes presence");
        assert!(!detached.attached, "{detached:?}");
        assert_eq!((detached.tab_id, detached.pane_id), (0, 0), "{detached:?}");
        assert!(
            detached.generation > attached.generation,
            "a later presence must supersede the one before it: {detached:?}"
        );

        tokio::task::block_in_place(|| node.shutdown());
    }

    #[test]
    fn large_snapshot_survives_nonblocking_listener_accept() {
        let path = std::path::PathBuf::from(format!(
            "/tmp/p2pmux-large-snapshot-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let stream = UnixStream::connect(client_path).unwrap();
            let mut reader = BufReader::new(stream);
            crate::client::read_message(&mut reader).unwrap().unwrap()
        });

        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::yield_now(),
                Err(error) => panic!("failed to accept local IPC client: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        write_message(
            &mut stream,
            &NodeMessage::Snapshot {
                room_name: "test".into(),
                role: "coordinator".into(),
                summary: SessionSummary::default(),
                layout: Box::new(crate::layout::LayoutSnapshot {
                    revision: 0,
                    members: vec![],
                    tabs: vec![],
                    panes: Default::default(),
                }),
                screens: vec![PaneScreenSnapshot {
                    pane_id: 1,
                    state: ScreenUpdate::Snapshot {
                        sequence: 1,
                        snapshot: vec![b'x'; 256 * 1024],
                        kitty_keyboard_active: false,
                    },
                    reset_outer: false,
                    history_len: 0,
                    history_end: 0,
                }],
                leases: vec![],
                rosters: vec![],
                presence: vec![],
                local_peer_id: vec![],
                tab_id: 1,
                pane_id: 1,
                ticket: None,
                code: None,
            },
        )
        .unwrap();

        let NodeMessage::Snapshot { screens, .. } = client.join().unwrap() else {
            panic!("expected snapshot");
        };
        assert!(matches!(
            &screens[0].state,
            ScreenUpdate::Snapshot { snapshot, .. } if snapshot == &vec![b'x'; 256 * 1024]
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bootstrap_is_private_and_round_trips() {
        let path = std::env::temp_dir().join(format!("p2pmux-bootstrap-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let descriptor = SessionDescriptor::new(
            "0123456789abcdef0123456789abcdef".into(),
            "lisbon".into(),
            "/tmp/p2pmux-test.sock".into(),
            1,
            SessionRole::Coordinator,
        );
        write_bootstrap(
            &path,
            &NodeBootstrap {
                descriptor: descriptor.clone(),
                kind: NodeBootstrapKind::Create {
                    display_name: "A".into(),
                    cols: 80,
                    rows: 24,
                },
                tether: Tether::Detached,
                supervisor: None,
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read_bootstrap(&path).unwrap().descriptor, descriptor);
        assert!(!path.exists());
    }

    #[test]
    fn old_bootstrap_defaults_to_a_detached_lifecycle() {
        let bootstrap: NodeBootstrap = serde_json::from_value(serde_json::json!({
            "descriptor": SessionDescriptor::new(
                "0123456789abcdef0123456789abcdef".into(),
                "lisbon".into(),
                "/tmp/p2pmux-test.sock".into(),
                1,
                SessionRole::Coordinator,
            ),
            "kind": { "Create": { "display_name": "A", "cols": 80, "rows": 24 } }
        }))
        .expect("an old bootstrap parses");
        assert_eq!(bootstrap.tether, Tether::Detached);
    }

    #[test]
    fn old_join_bootstrap_keeps_the_thirty_second_connect_timeout() {
        let kind: NodeBootstrapKind = serde_json::from_value(serde_json::json!({
            "Join": {
                "ticket": "ticket",
                "display_name": "A",
                "cols": 80,
                "rows": 24
            }
        }))
        .expect("old bootstrap parses");

        assert!(matches!(
            kind,
            NodeBootstrapKind::Join {
                connect_timeout_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn fleet_invites_keep_the_default_join_timeout() {
        let source = include_str!("node.rs");
        let invite = source
            .split_once("pub fn follow_fleet_invite")
            .expect("invite launcher")
            .1
            .split_once("pub fn write_bootstrap")
            .expect("next function")
            .0;

        assert!(invite.contains("connect_timeout_ms: None"));
    }

    /// Issue #107: a machine that was switched off must not decide it is still
    /// in its home session.
    ///
    /// Asserted against the source in the style of the test above, because the
    /// function it guards reaches the real session store and the real process
    /// table, and the bug is precisely that a record and a running node are not
    /// the same thing. The behaviour itself is covered on two real machines by
    /// `scripts/e2e/scenario_am_trusted_autojoin.py`, where the check without
    /// this line fails and with it passes.
    #[test]
    fn already_being_in_a_session_requires_the_node_to_still_be_running() {
        let source = include_str!("node.rs");
        let invite = source
            .split_once("pub fn follow_fleet_invite")
            .expect("invite launcher")
            .1
            .split_once("pub fn write_bootstrap")
            .expect("next function")
            .0;

        assert!(
            invite.contains("node_process_is_alive(session.node_pid)"),
            "a recorded session only proves membership while its node is alive; \
             without this a rebooted machine never rejoins and so never hears \
             about any session started while it was away"
        );
    }

    #[test]
    fn remote_history_selection_is_unavailable_instead_of_blank_lines() {
        assert!(matches!(
            selection_copy_reply(9, Err(())),
            NodeMessage::SelectionCopy {
                request_id: 9,
                text: None,
                unavailable: Some(reason),
            } if reason == SCROLLBACK_NOT_OURS
        ));
    }
}
