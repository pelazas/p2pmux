//! Authenticated Join/Welcome session handshakes over Iroh bi-streams.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    time::{Duration, Instant},
};

use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{ConnectingError, Connection, Incoming},
};
use tokio::{
    sync::{mpsc, watch},
    time::{interval, timeout},
};

use crate::{
    layout::{Axis, LayoutError, LayoutSnapshot, Node, SessionState},
    lease::LeaseState,
    protocol::{
        ControlLease, CreatePane, CreateTab, DeletePane, DeleteTab, Delta, Envelope, Input, Join,
        LayoutCommit, LayoutNode, LayoutReject, LayoutRejectReason, LayoutRequest, LayoutSplit,
        LayoutState, MemberDescriptor, PROTOCOL_VERSION, PaneDescriptor, PaneFailed, PaneReady,
        PaneReservation, SessionSnapshot, Snapshot, SplitAxis, TabDescriptor, TakeControl, Welcome,
        envelope,
    },
    screen::ScreenFrame,
    ticket::{JoinTicket, TicketError},
    transport::{HANDSHAKE_TIMEOUT, Transport, TransportError},
};

pub const DEFAULT_PANE_ID: &[u8] = b"default-pane";
pub const DEFAULT_RESERVATION_TIMEOUT: Duration = Duration::from_secs(30);

/// In-memory authority for the shared layout. Network code authenticates callers before handing
/// their protocol messages to this type.
pub struct LayoutCoordinator {
    state: SessionState,
    reservations: BTreeMap<u64, ReservationContext>,
    reservation_timeout: Duration,
}

#[derive(Clone, Debug)]
struct ReservationContext {
    request_id: u64,
    creator_peer_id: Vec<u8>,
    deadline: Instant,
}

#[derive(Debug)]
pub enum CoordinatorError {
    Layout(LayoutError),
    EndpointIdentityMismatch,
    InvalidEndpointAddress,
    EndpointSerialization(serde_json::Error),
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(error) => write!(formatter, "layout error: {error:?}"),
            Self::EndpointIdentityMismatch => {
                formatter.write_str("endpoint identity did not match peer")
            }
            Self::InvalidEndpointAddress => {
                formatter.write_str("endpoint address was empty or invalid")
            }
            Self::EndpointSerialization(error) => {
                write!(formatter, "endpoint address serialization failed: {error}")
            }
        }
    }
}

