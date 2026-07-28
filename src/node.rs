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
    local_ipc::{
        AgentOverlaySnapshotRow, AttachmentGate, ClientMessage, NodeMessage, PaneLeaseSnapshot,
        PaneScreenSnapshot, ScreenUpdate, SessionSummary,
    },
    rendezvous::LocalRendezvous,
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
const SCREEN_PUBLISH_INTERVAL: Duration = Duration::from_millis(16);
const TARGET_SCREEN_PUBLISH_INTERVAL: Duration = Duration::from_millis(8);
const TARGET_SCREEN_URGENCY_TTL: Duration = Duration::from_millis(64);
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

/// Private child entrypoint. It owns the descriptor, rendezvous record, socket and every PTY.
pub async fn run_background(bootstrap: NodeBootstrap) -> Result<(), Box<dyn Error>> {
    let mut descriptor = bootstrap.descriptor.clone();
    descriptor.node_pid = std::process::id();
    let (mut node, dispatcher_task, rendezvous) = match bootstrap.kind {
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
            let rendezvous = LocalRendezvous::for_current_user()?.publish(host.ticket())?;
            let join_code = rendezvous.code().to_owned();
            let session_id = host.ticket().session_id().to_vec();
            let handle = tokio::runtime::Handle::current();
            let mut runtime = crate::tui::SharedLayoutRuntime::host(
                host, panes, layout, initial, join_code, handle,
            )?;
            runtime.set_session_id(session_id);
            (
                SharedLayoutNode::new(runtime),
                dispatcher_task,
                Some(rendezvous),
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
            let mut member =
                join_layout_with_display_name(transport, ticket.clone(), display_name).await?;
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
    if let Some(rendezvous) = rendezvous {
        let _ = rendezvous.remove();
    }
    let _ = fs::remove_file(&descriptor.socket_path);
    let _ = store.remove(&descriptor.id);
    result.map_err(Into::into)
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
    loop {
        let mut shutdown = false;
        let mut did_work = false;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(FIRST_MESSAGE_TIMEOUT))?;
                    // Prevent stalled clients from wedging the node on large Snapshot writes.
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let mut reader = BufReader::new(stream);
                    match read_message(&mut reader) {
                        // Probes and shutdowns are control requests. They must not consume or
                        // contend with the single interactive attachment slot.
                        Ok(Some(ClientMessage::Probe)) => {
                            let _ = write_message(reader.get_mut(), &NodeMessage::ProbeAck);
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
        let mut changed = node
            .drain()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut drain_elapsed = drain_started.elapsed();
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
                            .get(&history_id)
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
                            let frozen = FrozenScrollback {
                                pane_id,
                                total_rows: window.total_rows,
                                screen: window.screen,
                            };
                            let viewport = frozen.viewport(offset);
                            frozen_scrollback.insert(history_id, frozen);
                            viewport
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
        match socket_loop_backoff(client.is_some(), did_work) {
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
    fn viewport(&self, offset: u64) -> (u64, Vec<u8>) {
        let mut screen = self.screen.clone();
        screen.set_scrollback(offset.min(self.total_rows) as usize);
        let snapshot = crate::screen::snapshot_payload(&screen)
            .expect("host scrollback viewport must fit the screen codec")
            .as_ref()
            .to_vec();
        (self.total_rows, snapshot)
    }
}

fn socket_loop_backoff(client_attached: bool, did_work: bool) -> Option<Duration> {
    match (client_attached, did_work) {
        (true, true) => None,
        (true, false) => Some(ATTACHED_IDLE_BACKOFF),
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
        local_peer_id,
        tab_id,
        pane_id,
        join_code: node.runtime.join_code().map(str::to_owned),
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
    focus: Option<(u64, u64)>,
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
        self.focus = None;
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
    pub(crate) fn status(&self) -> String {
        self.runtime.status().to_owned()
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
        assert_eq!(socket_loop_backoff(true, true), None);
        assert_eq!(
            socket_loop_backoff(true, false),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            socket_loop_backoff(false, false),
            Some(Duration::from_millis(16))
        );
        assert!(FIRST_MESSAGE_TIMEOUT <= Duration::from_millis(5));
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
        let frozen = FrozenScrollback {
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
                local_peer_id: vec![],
                tab_id: 1,
                pane_id: 1,
                join_code: None,
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
