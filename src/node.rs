//! Headless owner of a shared layout.
//!
//! `SharedLayoutRuntime` remains the temporary foreground adapter during the split.  The node
//! owns it without terminal I/O and exposes only node operations; the local socket client is the
//! only component allowed to render a terminal.

use std::{
    error::Error,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::{
        fs::OpenOptionsExt,
        net::{UnixListener, UnixStream},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    local_ipc::{
        AgentOverlaySnapshotRow, AttachmentGate, ClientMessage, NodeMessage, PaneLeaseSnapshot,
        PaneScreenSnapshot, SessionSummary,
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

pub struct SharedLayoutNode {
    runtime: SharedLayoutRuntime,
    last_tab_id: u64,
    last_pane_id: u64,
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
    let mut client: Option<(BufReader<UnixStream>, u64)> = None;
    loop {
        let mut shutdown = false;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
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
                                let attached = write_message(
                                    reader.get_mut(),
                                    &NodeMessage::AttachAccepted { generation },
                                )
                                .and_then(|()| {
                                    write_snapshot(reader.get_mut(), descriptor, node, generation)
                                })
                                .is_ok();
                                if attached {
                                    reader
                                        .get_mut()
                                        .set_read_timeout(Some(Duration::from_millis(1)))?;
                                    client = Some((reader, generation));
                                } else {
                                    let _ = gate.detach(generation);
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
        let mut changed = node
            .drain()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut detached = false;
        if !shutdown && let Some((reader, generation)) = client.as_mut() {
            match read_message(reader) {
                Ok(Some(ClientMessage::Input { bytes })) => node
                    .input(bytes)
                    .map_err(|error| io::Error::other(error.to_string()))?,
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
            if changed
                && !detached
                && write_snapshot(reader.get_mut(), descriptor, node, *generation).is_err()
            {
                detached = true;
            }
        }
        if (detached || shutdown)
            && let Some((mut reader, generation)) = client.take()
        {
            let _ = reader.get_mut().shutdown(Shutdown::Both);
            let _ = gate.detach(generation);
            node.release_all_local_control()
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        if shutdown {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(16));
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
    _generation: u64,
) -> io::Result<()> {
    let (tab_id, pane_id) = node.local_focus();
    let (layout, screens, leases, rosters) = node.snapshot();
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
    write_message(
        stream,
        &NodeMessage::Snapshot {
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
            layout: Box::new(layout),
            screens: screens
                .into_iter()
                .map(
                    |(pane_id, (sequence, snapshot, kitty_keyboard_active))| PaneScreenSnapshot {
                        pane_id,
                        sequence,
                        snapshot,
                        kitty_keyboard_active,
                    },
                )
                .collect(),
            leases: leases
                .into_iter()
                .map(
                    |(pane_id, (ready, controller_peer_id, controller_active))| PaneLeaseSnapshot {
                        pane_id,
                        ready,
                        controller_peer_id,
                        controller_active,
                    },
                )
                .collect(),
            rosters,
            tab_id,
            pane_id,
        },
    )
}

impl SharedLayoutNode {
    pub fn new(runtime: SharedLayoutRuntime) -> Self {
        let (last_tab_id, last_pane_id) = runtime.local_focus();
        Self {
            runtime,
            last_tab_id,
            last_pane_id,
        }
    }

    /// Advances Iroh, pane servers, PTYs, leases, subscriptions and agent sampling without ever
    /// touching terminal state.
    pub fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let changed = self.runtime.drain_node()?;
        (self.last_tab_id, self.last_pane_id) = self.runtime.local_focus();
        Ok(changed)
    }

    pub fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        self.runtime.node_input(bytes)
    }
    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> {
        self.runtime.release_all_local_control()
    }
    pub fn local_focus(&self) -> (u64, u64) {
        (self.last_tab_id, self.last_pane_id)
    }
    pub fn snapshot(
        &self,
    ) -> (
        crate::layout::LayoutSnapshot,
        crate::tui::NodeScreenSnapshots,
        crate::tui::NodeLeaseSnapshots,
        Vec<AgentOverlaySnapshotRow>,
    ) {
        self.runtime.node_snapshot()
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
    use std::{
        io::Write,
        os::unix::{fs::PermissionsExt, net::UnixStream},
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
