//! Authenticated Join/Welcome session handshakes over Iroh bi-streams.

use std::{error::Error, fmt, time::Duration};

use iroh::{
    EndpointId,
    endpoint::{ConnectingError, Connection, Incoming},
};
use tokio::{
    sync::{mpsc, watch},
    time::{interval, timeout},
};

use crate::{
    lease::LeaseState,
    protocol::{
        ControlLease, Delta, Envelope, Input, Join, PROTOCOL_VERSION, Snapshot, TakeControl,
        Welcome, envelope,
    },
    screen::ScreenFrame,
    ticket::{JoinTicket, TicketError},
    transport::{HANDSHAKE_TIMEOUT, Transport, TransportError},
};

pub const DEFAULT_PANE_ID: &[u8] = b"default-pane";

pub struct HostPaneChannels {
    pub pane_id: Vec<u8>,
    pub host_peer_id: Vec<u8>,
    pub screen_rx: watch::Receiver<ScreenFrame>,
    pub lease_rx: watch::Receiver<LeaseState>,
    pub control_tx: mpsc::Sender<HostControlEvent>,
}

#[derive(Debug)]
pub enum HostControlEvent {
    Input {
        peer_id: Vec<u8>,
        input: Input,
    },
    TakeControl {
        peer_id: Vec<u8>,
        request: TakeControl,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub enum ScreenSendPlan {
    Snapshot,
    Delta,
}

pub fn screen_send_plan(
    last_sent_sequence: u64,
    frame: &ScreenFrame,
    heartbeat_due: bool,
) -> ScreenSendPlan {
    if heartbeat_due || last_sent_sequence != frame.base_sequence {
        ScreenSendPlan::Snapshot
    } else {
        ScreenSendPlan::Delta
    }
}

#[derive(Debug)]
pub enum GuestEvent {
    ScreenSnapshot(Snapshot),
    ScreenDelta(Delta),
    ScreenGap {
        expected_base: Option<u64>,
        received_base: u64,
    },
    Lease(ControlLease),
    Disconnected,
}

#[derive(Clone)]
pub struct GuestControlSender {
    peer_id: Vec<u8>,
    pane_id: Vec<u8>,
    take_control_tx: mpsc::Sender<u64>,
    input_tx: mpsc::Sender<(u64, Vec<u8>)>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ControlQueueError {
    Full,
    Closed,
}

impl GuestControlSender {
    pub fn try_take_control(&self, known_lease_epoch: u64) -> Result<(), ControlQueueError> {
        self.take_control_tx
            .try_send(known_lease_epoch)
            .map_err(queue_error)
    }

    pub fn try_input(&self, lease_epoch: u64, data: Vec<u8>) -> Result<(), ControlQueueError> {
        self.input_tx
            .try_send((lease_epoch, data))
            .map_err(queue_error)
    }

    pub fn peer_id(&self) -> &[u8] {
        &self.peer_id
    }
    pub fn pane_id(&self) -> &[u8] {
        &self.pane_id
    }
}

fn queue_error<T>(error: mpsc::error::TrySendError<T>) -> ControlQueueError {
    match error {
        mpsc::error::TrySendError::Full(_) => ControlQueueError::Full,
        mpsc::error::TrySendError::Closed(_) => ControlQueueError::Closed,
    }
}

pub struct GuestPane {
    pub pane_id: Vec<u8>,
    pub host_peer_id: Vec<u8>,
    pub events: mpsc::Receiver<GuestEvent>,
    pub controls: GuestControlSender,
    transport: Transport,
    connection: Connection,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl GuestPane {
    pub async fn shutdown(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        while let Some(task) = self.tasks.pop() {
            let _ = task.await;
        }
        self.connection.close(0u8.into(), b"");
        self.transport.close().await;
    }
}

impl Drop for GuestPane {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.connection.close(0u8.into(), b"");
    }
}

#[derive(Clone)]
pub struct HostSession {
    transport: Transport,
    ticket: JoinTicket,
    address_ready: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinReceipt {
    pub session_id: Vec<u8>,
    pub admitted_peer_id: Vec<u8>,
    pub coordinator_peer_id: Vec<u8>,
}

#[derive(Debug)]
pub enum SessionError {
    Transport(TransportError),
    Ticket(TicketError),
    Incoming(ConnectingError),
    TimedOut(&'static str),
    InvalidJoin,
    InvalidWelcome,
    UnauthenticatedPeer,
    InvalidPostWelcome,
    PeerTask,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "transport error: {error}"),
            Self::Ticket(error) => write!(formatter, "ticket error: {error}"),
            Self::Incoming(error) => write!(formatter, "incoming handshake failed: {error}"),
            Self::TimedOut(operation) => write!(formatter, "session {operation} timed out"),
            Self::InvalidJoin => formatter.write_str("invalid Join handshake message"),
            Self::InvalidWelcome => formatter.write_str("invalid Welcome handshake message"),
            Self::UnauthenticatedPeer => {
                formatter.write_str("handshake peer identity did not match")
            }
            Self::InvalidPostWelcome => formatter.write_str("invalid post-Welcome message"),
            Self::PeerTask => formatter.write_str("peer stream task stopped unexpectedly"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Ticket(error) => Some(error),
            Self::Incoming(error) => Some(error),
            Self::TimedOut(_)
            | Self::InvalidJoin
            | Self::InvalidWelcome
            | Self::UnauthenticatedPeer
            | Self::InvalidPostWelcome
            | Self::PeerTask => None,
        }
    }
}

impl From<TransportError> for SessionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl HostSession {
    pub async fn create() -> Result<Self, SessionError> {
        let transport = Transport::bind().await?;
        let address_ready = transport.wait_until_online().await;
        let ticket = JoinTicket::mint(transport.endpoint_addr()).map_err(SessionError::Ticket)?;
        Ok(Self {
            transport,
            ticket,
            address_ready,
        })
    }

    pub fn from_transport(transport: Transport) -> Result<Self, SessionError> {
        let ticket = JoinTicket::mint(transport.endpoint_addr()).map_err(SessionError::Ticket)?;
        Ok(Self {
            transport,
            ticket,
            address_ready: true,
        })
    }

    pub fn ticket(&self) -> &JoinTicket {
        &self.ticket
    }

    pub fn address_ready(&self) -> bool {
        self.address_ready
    }

    pub async fn accept_incoming(&self) -> Result<Incoming, SessionError> {
        self.transport.accept_incoming().await.map_err(Into::into)
    }

    pub async fn handle_incoming(&self, incoming: Incoming) -> Result<JoinReceipt, SessionError> {
        let connection = timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| SessionError::TimedOut("incoming connection"))?
            .map_err(SessionError::Incoming)?;
        match self.handshake_connection(&connection).await {
            Ok(receipt) => {
                let _ = timeout(HANDSHAKE_TIMEOUT, connection.closed()).await;
                Ok(receipt)
            }
            Err(error) => {
                connection.close(0u8.into(), b"");
                Err(error)
            }
        }
    }

    pub async fn accept_one_join(&self) -> Result<JoinReceipt, SessionError> {
        let incoming = self.accept_incoming().await?;
        self.handle_incoming(incoming).await
    }

    pub async fn close(&self) {
        self.transport.close().await;
    }

    pub async fn serve_peer(
        &self,
        incoming: Incoming,
        pane: HostPaneChannels,
    ) -> Result<(), SessionError> {
        if pane.pane_id.as_slice() != DEFAULT_PANE_ID
            || pane.host_peer_id.as_slice() != self.transport.endpoint_id().as_bytes()
        {
            return Err(SessionError::InvalidPostWelcome);
        }
        let connection = timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| SessionError::TimedOut("incoming connection"))?
            .map_err(SessionError::Incoming)?;
        let result = async {
            self.handshake_connection(&connection).await?;
            let (screen_writer, _) = self.transport.open_framed_bi(&connection).await?;
            let screen_task = tokio::spawn(screen_writer_task(
                screen_writer,
                pane.screen_rx,
                pane.pane_id.clone(),
                pane.host_peer_id.clone(),
            ));
            let (control_writer, control_reader) = match self
                .transport
                .accept_framed_bi_when_ready(&connection)
                .await
            {
                Ok(streams) => streams,
                Err(error) => {
                    screen_task.abort();
                    return Err(error.into());
                }
            };
            let lease_task = tokio::spawn(lease_writer_task(
                self.transport.endpoint_id().as_bytes().to_vec(),
                pane.lease_rx,
                pane.pane_id.clone(),
                control_writer,
            ));
            let control_task = tokio::spawn(control_reader_task(
                control_reader,
                connection.remote_id().as_bytes().to_vec(),
                pane.pane_id,
                pane.control_tx,
            ));
            let mut screen_task = screen_task;
            let mut lease_task = lease_task;
            let mut control_task = control_task;
            let result = tokio::select! {
                result = &mut screen_task => join_peer_task(result),
                result = &mut lease_task => join_peer_task(result),
                result = &mut control_task => join_peer_task(result),
            };
            screen_task.abort();
            lease_task.abort();
            control_task.abort();
            result
        }
        .await;
        if result.is_err() {
            connection.close(0u8.into(), b"");
        }
        result
    }

    async fn handshake_connection(
        &self,
        connection: &Connection,
    ) -> Result<JoinReceipt, SessionError> {
        let remote_id = connection.remote_id();
        let (mut send, mut recv) = self.transport.accept_bi(connection).await?;
        let envelope = self.transport.read_frame(&mut recv).await?;
        let join = match envelope.body {
            Some(envelope::Body::Join(join)) => join,
            _ => return Err(SessionError::InvalidJoin),
        };
        if join.session_id.as_slice() != self.ticket.session_id()
            || envelope.sender_peer_id.as_slice() != remote_id.as_bytes()
            || join.peer_id.as_slice() != remote_id.as_bytes()
        {
            return Err(SessionError::UnauthenticatedPeer);
        }

        let coordinator = self.transport.endpoint_id();
        let receipt = JoinReceipt {
            session_id: self.ticket.session_id().to_vec(),
            admitted_peer_id: remote_id.as_bytes().to_vec(),
            coordinator_peer_id: coordinator.as_bytes().to_vec(),
        };
        self.transport
            .write_frame(
                &mut send,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    sender_peer_id: coordinator.as_bytes().to_vec(),
                    body: Some(envelope::Body::Welcome(Welcome {
                        session_id: receipt.session_id.clone(),
                        admitted_peer_id: receipt.admitted_peer_id.clone(),
                        coordinator_peer_id: receipt.coordinator_peer_id.clone(),
                    })),
                },
            )
            .await?;
        Ok(receipt)
    }
}

