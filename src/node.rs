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
    os::unix::{fs::OpenOptionsExt, net::{UnixListener, UnixStream}},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    local_ipc::{AttachmentGate, ClientMessage, NodeMessage, SessionSummary},
    rendezvous::LocalRendezvous,
    session::{HostSession, LayoutControlEvent, SharedLayoutHost, join_layout_with_display_name, layout_snapshot_from_state},
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
    Create { display_name: String, cols: u16, rows: u16 },
    Join { ticket: String, display_name: String, cols: u16, rows: u16 },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeBootstrap { pub descriptor: SessionDescriptor, pub kind: NodeBootstrapKind }

pub fn write_bootstrap(path: &std::path::Path, bootstrap: &NodeBootstrap) -> io::Result<()> {
    let bytes = serde_json::to_vec(bootstrap).map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(&bytes)?; file.sync_all()
}

pub fn read_bootstrap(path: &std::path::Path) -> io::Result<NodeBootstrap> {
    let bootstrap = serde_json::from_slice(&fs::read(path)?).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid node bootstrap"))?;
    let _ = fs::remove_file(path);
    Ok(bootstrap)
}

/// Private child entrypoint. It owns the descriptor, rendezvous record, socket and every PTY.
pub async fn run_background(bootstrap: NodeBootstrap) -> Result<(), Box<dyn Error>> {
    let mut descriptor = bootstrap.descriptor.clone();
    descriptor.node_pid = std::process::id();
    let (mut node, dispatcher_task, rendezvous) = match bootstrap.kind {
        NodeBootstrapKind::Create { display_name, cols, rows } => {
            let (shell_rows, shell_cols) = crate::tui::initial_root_pane_grid(cols, rows);
            let host = SharedLayoutHost::with_display_name(HostSession::create().await?, display_name, shell_rows, shell_cols)?;
            let host_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
            let initial = SharedLocalPane::spawn(1, shell_rows, shell_cols, host_peer_id.clone())?;
            let panes = host.pane_server();
            panes.register_local_pane(crate::protocol::PaneDescriptor { pane_id: 1, host_peer_id, grid_rows: u32::from(shell_rows), grid_cols: u32::from(shell_cols) }, initial.channels())?;
            let snapshot = host.session_snapshot()?;
            let layout = layout_snapshot_from_state(snapshot.state.as_ref().ok_or("missing host layout")?)
                .map_err(|error| io::Error::other(format!("invalid host layout: {error:?}")))?;
            let dispatcher = host.incoming_dispatcher(panes.clone())?;
            let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });
            let rendezvous = LocalRendezvous::for_current_user()?.publish(host.ticket())?;
            let join_code = rendezvous.code().to_owned();
            let handle = tokio::runtime::Handle::current();
            let mut runtime = crate::tui::SharedLayoutRuntime::host(host, panes, layout, initial, join_code, handle)?;
            runtime.set_session_id(descriptor.id.as_bytes().to_vec());
            (SharedLayoutNode::new(runtime), dispatcher_task, Some(rendezvous))
        }
        NodeBootstrapKind::Join { ticket, display_name, cols: _, rows: _ } => {
            let ticket = ticket.parse::<JoinTicket>().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid ticket"))?;
            let transport = Transport::bind().await?;
            let mut member = join_layout_with_display_name(transport, ticket.clone(), display_name).await?;
            let state = match member.events.recv().await {
                Some(LayoutControlEvent::Snapshot(snapshot)) => snapshot.state.ok_or("missing layout snapshot")?,
                _ => return Err(io::Error::other("layout coordinator disconnected during join").into()),
            };
            let panes = member.pane_server(ticket.session_id().to_vec())?;
            panes.replace_roster_from_layout(&state)?;
            let acceptor = panes.clone(); let dispatcher_task = tokio::spawn(async move { acceptor.accept_loop().await });
            let runtime = crate::tui::SharedLayoutRuntime::member_from_state(member, panes, ticket.session_id().to_vec(), state, tokio::runtime::Handle::current())?;
            (SharedLayoutNode::new(runtime), dispatcher_task, None)
        }
    };
    let store = SessionStore::for_current_user()?;
    let _ = fs::remove_file(&descriptor.socket_path);
    let listener = UnixListener::bind(&descriptor.socket_path)?;
    listener.set_nonblocking(true)?;
    store.write(&descriptor)?;
    let result = run_socket_loop(&mut node, listener, &descriptor);
    node.shutdown();
    dispatcher_task.abort(); let _ = dispatcher_task.await;
    if let Some(rendezvous) = rendezvous { let _ = rendezvous.remove(); }
    let _ = fs::remove_file(&descriptor.socket_path); let _ = store.remove(&descriptor.id);
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn bootstrap_is_private_and_round_trips() {
        let path = std::env::temp_dir().join(format!("p2pmux-bootstrap-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let descriptor = SessionDescriptor::new("0123456789abcdef0123456789abcdef".into(), "amber-otter-01".into(), "/tmp/p2pmux-test.sock".into(), 1, SessionRole::Coordinator);
        write_bootstrap(&path, &NodeBootstrap { descriptor: descriptor.clone(), kind: NodeBootstrapKind::Create { display_name: "A".into(), cols: 80, rows: 24 } }).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_bootstrap(&path).unwrap().descriptor, descriptor);
        assert!(!path.exists());
    }
}

