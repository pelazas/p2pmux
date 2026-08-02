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
        HostSession, LayoutControlEvent, SharedLayoutHost, join_layout_with_display_name,
        layout_snapshot_from_state,
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
const MAX_FROZEN_SCROLLBACK_SESSIONS: usize = 8;
const OUTBOUND_QUEUE: usize = 64;

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
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeBootstrap {
    pub descriptor: SessionDescriptor,
    pub kind: NodeBootstrapKind,
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
    match error {
        SessionError::Transport(_) | SessionError::TimedOut(_) => io::Error::other(
            "could not reach the session host: they may be offline or on a different network, \
             or the invite may be out of date. Ask for a fresh join code.",
        )
        .into(),
        other => Box::<dyn Error>::from(other.to_string()),
    }
}

/// Private child entrypoint. It owns the descriptor, socket and every PTY.
pub async fn run_background(bootstrap: NodeBootstrap) -> Result<(), Box<dyn Error>> {
    let mut descriptor = bootstrap.descriptor.clone();
    descriptor.node_pid = std::process::id();
    // Before the first pane spawns: every PTY this node opens inherits the
    // socket path from here, and pane 1 is created a few lines below.
    crate::pty_host::set_agent_socket_path(descriptor.socket_path.clone());
    let (mut node, dispatcher_task, published_code) = match bootstrap.kind {
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
            (
                SharedLayoutNode::new(runtime),
                dispatcher_task,
                published_code,
            )
        }
        NodeBootstrapKind::Join {
            ticket,
            display_name,
            cols: _,
            rows: _,
        } => {
            let ticket = ticket
                .parse::<JoinTicket>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid ticket"))?;
            let transport = Transport::bind().await?;
            let mut member = join_layout_with_display_name(transport, ticket.clone(), display_name)
                .await
                .map_err(describe_join_failure)?;
            let state = match member.events.recv().await {
                Some(LayoutControlEvent::Snapshot(snapshot)) => {
                    snapshot.state.ok_or("missing layout snapshot")?
                }
                _ => {
                    return Err(
                        io::Error::other("layout coordinator disconnected during join").into(),
                    );
                }
            };
            let live_names = SessionStore::for_current_user()?
                .list_live()?
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
            let runtime = crate::tui::SharedLayoutRuntime::member_from_state(
                member,
                panes,
                ticket.session_id().to_vec(),
                state,
                tokio::runtime::Handle::current(),
            )?;
            (SharedLayoutNode::new(runtime), dispatcher_task, None)
        }
    };
    let store = SessionStore::for_current_user()?;
    let _ = fs::remove_file(&descriptor.socket_path);
    let listener = UnixListener::bind(&descriptor.socket_path)?;
    listener.set_nonblocking(true)?;
    store.write(&descriptor)?;
    let result = run_socket_loop(&mut node, listener, &descriptor);
    // `SharedLayoutRuntime` owns a Tokio handle for its asynchronous pane/control cleanup.
    // The node itself runs on this runtime, so perform that blocking teardown outside its worker.
    tokio::task::block_in_place(|| node.shutdown());
    dispatcher_task.abort();
    let _ = dispatcher_task.await;
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
    descriptor: &SessionDescriptor,
) -> io::Result<()> {
    let gate = AttachmentGate::default();
    let mut client: Option<AttachedClient> = None;
    let mut frozen_scrollback = BTreeMap::<u64, FrozenScrollback>::new();
    let mut next_history_id = 1_u64;
    let mut last_periodic_drain: Option<Instant> = None;
    let mut last_work = Instant::now();
    loop {
        let mut shutdown = false;
        let mut did_work = false;
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
                            if let Ok(generation) = gate.attach() {
                                node.resize(cols, rows)
                                    .map_err(|error| io::Error::other(error.to_string()))?;
                                let mut publish = AttachmentPublishState::default();
                                match write_message(
                                    reader.get_mut(),
                                    &NodeMessage::AttachAccepted { generation },
                                )
                                .and_then(|()| {
                                    write_snapshot(
                                        reader.get_mut(),
                                        descriptor,
                                        node,
                                        &mut publish,
                                        Duration::ZERO,
                                    )
                                }) {
                                    Ok(()) => {
                                        reader
                                            .get_mut()
                                            .set_read_timeout(Some(Duration::from_millis(1)))?;
                                        let writer =
                                            AttachmentWriter::start(reader.get_mut().try_clone()?)?;
                                        client = Some(AttachedClient {
                                            reader,
                                            generation,
                                            publish,
                                            writer,
                                            close_after_ack: false,
                                            shutdown_after_ack: false,
                                        });
                                        did_work = true;
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "p2pmux node: failed to write initial local snapshot: {error}"
                                        );
                                        let _ = gate.detach(generation);
                                    }
                                }
                            } else {
                                let _ = write_message(
                                    reader.get_mut(),
                                    &NodeMessage::AttachRejected {
                                        reason: "already attached".into(),
                                    },
                                );
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
                Err(error) => return Err(error),
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
        did_work |= changed;
        let mut detached = false;
        let mut full_snapshot = false;
        if !shutdown && let Some(client) = client.as_mut() {
            match read_message(&mut client.reader) {
                Ok(Some(ClientMessage::Input { bytes, perf_id })) => {
                    let focused_pane = node.local_focus().1;
                    node.input(bytes)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    client
                        .publish
                        .arm_target_urgency(focused_pane, Instant::now());
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
                    let (history_id, result) = if let Some(history_id) = history_id {
                        let result = frozen_scrollback
                            .get_mut(&history_id)
                            .filter(|frozen| frozen.pane_id == pane_id)
                            .map(|frozen| frozen.viewport(offset));
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
                        let result = node.node_local_scrollback(pane_id).map(|window| {
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
                            frozen_scrollback
                                .get_mut(&history_id)
                                .expect("the frozen session was just inserted")
                                .viewport(offset)
                        });
                        (history_id, result)
                    };
                    let (total_rows, snapshot, unavailable) = match result {
                        Some((total_rows, snapshot)) => (total_rows, Some(snapshot), None),
                        None => (
                            0,
                            None,
                            Some(String::from(
                                "local scrollback is unavailable for this pane (remote, alternate screen, or stale history)",
                            )),
                        ),
                    };
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
                Ok(Some(ClientMessage::Focus { tab_id, pane_id })) => {
                    node.focus(tab_id, pane_id)
                        .map_err(|error| io::Error::other(error.to_string()))?;
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
        role: match descriptor.role {
            SessionRole::Coordinator => "coordinator",
            SessionRole::Member => "member",
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
    if publish.focus != Some(focus) {
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
                        Some((frame.sequence, state, history_len, history_end))
                    } else {
                        let state = ScreenUpdate::Snapshot {
                            sequence: frame.sequence,
                            snapshot: frame.snapshot.as_ref().to_vec(),
                            kitty_keyboard_active: frame.kitty_keyboard_active,
                        };
                        Some((frame.sequence, state, history_len, history_end))
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
                        ))
                    }
                }
            };
            let (_, state, history_len, history_end) = update?;
            Some(PaneScreenSnapshot {
                pane_id,
                state,
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

    pub fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        self.runtime.node_input(bytes)
    }
    pub fn local_peer_id(&self) -> Vec<u8> {
        self.runtime.local_peer_id()
    }
    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> {
        self.runtime.release_all_local_control()
    }
    pub fn local_focus(&self) -> (u64, u64) {
        self.runtime.local_focus()
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
    pub(crate) fn node_local_scrollback(
        &self,
        pane_id: u64,
    ) -> Option<crate::tui::LocalScrollbackWindow> {
        self.runtime.node_local_scrollback(pane_id)
    }
    pub(crate) fn node_remote_snapshot(&self, pane_id: u64) -> Option<Vec<u8>> {
        self.runtime.node_remote_snapshot(pane_id)
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), Box<dyn Error>> {
        self.runtime.node_resize(cols, rows)
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
    pub fn shutdown(self) {
        self.runtime.shutdown_node();
    }
}

#[cfg(test)]
mod tests {
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
    async fn local_focus_reads_runtime_state_without_drain() {
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
}
