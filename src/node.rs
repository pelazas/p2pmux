//! Headless owner of a shared layout.
//!
//! `SharedLayoutRuntime` remains the temporary foreground adapter during the split.  The node
//! owns it without terminal I/O and exposes only node operations; the local socket client is the
//! only component allowed to render a terminal.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::{
        fs::OpenOptionsExt,
        net::{UnixListener, UnixStream},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::{
    local_ipc::{
        AgentOverlaySnapshotRow, AttachmentGate, ClientMessage, LocalHistorySnapshot, NodeMessage,
        PaneLeaseSnapshot, PaneScreenSnapshot, ScreenUpdate, SessionSummary,
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
                HostSession::create().await?,
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
    let mut client: Option<(BufReader<UnixStream>, u64, AttachmentPublishState)> = None;
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
                                        client = Some((reader, generation, publish));
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
        if !shutdown && let Some((reader, generation, publish)) = client.as_mut() {
            match read_message(reader) {
                Ok(Some(ClientMessage::Input { bytes })) => {
                    node.input(bytes)
                        .map_err(|error| io::Error::other(error.to_string()))?;
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
                    changed = true;
                }
                Ok(Some(ClientMessage::ResyncScreen { pane_id })) => {
                    publish.screen_sequences.remove(&pane_id);
                    changed = true;
                }
                Ok(Some(ClientMessage::Focus { tab_id, pane_id })) => {
                    node.focus(tab_id, pane_id)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    changed = true;
                }
                Ok(Some(ClientMessage::Detach {
                    generation: requested,
                })) if requested == *generation => {
                    node.release_all_local_control()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    let _ = write_message(
                        reader.get_mut(),
                        &NodeMessage::DetachAck {
                            generation: *generation,
                        },
                    );
                    detached = true;
                }
                Ok(Some(ClientMessage::Shutdown {
                    generation: requested,
                })) if requested == *generation => {
                    write_message(
                        reader.get_mut(),
                        &NodeMessage::ShutdownAck {
                            generation: *generation,
                        },
                    )?;
                    shutdown = true;
                }
                Ok(Some(ClientMessage::Rename { name })) => {
                    if crate::session_store::valid_name(&name) {
                        write_message(
                            reader.get_mut(),
                            &NodeMessage::Update {
                                state: serde_json::json!({"name": name}),
                            },
                        )?;
                    } else {
                        write_message(
                            reader.get_mut(),
                            &NodeMessage::Error {
                                message: "invalid session name".into(),
                            },
                        )?;
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
            if !detached {
                match write_updates(reader.get_mut(), node, publish, drain_elapsed) {
                    Ok(published) => did_work |= published,
                    Err(error) => {
                        eprintln!("p2pmux node: failed to write local update: {error}");
                        detached = true;
                    }
                }
            }
        }
        if !changed {
            log_slow_drain(drain_elapsed);
        }
        if (detached || shutdown)
            && let Some((mut reader, generation, _)) = client.take()
        {
            let _ = reader.get_mut().shutdown(Shutdown::Both);
            let _ = gate.detach(generation);
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
fn write_snapshot(
    stream: &mut UnixStream,
    descriptor: &SessionDescriptor,
    node: &SharedLayoutNode,
    publish: &mut AttachmentPublishState,
    drain_elapsed: Duration,
) -> io::Result<()> {
    let snapshot_started = Instant::now();
    let (tab_id, pane_id) = node.local_focus();
    let local_peer_id = node.local_peer_id();
    let (layout, screens, leases, rosters) = node.snapshot();
    let snapshot_build = snapshot_started.elapsed();
    let local_panes = screens
        .values()
        .filter(|screen| matches!(screen, crate::tui::NodeScreenSnapshot::Local { .. }))
        .count();
    let remote_panes = screens.len().saturating_sub(local_panes);
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
    let mut history_extract = Duration::ZERO;
    let screens = pane_screen_updates(
        screens,
        &publish.screen_sequences,
        |pane_id| {
            let started = Instant::now();
            let history =
                node.node_local_history(pane_id)
                    .map(|(total_rows, rows)| LocalHistorySnapshot {
                        total_rows,
                        rows: rows.into_iter().map(|row| STANDARD.encode(row)).collect(),
                    });
            history_extract += started.elapsed();
            history
        },
        |pane_id| node.node_remote_snapshot(pane_id),
    );
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
    };
    let json_started = Instant::now();
    let mut frame = serde_json::to_vec(&message).map_err(io::Error::other)?;
    frame.push(b'\n');
    let json_serialize = json_started.elapsed();
    let write_started = Instant::now();
    stream.write_all(&frame)?;
    stream.flush()?;
    let json_write = write_started.elapsed();
    publish.layout = Some(layout.clone());
    publish.leases = Some(leases);
    publish.rosters = Some(rosters);
    publish.focus = Some((tab_id, pane_id));
    update_screen_sequences(&mut publish.screen_sequences, &screens);
    if crate::perf::enabled()
        && [
            drain_elapsed,
            snapshot_build,
            history_extract,
            json_serialize,
            json_write,
        ]
        .into_iter()
        .any(|elapsed| elapsed >= Duration::from_millis(5))
    {
        crate::perf::log(&format!(
            "P2PMUX_PERF node drain_ms={} snapshot_build_ms={} history_extract_ms={} history_panes={} json_serialize_ms={} json_write_ms={} write_bytes={} panes_local={} panes_remote={} updates_snapshot={}({}B) updates_delta={}({}B) updates_unchanged={}",
            drain_elapsed.as_millis(),
            snapshot_build.as_millis(),
            history_extract.as_millis(),
            updates.history_panes,
            json_serialize.as_millis(),
            json_write.as_millis(),
            frame.len(),
            local_panes,
            remote_panes,
            updates.snapshots,
            updates.snapshot_bytes,
            updates.deltas,
            updates.delta_bytes,
            updates.unchanged,
        ));
    }
    Ok(())
}

#[derive(Default)]
struct AttachmentPublishState {
    layout: Option<crate::layout::LayoutSnapshot>,
    leases: Option<Vec<PaneLeaseSnapshot>>,
    rosters: Option<Vec<AgentOverlaySnapshotRow>>,
    focus: Option<(u64, u64)>,
    screen_sequences: BTreeMap<u64, u64>,
}

fn write_updates(
    stream: &mut UnixStream,
    node: &SharedLayoutNode,
    publish: &mut AttachmentPublishState,
    _drain_elapsed: Duration,
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
        write_message(stream, &NodeMessage::Layout {
            layout: Box::new(layout.clone()),
        })?;
        publish.layout = Some(layout.clone());
        publish
            .screen_sequences
            .retain(|pane_id, _| layout.panes.contains_key(pane_id));
        published = true;
    }
    if publish.leases.as_ref() != Some(&leases) {
        write_message(stream, &NodeMessage::Leases {
            leases: leases.clone(),
        })?;
        publish.leases = Some(leases);
        published = true;
    }
    if publish.rosters.as_ref() != Some(&rosters) {
        write_message(stream, &NodeMessage::Rosters {
            rosters: rosters.clone(),
        })?;
        publish.rosters = Some(rosters);
        published = true;
    }
    if publish.focus != Some(focus) {
        write_message(
            stream,
            &NodeMessage::Focus {
                tab_id: focus.0,
                pane_id: focus.1,
            },
        )?;
        publish.focus = Some(focus);
        published = true;
    }
    let frames = pane_screen_updates(
        screens,
        &publish.screen_sequences,
        |pane_id| {
            node.node_local_history(pane_id)
                .map(|(total_rows, rows)| LocalHistorySnapshot {
                    total_rows,
                    rows: rows.into_iter().map(|row| STANDARD.encode(row)).collect(),
                })
        },
        |pane_id| node.node_remote_snapshot(pane_id),
    );
    if frames.is_empty() {
        return Ok(published);
    }
    write_message(stream, &NodeMessage::Screens {
        screens: frames.clone(),
    })?;
    update_screen_sequences(&mut publish.screen_sequences, &frames);
    Ok(true)
}

fn pane_screen_updates<LocalHistory, RemoteSnapshot>(
    screens: crate::tui::NodeScreenSnapshots,
    sequences: &BTreeMap<u64, u64>,
    mut local_history: LocalHistory,
    mut remote_snapshot: RemoteSnapshot,
) -> Vec<PaneScreenSnapshot>
where
    LocalHistory: FnMut(u64) -> Option<LocalHistorySnapshot>,
    RemoteSnapshot: FnMut(u64) -> Option<Vec<u8>>,
{
    screens
        .into_iter()
        .filter_map(|(pane_id, screen)| {
            let update = match screen {
                crate::tui::NodeScreenSnapshot::Local { frame } => {
                    if sequences.get(&pane_id) == Some(&frame.sequence) {
                        None
                    } else if sequences.get(&pane_id) == Some(&frame.base_sequence) {
                        let state = ScreenUpdate::Delta {
                            base_sequence: frame.base_sequence,
                            sequence: frame.sequence,
                            delta: frame.delta.as_ref().to_vec(),
                            kitty_keyboard_active: frame.kitty_keyboard_active,
                        };
                        Some((frame.sequence, state, local_history(pane_id)))
                    } else {
                        let state = ScreenUpdate::Snapshot {
                            sequence: frame.sequence,
                            snapshot: frame.snapshot.as_ref().to_vec(),
                            kitty_keyboard_active: frame.kitty_keyboard_active,
                        };
                        Some((frame.sequence, state, local_history(pane_id)))
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
                            None,
                        ))
                    }
                }
            };
            let (_, state, local_history) = update?;
            Some(PaneScreenSnapshot {
                pane_id,
                state,
                local_history,
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
    history_panes: usize,
}

impl ScreenUpdateStats {
    fn from_screens(screens: &[PaneScreenSnapshot]) -> Self {
        let mut stats = Self::default();
        for screen in screens {
            stats.history_panes += usize::from(screen.local_history.is_some());
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
    pub(crate) fn node_local_history(&self, pane_id: u64) -> Option<(usize, Vec<Vec<u8>>)> {
        self.runtime.node_local_history(pane_id)
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
            Some(ClientMessage::Input { bytes }) if bytes == b"first"
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
    fn local_screen_updates_use_snapshot_delta_and_unchanged() {
        let mut screen = HostScreen::new(1, 3).unwrap();
        let mut sequences = BTreeMap::new();
        let initial = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
            },
        )]);
        let updates = pane_screen_updates(initial, &mut sequences, |_| None, |_| None);
        assert!(matches!(updates[0].state, ScreenUpdate::Snapshot { .. }));
        update_screen_sequences(&mut sequences, &updates);

        screen.process_pty(b"a").unwrap();
        let changed = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
            },
        )]);
        let updates = pane_screen_updates(changed, &mut sequences, |_| None, |_| None);
        assert!(matches!(updates[0].state, ScreenUpdate::Delta { .. }));
        update_screen_sequences(&mut sequences, &updates);

        let unchanged = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
            },
        )]);
        let updates = pane_screen_updates(unchanged, &mut sequences, |_| None, |_| None);
        assert!(updates.is_empty());

        screen.resize(2, 3).unwrap();
        let resized = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
            },
        )]);
        let updates = pane_screen_updates(resized, &mut sequences, |_| None, |_| None);
        assert!(matches!(updates[0].state, ScreenUpdate::Snapshot { .. }));
    }

    #[test]
    fn unchanged_local_screen_updates_omit_history() {
        let screen = HostScreen::new(1, 3).unwrap();
        let mut sequences = BTreeMap::new();
        let initial = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
            },
        )]);
        let updates = pane_screen_updates(
            initial,
            &mut sequences,
            |_| {
                Some(LocalHistorySnapshot {
                    total_rows: 1,
                    rows: vec![STANDARD.encode(b"history")],
                })
            },
            |_| None,
        );
        assert!(updates[0].local_history.is_some());
        update_screen_sequences(&mut sequences, &updates);

        let unchanged = BTreeMap::from([(
            1,
            crate::tui::NodeScreenSnapshot::Local {
                frame: screen.current_frame().clone(),
            },
        )]);
        let updates = pane_screen_updates(
            unchanged,
            &mut sequences,
            |_| panic!("unchanged local panes must not fetch history"),
            |_| None,
        );
        assert!(updates.is_empty());
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
        let updates = pane_screen_updates(
            initial,
            &mut sequences,
            |_| None,
            |_| {
                calls.set(calls.get() + 1);
                Some(vec![1])
            },
        );
        assert!(matches!(updates[0].state, ScreenUpdate::Snapshot { .. }));
        assert_eq!(calls.get(), 1);
        update_screen_sequences(&mut sequences, &updates);

        let unchanged = BTreeMap::from([(1, remote())]);
        let updates = pane_screen_updates(
            unchanged,
            &mut sequences,
            |_| None,
            |_| panic!("unchanged remote panes must not encode a snapshot"),
        );
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
                    local_history: None,
                }],
                leases: vec![],
                rosters: vec![],
                local_peer_id: vec![],
                tab_id: 1,
                pane_id: 1,
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