impl Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EndpointSerialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LayoutError> for CoordinatorError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CoordinatorResponse {
    Reservation(PaneReservation),
    Commit(LayoutCommit),
    Reject(LayoutReject),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TargetedLayoutReject {
    pub peer_id: Vec<u8>,
    pub reject: LayoutReject,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MembershipChange {
    pub commit: LayoutCommit,
    pub invalidated_reservation: Option<TargetedLayoutReject>,
}

impl LayoutCoordinator {
    pub fn new(
        coordinator_peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<Self, CoordinatorError> {
        Self::with_reservation_timeout(
            coordinator_peer_id,
            endpoint_addr,
            grid_rows,
            grid_cols,
            DEFAULT_RESERVATION_TIMEOUT,
            Instant::now(),
        )
    }

    pub fn with_reservation_timeout(
        coordinator_peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
        grid_rows: u16,
        grid_cols: u16,
        reservation_timeout: Duration,
        _now: Instant,
    ) -> Result<Self, CoordinatorError> {
        let endpoint_addr = serialized_endpoint(&coordinator_peer_id, endpoint_addr)?;
        Ok(Self {
            state: SessionState::new(coordinator_peer_id, endpoint_addr, grid_rows, grid_cols)?,
            reservations: BTreeMap::new(),
            reservation_timeout,
        })
    }

    pub fn session_snapshot(&self) -> Result<SessionSnapshot, CoordinatorError> {
        Ok(SessionSnapshot {
            state: Some(self.protocol_layout_state()?),
        })
    }

    pub fn admit(
        &mut self,
        peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
    ) -> Result<MembershipChange, CoordinatorError> {
        let endpoint_addr = serialized_endpoint(&peer_id, endpoint_addr)?;
        let invalidated = self
            .state
            .add_member(self.state.revision(), peer_id, endpoint_addr)?;
        self.membership_change(invalidated)
    }

    pub fn update_member_endpoint(
        &mut self,
        authenticated_peer_id: &[u8],
        endpoint_addr: EndpointAddr,
    ) -> Result<MembershipChange, CoordinatorError> {
        let endpoint_addr = serialized_endpoint(authenticated_peer_id, endpoint_addr)?;
        let invalidated = self.state.update_member_endpoint(
            self.state.revision(),
            authenticated_peer_id,
            endpoint_addr,
        )?;
        self.membership_change(invalidated)
    }

    pub fn handle_request(
        &mut self,
        authenticated_peer_id: &[u8],
        request: LayoutRequest,
    ) -> CoordinatorResponse {
        self.handle_request_at(authenticated_peer_id, request, Instant::now())
    }

    pub fn handle_request_at(
        &mut self,
        authenticated_peer_id: &[u8],
        request: LayoutRequest,
        now: Instant,
    ) -> CoordinatorResponse {
        let request_id = request.request_id;
        let action_count = usize::from(request.create_pane.is_some())
            + usize::from(request.delete_pane.is_some())
            + usize::from(request.create_tab.is_some())
            + usize::from(request.delete_tab.is_some());
        if request_id == 0 || request.base_revision == 0 || action_count != 1 {
            return reject(request_id, LayoutRejectReason::Malformed);
        }

        let result = if let Some(create) = request.create_pane {
            self.reserve_pane(
                authenticated_peer_id,
                request.base_revision,
                create,
                request_id,
                now,
            )
        } else if let Some(delete) = request.delete_pane {
            self.delete_pane(authenticated_peer_id, request.base_revision, delete)
        } else if let Some(create) = request.create_tab {
            self.reserve_tab(
                authenticated_peer_id,
                request.base_revision,
                create,
                request_id,
                now,
            )
        } else if let Some(delete) = request.delete_tab {
            self.delete_tab(authenticated_peer_id, request.base_revision, delete)
        } else {
            Err(LayoutError::InvalidSnapshot)
        };

        match result {
            Ok(response) => response,
            Err(error) => reject(request_id, reject_reason(&error)),
        }
    }

    pub fn handle_pane_ready(
        &mut self,
        authenticated_peer_id: &[u8],
        ready: PaneReady,
    ) -> CoordinatorResponse {
        if ready.reservation_id == 0 || ready.base_revision == 0 || ready.request_id == 0 {
            return reject(ready.request_id, LayoutRejectReason::Malformed);
        }
        let Some(context) = self.reservations.get(&ready.reservation_id) else {
            return reject(ready.request_id, LayoutRejectReason::ReservationFailure);
        };
        if context.request_id != ready.request_id {
            return reject(ready.request_id, LayoutRejectReason::ReservationFailure);
        }
        match self.state.pane_ready(
            authenticated_peer_id,
            ready.base_revision,
            ready.reservation_id,
        ) {
            Ok(_) => {
                self.reservations.remove(&ready.reservation_id);
                match self.layout_commit() {
                    Ok(commit) => CoordinatorResponse::Commit(commit),
                    Err(error) => reject(ready.request_id, reject_reason(&layout_error(error))),
                }
            }
            Err(error) => reject(ready.request_id, reject_reason(&error)),
        }
    }

    pub fn cancel_reservation(
        &mut self,
        authenticated_peer_id: &[u8],
        reservation_id: u64,
    ) -> Result<(), CoordinatorError> {
        self.state
            .cancel_reservation(authenticated_peer_id, reservation_id)?;
        self.reservations.remove(&reservation_id);
        Ok(())
    }

    pub fn expire_reservation(&mut self, reservation_id: u64) -> Result<(), CoordinatorError> {
        self.state.expire_reservation(reservation_id)?;
        self.reservations.remove(&reservation_id);
        Ok(())
    }

    pub fn expire_reservation_at(
        &mut self,
        now: Instant,
    ) -> Result<Option<TargetedLayoutReject>, CoordinatorError> {
        let Some((&reservation_id, context)) = self.reservations.iter().next() else {
            return Ok(None);
        };
        if now < context.deadline {
            return Ok(None);
        }
        let context = context.clone();
        self.state.expire_reservation(reservation_id)?;
        self.reservations.remove(&reservation_id);
        Ok(Some(targeted_reject(
            context.creator_peer_id,
            context.request_id,
            LayoutRejectReason::ReservationFailure,
        )))
    }

    pub fn handle_pane_failed(
        &mut self,
        authenticated_peer_id: &[u8],
        failed: PaneFailed,
    ) -> TargetedLayoutReject {
        if failed.reservation_id == 0 || failed.request_id == 0 || failed.base_revision == 0 {
            return targeted_reject(
                authenticated_peer_id.to_vec(),
                failed.request_id,
                LayoutRejectReason::Malformed,
            );
        }
        let Some(context) = self.reservations.get(&failed.reservation_id) else {
            return targeted_reject(
                authenticated_peer_id.to_vec(),
                failed.request_id,
                LayoutRejectReason::ReservationFailure,
            );
        };
        if context.request_id != failed.request_id {
            return targeted_reject(
                authenticated_peer_id.to_vec(),
                failed.request_id,
                LayoutRejectReason::ReservationFailure,
            );
        }
        match self.state.fail_reservation(
            authenticated_peer_id,
            failed.base_revision,
            failed.reservation_id,
        ) {
            Ok(()) => match self.reservations.remove(&failed.reservation_id) {
                Some(context) => targeted_reject(
                    context.creator_peer_id,
                    context.request_id,
                    LayoutRejectReason::ReservationFailure,
                ),
                None => targeted_reject(
                    authenticated_peer_id.to_vec(),
                    failed.request_id,
                    LayoutRejectReason::ReservationFailure,
                ),
            },
            Err(error) => targeted_reject(
                authenticated_peer_id.to_vec(),
                failed.request_id,
                reject_reason(&error),
            ),
        }
    }

    fn reserve_pane(
        &mut self,
        authenticated_peer_id: &[u8],
        base_revision: u64,
        create: CreatePane,
        request_id: u64,
        now: Instant,
    ) -> Result<CoordinatorResponse, LayoutError> {
        let axis = protocol_axis(create.axis)?;
        let (grid_rows, grid_cols) = protocol_grid(create.grid_rows, create.grid_cols)?;
        if create.target_pane_id == 0 {
            return Err(LayoutError::InvalidSnapshot);
        }
        let reservation = self.state.reserve_pane(
            authenticated_peer_id,
            base_revision,
            create.target_pane_id,
            axis,
            grid_rows,
            grid_cols,
        )?;
        self.reservations.insert(
            reservation.reservation_id,
            ReservationContext {
                request_id,
                creator_peer_id: authenticated_peer_id.to_vec(),
                deadline: now + self.reservation_timeout,
            },
        );
        Ok(CoordinatorResponse::Reservation(protocol_reservation(
            reservation,
        )))
    }

    fn reserve_tab(
        &mut self,
        authenticated_peer_id: &[u8],
        base_revision: u64,
        create: CreateTab,
        request_id: u64,
        now: Instant,
    ) -> Result<CoordinatorResponse, LayoutError> {
        let (grid_rows, grid_cols) = protocol_grid(create.grid_rows, create.grid_cols)?;
        let reservation =
            self.state
                .reserve_tab(authenticated_peer_id, base_revision, grid_rows, grid_cols)?;
        self.reservations.insert(
            reservation.reservation_id,
            ReservationContext {
                request_id,
                creator_peer_id: authenticated_peer_id.to_vec(),
                deadline: now + self.reservation_timeout,
            },
        );
        Ok(CoordinatorResponse::Reservation(protocol_reservation(
            reservation,
        )))
    }

    fn delete_pane(
        &mut self,
        authenticated_peer_id: &[u8],
        base_revision: u64,
        delete: DeletePane,
    ) -> Result<CoordinatorResponse, LayoutError> {
        if delete.pane_id == 0 {
            return Err(LayoutError::InvalidSnapshot);
        }
        self.state
            .delete_pane(authenticated_peer_id, base_revision, delete.pane_id)?;
        Ok(CoordinatorResponse::Commit(
            self.layout_commit().map_err(layout_error)?,
        ))
    }

    fn delete_tab(
        &mut self,
        authenticated_peer_id: &[u8],
        base_revision: u64,
        delete: DeleteTab,
    ) -> Result<CoordinatorResponse, LayoutError> {
        if delete.tab_id == 0 {
            return Err(LayoutError::InvalidSnapshot);
        }
        self.state
            .delete_tab(authenticated_peer_id, base_revision, delete.tab_id)?;
        Ok(CoordinatorResponse::Commit(
            self.layout_commit().map_err(layout_error)?,
        ))
    }

    fn layout_commit(&self) -> Result<LayoutCommit, CoordinatorError> {
        let state = self.protocol_layout_state()?;
        Ok(LayoutCommit {
            revision: state.revision,
            state: Some(state),
        })
    }

    fn protocol_layout_state(&self) -> Result<LayoutState, CoordinatorError> {
        let snapshot = self.state.snapshot();
        SessionState::validate_snapshot(&snapshot)?;
        Ok(protocol_layout_state(snapshot))
    }

    fn membership_change(
        &mut self,
        invalidated: Option<crate::layout::InvalidatedReservation>,
    ) -> Result<MembershipChange, CoordinatorError> {
        let invalidated_reservation = invalidated.and_then(|reservation| {
            self.reservations
                .remove(&reservation.reservation_id)
                .map(|context| {
                    targeted_reject(
                        reservation.creator_peer_id,
                        context.request_id,
                        LayoutRejectReason::Stale,
                    )
                })
        });
        Ok(MembershipChange {
            commit: self.layout_commit()?,
            invalidated_reservation,
        })
    }
}

fn serialized_endpoint(
    peer_id: &[u8],
    endpoint_addr: EndpointAddr,
) -> Result<Vec<u8>, CoordinatorError> {
    if endpoint_addr.is_empty() {
        return Err(CoordinatorError::InvalidEndpointAddress);
    }
    if endpoint_addr.id.as_bytes() != peer_id {
        return Err(CoordinatorError::EndpointIdentityMismatch);
    }
    serde_json::to_vec(&endpoint_addr).map_err(CoordinatorError::EndpointSerialization)
}

fn targeted_reject(
    peer_id: Vec<u8>,
    request_id: u64,
    reason: LayoutRejectReason,
) -> TargetedLayoutReject {
    TargetedLayoutReject {
        peer_id,
        reject: LayoutReject {
            request_id,
            reason: reason as i32,
        },
    }
}

fn layout_error(error: CoordinatorError) -> LayoutError {
    match error {
        CoordinatorError::Layout(error) => error,
        CoordinatorError::EndpointIdentityMismatch
        | CoordinatorError::InvalidEndpointAddress
        | CoordinatorError::EndpointSerialization(_) => LayoutError::InvalidSnapshot,
    }
}

fn protocol_grid(rows: u32, cols: u32) -> Result<(u16, u16), LayoutError> {
    let rows = u16::try_from(rows).map_err(|_| LayoutError::InvalidGrid)?;
    let cols = u16::try_from(cols).map_err(|_| LayoutError::InvalidGrid)?;
    if rows == 0 || cols == 0 {
        return Err(LayoutError::InvalidGrid);
    }
    Ok((rows, cols))
}

fn protocol_axis(axis: Option<i32>) -> Result<Axis, LayoutError> {
    match axis.and_then(|axis| SplitAxis::try_from(axis).ok()) {
        Some(SplitAxis::LeftRight) => Ok(Axis::LeftRight),
        Some(SplitAxis::TopBottom) => Ok(Axis::TopBottom),
        None => Err(LayoutError::InvalidSnapshot),
    }
}

fn protocol_reservation(reservation: crate::layout::PaneReservation) -> PaneReservation {
    PaneReservation {
        reservation_id: reservation.reservation_id,
        pane_id: reservation.pane_id,
        tab_id: reservation.tab_id,
    }
}

fn protocol_layout_state(snapshot: LayoutSnapshot) -> LayoutState {
    LayoutState {
        revision: snapshot.revision,
        members: snapshot
            .members
            .into_iter()
            .map(|member| MemberDescriptor {
                peer_id: member.peer_id,
                endpoint_addr: member.endpoint_addr,
            })
            .collect(),
        panes: snapshot
            .panes
            .into_values()
            .map(|pane| PaneDescriptor {
                pane_id: pane.pane_id,
                host_peer_id: pane.host_peer_id,
                grid_rows: u32::from(pane.grid_rows),
                grid_cols: u32::from(pane.grid_cols),
            })
            .collect(),
        tabs: snapshot
            .tabs
            .into_iter()
            .map(|tab| TabDescriptor {
                tab_id: tab.tab_id,
                root: Some(protocol_node(tab.root)),
            })
            .collect(),
    }
}

fn protocol_node(node: Node) -> LayoutNode {
    match node {
        Node::Leaf { pane_id } => LayoutNode {
            leaf_pane_id: Some(pane_id),
            split: None,
        },
        Node::Split {
            axis,
            first,
            second,
        } => LayoutNode {
            leaf_pane_id: None,
            split: Some(Box::new(LayoutSplit {
                axis: Some(match axis {
                    Axis::LeftRight => SplitAxis::LeftRight as i32,
                    Axis::TopBottom => SplitAxis::TopBottom as i32,
                }),
                first: Some(protocol_node(*first)),
                second: Some(protocol_node(*second)),
            })),
        },
    }
}

fn reject(request_id: u64, reason: LayoutRejectReason) -> CoordinatorResponse {
    CoordinatorResponse::Reject(LayoutReject {
        request_id,
        reason: reason as i32,
    })
}

fn reject_reason(error: &LayoutError) -> LayoutRejectReason {
    match error {
        LayoutError::StaleRevision { .. } | LayoutError::RevisionExhausted => {
            LayoutRejectReason::Stale
        }
        LayoutError::NotPaneHost { .. } | LayoutError::NotMember => LayoutRejectReason::NotHost,
        LayoutError::NotTabHost { .. } => LayoutRejectReason::MixedTab,
        LayoutError::MemberLimit
        | LayoutError::TabLimit
        | LayoutError::PaneLimit
        | LayoutError::SplitDepthLimit
        | LayoutError::IdExhausted => LayoutRejectReason::Limit,
        LayoutError::UnknownPane { .. } | LayoutError::UnknownTab { .. } => {
            LayoutRejectReason::UnknownId
        }
        LayoutError::LastPaneInTab { .. } | LayoutError::LastTab => {
            LayoutRejectReason::LastPaneOrTab
        }
        LayoutError::ReservationPending
        | LayoutError::UnknownReservation { .. }
        | LayoutError::ReservationCreatorMismatch
        | LayoutError::ReservationInvalid => LayoutRejectReason::ReservationFailure,
        LayoutError::InvalidPeerId
        | LayoutError::InvalidEndpointAddress
        | LayoutError::InvalidGrid
        | LayoutError::AlreadyMember
        | LayoutError::InvalidSnapshot => LayoutRejectReason::Malformed,
    }
}

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
    InitialLease(ControlLease),
    Lease(ControlLease),
    Disconnected,
}

#[derive(Clone)]
pub struct GuestControlSender {
    peer_id: Vec<u8>,
    pane_id: Vec<u8>,
    take_control_tx: mpsc::Sender<(u64, bool)>,
    input_tx: mpsc::Sender<(u64, Vec<u8>)>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ControlQueueError {
    Full,
    Closed,
}

impl GuestControlSender {
    pub fn try_take_control(
        &self,
        known_lease_epoch: u64,
        force: bool,
    ) -> Result<(), ControlQueueError> {
        self.take_control_tx
            .try_send((known_lease_epoch, force))
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
    pub endpoint_addr: EndpointAddr,
}

#[derive(Debug)]
pub enum SessionError {
    Transport(TransportError),
    Ticket(TicketError),
    Incoming(ConnectingError),
    TimedOut(&'static str),
    InvalidJoin,
    InvalidJoinEndpointAddress,
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
            Self::InvalidJoinEndpointAddress => {
                formatter.write_str("Join endpoint address is malformed or does not match the peer")
            }
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
            | Self::InvalidJoinEndpointAddress
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
        let endpoint_addr: EndpointAddr = serde_json::from_slice(&join.endpoint_addr)
            .map_err(|_| SessionError::InvalidJoinEndpointAddress)?;
        if endpoint_addr.is_empty()
            || endpoint_addr.id.as_bytes() != remote_id.as_bytes()
            || endpoint_addr.id.as_bytes() != join.peer_id.as_slice()
            || endpoint_addr.id.as_bytes() != envelope.sender_peer_id.as_slice()
        {
            return Err(SessionError::InvalidJoinEndpointAddress);
        }

        let coordinator = self.transport.endpoint_id();
        let receipt = JoinReceipt {
            session_id: self.ticket.session_id().to_vec(),
            admitted_peer_id: remote_id.as_bytes().to_vec(),
            coordinator_peer_id: coordinator.as_bytes().to_vec(),
            endpoint_addr,
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
        let _ = events_tx.try_send(GuestEvent::InitialLease(ControlLease {
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
                    endpoint_addr: serde_json::to_vec(&transport.endpoint_addr())
                        .expect("endpoint address should serialize"),
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
        endpoint_addr: ticket.endpoint_addr().clone(),
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
    mut take_control_rx: mpsc::Receiver<(u64, bool)>,
    mut input_rx: mpsc::Receiver<(u64, Vec<u8>)>,
    peer_id: Vec<u8>,
    pane_id: Vec<u8>,
) {
    loop {
        tokio::select! {
            biased;
            Some((known_lease_epoch, force)) = take_control_rx.recv() => {
                let _ = writer.write_next(&Envelope {
                    version: PROTOCOL_VERSION,
                    sender_peer_id: peer_id.clone(),
                    body: Some(envelope::Body::TakeControl(TakeControl {
                        pane_id: pane_id.clone(),
                        requester_peer_id: peer_id.clone(),
                        known_lease_epoch,
                        force,
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