fn join_peer_task(
    result: Result<Result<(), SessionError>, tokio::task::JoinError>,
) -> Result<(), SessionError> {
    result.map_err(|_| SessionError::PeerTask)?
}

async fn screen_writer_task(
    mut writer: crate::transport::FrameWriter,
    mut screen_rx: watch::Receiver<ScreenFrame>,
    pane_id: Vec<u8>,
    host_peer_id: Vec<u8>,
) -> Result<(), SessionError> {
    let initial = screen_rx.borrow().clone();
    write_snapshot(&mut writer, &pane_id, &host_peer_id, &initial).await?;
    let mut last_sent_sequence = initial.sequence;
    let mut heartbeat = interval(Duration::from_millis(500));
    heartbeat.tick().await;
    loop {
        tokio::select! {
            changed = screen_rx.changed() => {
                changed.map_err(|_| SessionError::PeerTask)?;
                let frame = screen_rx.borrow_and_update().clone();
                if screen_send_plan(last_sent_sequence, &frame, false) == ScreenSendPlan::Snapshot {
                    write_snapshot(&mut writer, &pane_id, &host_peer_id, &frame).await?;
                } else {
                    writer.write_next(&Envelope {
                        version: PROTOCOL_VERSION,
                        sender_peer_id: host_peer_id.clone(),
                        body: Some(envelope::Body::Delta(Delta {
                            pane_id: pane_id.clone(),
                            host_peer_id: host_peer_id.clone(),
                            base_sequence: frame.base_sequence,
                            sequence: frame.sequence,
                            changes: frame.delta.to_vec(),
                        })),
                    }).await?;
                }
                last_sent_sequence = frame.sequence;
            }
            _ = heartbeat.tick() => {
                let frame = screen_rx.borrow().clone();
                if screen_send_plan(last_sent_sequence, &frame, true) == ScreenSendPlan::Snapshot {
                    write_snapshot(&mut writer, &pane_id, &host_peer_id, &frame).await?;
                }
                last_sent_sequence = frame.sequence;
            }
        }
    }
}