fn run_socket_loop(node: &mut SharedLayoutNode, listener: UnixListener, descriptor: &SessionDescriptor) -> io::Result<()> {
    let gate = AttachmentGate::default();
    let mut client: Option<(UnixStream, u64)> = None;
    loop {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_millis(1)))?;
                    if let Ok(generation) = gate.attach() {
                        let hello = read_message(&mut stream)?;
                        if matches!(hello, Some(ClientMessage::Hello { .. })) {
                            write_message(&mut stream, &NodeMessage::AttachAccepted { generation })?;
                            write_snapshot(&mut stream, descriptor, node, generation)?;
                            client = Some((stream, generation));
                        } else { let _ = gate.detach(generation); }
                    } else { let _ = write_message(&mut stream, &NodeMessage::AttachRejected { reason: "already attached".into() }); }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let changed = node.drain().map_err(|error| io::Error::other(error.to_string()))?;
        let mut detached = false; let mut shutdown = false;
        if let Some((stream, generation)) = client.as_mut() {
            match read_message(stream) {
                Ok(Some(ClientMessage::Input { bytes })) => node.input(bytes).map_err(|error| io::Error::other(error.to_string()))?,
                Ok(Some(ClientMessage::Detach { generation: requested })) if requested == *generation => { node.release_all_local_control().map_err(|error| io::Error::other(error.to_string()))?; write_message(stream, &NodeMessage::DetachAck { generation: *generation })?; detached = true; }
                Ok(Some(ClientMessage::Shutdown { generation: requested })) if requested == *generation => { write_message(stream, &NodeMessage::ShutdownAck { generation: *generation })?; shutdown = true; }
                Ok(Some(ClientMessage::Rename { name })) => { if crate::session_store::valid_name(&name) { write_message(stream, &NodeMessage::Update { state: serde_json::json!({"name": name}) })?; } else { write_message(stream, &NodeMessage::Error { message: "invalid session name".into() })?; } }
                Ok(Some(_)) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut => {}
                Ok(None) => detached = true,
                Err(_) => detached = true,
            }
            if changed { write_snapshot(stream, descriptor, node, *generation)?; }
        }
        if let Some((stream, generation)) = client.take().filter(|_| detached || shutdown) {
            let _ = stream.shutdown(Shutdown::Both); let _ = gate.detach(generation); node.release_all_local_control().map_err(|error| io::Error::other(error.to_string()))?;
        }
        if shutdown { return Ok(()); }
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn read_message(stream: &mut UnixStream) -> io::Result<Option<ClientMessage>> {
    let mut reader = BufReader::new(stream.try_clone()?); let mut line = String::new();
    match reader.read_line(&mut line) { Ok(0) => Ok(None), Ok(_) => serde_json::from_str(&line).map(Some).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid local IPC message")), Err(error) => Err(error) }
}
fn write_message(stream: &mut UnixStream, message: &NodeMessage) -> io::Result<()> { serde_json::to_writer(&mut *stream, message).map_err(io::Error::other)?; stream.write_all(b"\n")?; stream.flush() }
fn write_snapshot(stream: &mut UnixStream, descriptor: &SessionDescriptor, node: &SharedLayoutNode, _generation: u64) -> io::Result<()> {
    let (tab_id, pane_id) = node.local_focus();
    write_message(stream, &NodeMessage::Snapshot { room_name: descriptor.name.clone(), role: match descriptor.role { SessionRole::Coordinator => "coordinator", SessionRole::Member => "member" }.into(), summary: SessionSummary { tabs: 1, panes: 1, hosts: 1, coordinator_name: String::new() }, layout: serde_json::json!({}), screens: serde_json::json!(node.screen_text()), leases: serde_json::json!({}), rosters: serde_json::json!({}), tab_id, pane_id })
}

impl SharedLayoutNode {
    pub fn new(runtime: SharedLayoutRuntime) -> Self {
        let (last_tab_id, last_pane_id) = runtime.local_focus();
        Self { runtime, last_tab_id, last_pane_id }
    }

    /// Advances Iroh, pane servers, PTYs, leases, subscriptions and agent sampling without ever
    /// touching terminal state.
    pub fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let changed = self.runtime.drain_node()?;
        (self.last_tab_id, self.last_pane_id) = self.runtime.local_focus();
        Ok(changed)
    }

    pub fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> { self.runtime.node_input(bytes) }
    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> { self.runtime.release_all_local_control() }
    pub fn local_focus(&self) -> (u64, u64) { (self.last_tab_id, self.last_pane_id) }
    pub fn screen_text(&self) -> String { self.runtime.node_screen_text() }
    pub fn tick_due(&self) -> Instant { Instant::now() }
    pub fn shutdown(self) { self.runtime.shutdown_node(); }
}