async fn write_snapshot(
    writer: &mut crate::transport::FrameWriter,
    pane_id: &[u8],
    host_peer_id: &[u8],
    frame: &ScreenFrame,
) -> Result<(), SessionError> {
    writer
        .write_next(&Envelope {
            version: PROTOCOL_VERSION,
            sender_peer_id: host_peer_id.to_vec(),
            body: Some(envelope::Body::Snapshot(Snapshot {
                pane_id: pane_id.to_vec(),
                host_peer_id: host_peer_id.to_vec(),
                sequence: frame.sequence,
                screen: frame.snapshot.to_vec(),
            })),
        })
        .await
        .map_err(Into::into)
}

async fn lease_writer_task(
    sender_peer_id: Vec<u8>,
    mut lease_rx: watch::Receiver<LeaseState>,
    pane_id: Vec<u8>,
    mut writer: crate::transport::FrameWriter,
) -> Result<(), SessionError> {
    loop {
        let lease = lease_rx.borrow_and_update().clone();
        writer
            .write_next(&Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: sender_peer_id.clone(),
                body: Some(envelope::Body::ControlLease(ControlLease {
                    pane_id: pane_id.clone(),
                    controller_peer_id: lease.controller_peer_id,
                    lease_epoch: lease.epoch,
                })),
            })
            .await?;
        lease_rx
            .changed()
            .await
            .map_err(|_| SessionError::PeerTask)?;
    }
}

async fn control_reader_task(
    mut reader: crate::transport::FrameReader,
    peer_id: Vec<u8>,
    pane_id: Vec<u8>,
    control_tx: mpsc::Sender<HostControlEvent>,
) -> Result<(), SessionError> {
    while let Some(envelope) = reader.read_next().await? {
        if envelope.sender_peer_id != peer_id {
            return Err(SessionError::UnauthenticatedPeer);
        }
        let event = match envelope.body {
            Some(envelope::Body::Input(input)) if input.pane_id == pane_id => {
                HostControlEvent::Input {
                    peer_id: peer_id.clone(),
                    input,
                }
            }
            Some(envelope::Body::TakeControl(request))
                if request.pane_id == pane_id && request.requester_peer_id == peer_id =>
            {
                HostControlEvent::TakeControl {
                    peer_id: peer_id.clone(),
                    request,
                }
            }
            _ => return Err(SessionError::InvalidPostWelcome),
        };
        control_tx
            .send(event)
            .await
            .map_err(|_| SessionError::PeerTask)?;
    }
    Ok(())
}

pub async fn join_once(
    transport: Transport,
    ticket: JoinTicket,
) -> Result<JoinReceipt, SessionError> {
    let result = async {
        let connection = transport.connect(ticket.endpoint_addr().clone()).await?;
        let result = join_connected(&transport, &connection, &ticket).await;
        connection.close(0u8.into(), b"");
        result
    }
    .await;
    transport.close().await;
    result
}

pub async fn join_pane(
    transport: Transport,
    ticket: JoinTicket,
) -> Result<GuestPane, SessionError> {
    let connection = transport.connect(ticket.endpoint_addr().clone()).await?;
    let result = async {
        let receipt = join_handshake(&transport, &connection, &ticket).await?;
        let host_peer_id = receipt.coordinator_peer_id;
        let (_screen_send, screen_reader) = transport.accept_framed_bi(&connection).await?;
        let (control_writer, control_reader) = transport.open_framed_bi(&connection).await?;
        let (events_tx, events) = mpsc::channel(128);
        let (take_control_tx, take_control_rx) = mpsc::channel(16);
        let (input_tx, input_rx) = mpsc::channel(256);
        let peer_id = transport.endpoint_id().as_bytes().to_vec();
        let pane_id = DEFAULT_PANE_ID.to_vec();
        let _ = events_tx.try_send(GuestEvent::Lease(ControlLease {
            pane_id: pane_id.clone(),
            controller_peer_id: host_peer_id.clone(),
            lease_epoch: 1,
        }));
        let tasks = vec![
            tokio::spawn(guest_screen_reader_task(
                screen_reader,
                events_tx.clone(),
                pane_id.clone(),
                host_peer_id.clone(),
            )),
            tokio::spawn(guest_lease_reader_task(
                control_reader,
                events_tx,
                pane_id.clone(),
                host_peer_id.clone(),
            )),
            tokio::spawn(guest_control_writer_task(
                control_writer,
                take_control_rx,
                input_rx,
                peer_id.clone(),
                pane_id.clone(),
            )),
        ];
        Ok(GuestPane {
            pane_id: pane_id.clone(),
            host_peer_id,
            events,
            controls: GuestControlSender {
                peer_id,
                pane_id,
                take_control_tx,
                input_tx,
            },
            transport: transport.clone(),
            connection: connection.clone(),
            tasks,
        })
    }
    .await;
    if result.is_err() {
        connection.close(0u8.into(), b"");
        transport.close().await;
    }
    result
}

async fn join_connected(
    transport: &Transport,
    connection: &Connection,
    ticket: &JoinTicket,
) -> Result<JoinReceipt, SessionError> {
    join_handshake(transport, connection, ticket).await
}

async fn join_handshake(
    transport: &Transport,
    connection: &Connection,
    ticket: &JoinTicket,
) -> Result<JoinReceipt, SessionError> {
    let client_id = transport.endpoint_id();
    if connection.remote_id() != ticket.endpoint_addr().id {
        return Err(SessionError::UnauthenticatedPeer);
    }
    let (mut send, mut recv) = transport.open_bi(connection).await?;
    transport
        .write_frame(
            &mut send,
            &Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: client_id.as_bytes().to_vec(),
                body: Some(envelope::Body::Join(Join {
                    session_id: ticket.session_id().to_vec(),
                    peer_id: client_id.as_bytes().to_vec(),
                })),
            },
        )
        .await?;
    let envelope = transport.read_frame(&mut recv).await?;
    let sender_peer_id = envelope.sender_peer_id;
    let welcome = match envelope.body {
        Some(envelope::Body::Welcome(welcome)) => welcome,
        _ => return Err(SessionError::InvalidWelcome),
    };
    validate_welcome(&sender_peer_id, &welcome, ticket, client_id)?;
    Ok(JoinReceipt {
        session_id: welcome.session_id,
        admitted_peer_id: welcome.admitted_peer_id,
        coordinator_peer_id: welcome.coordinator_peer_id,
    })
}

async fn guest_screen_reader_task(
    mut reader: crate::transport::FrameReader,
    events_tx: mpsc::Sender<GuestEvent>,
    pane_id: Vec<u8>,
    host_peer_id: Vec<u8>,
) {
    let mut sequence = None;
    while let Ok(Some(envelope)) = reader.read_next().await {
        if envelope.sender_peer_id != host_peer_id {
            break;
        }
        match envelope.body {
            Some(envelope::Body::Snapshot(snapshot))
                if snapshot.pane_id == pane_id && snapshot.host_peer_id == host_peer_id =>
            {
                sequence = Some(snapshot.sequence);
                let _ = events_tx.try_send(GuestEvent::ScreenSnapshot(snapshot));
            }
            Some(envelope::Body::Delta(delta))
                if delta.pane_id == pane_id && delta.host_peer_id == host_peer_id =>
            {
                if sequence == Some(delta.base_sequence) {
                    sequence = Some(delta.sequence);
                    let _ = events_tx.try_send(GuestEvent::ScreenDelta(delta));
                } else {
                    let _ = events_tx.try_send(GuestEvent::ScreenGap {
                        expected_base: sequence,
                        received_base: delta.base_sequence,
                    });
                }
            }
            _ => break,
        }
    }
    let _ = events_tx.send(GuestEvent::Disconnected).await;
}

async fn guest_lease_reader_task(
    mut reader: crate::transport::FrameReader,
    events_tx: mpsc::Sender<GuestEvent>,
    pane_id: Vec<u8>,
    host_peer_id: Vec<u8>,
) {
    while let Ok(Some(envelope)) = reader.read_next().await {
        match envelope.body {
            Some(envelope::Body::ControlLease(lease))
                if envelope.sender_peer_id == host_peer_id && lease.pane_id == pane_id =>
            {
                if events_tx.send(GuestEvent::Lease(lease)).await.is_err() {
                    return;
                }
            }
            _ => break,
        }
    }
    let _ = events_tx.send(GuestEvent::Disconnected).await;
}

async fn guest_control_writer_task(
    mut writer: crate::transport::FrameWriter,
    mut take_control_rx: mpsc::Receiver<u64>,
    mut input_rx: mpsc::Receiver<(u64, Vec<u8>)>,
    peer_id: Vec<u8>,
    pane_id: Vec<u8>,
) {
    loop {
        tokio::select! {
            biased;
            Some(known_lease_epoch) = take_control_rx.recv() => {
                let _ = writer.write_next(&Envelope {
                    version: PROTOCOL_VERSION,
                    sender_peer_id: peer_id.clone(),
                    body: Some(envelope::Body::TakeControl(TakeControl {
                        pane_id: pane_id.clone(),
                        requester_peer_id: peer_id.clone(),
                        known_lease_epoch,
                    })),
                }).await;
            }
            Some((lease_epoch, data)) = input_rx.recv() => {
                let _ = writer.write_next(&Envelope {
                    version: PROTOCOL_VERSION,
                    sender_peer_id: peer_id.clone(),
                    body: Some(envelope::Body::Input(Input { pane_id: pane_id.clone(), lease_epoch, data })),
                }).await;
            }
            else => return,
        }
    }
}

fn validate_welcome(
    sender_peer_id: &[u8],
    welcome: &Welcome,
    ticket: &JoinTicket,
    client_id: EndpointId,
) -> Result<(), SessionError> {
    let coordinator = ticket.endpoint_addr().id.as_bytes();
    if sender_peer_id != coordinator
        || welcome.coordinator_peer_id.as_slice() != coordinator
        || welcome.session_id.as_slice() != ticket.session_id()
        || welcome.admitted_peer_id.as_slice() != client_id.as_bytes()
    {
        return Err(SessionError::UnauthenticatedPeer);
    }
    Ok(())
}
