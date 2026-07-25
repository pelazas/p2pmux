//! Authenticated Join/Welcome session handshakes over Iroh bi-streams.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{ConnectingError, Connection, Incoming, SendStream},
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
        PaneReservation, PaneSubscribe, SessionSnapshot, Snapshot, SplitAxis, TabDescriptor,
        TakeControl, Welcome, envelope,
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

    pub fn remove_member(&mut self, peer_id: &[u8]) -> Result<MembershipChange, CoordinatorError> {
        let invalidated = self.state.remove_member(peer_id)?;
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

    pub fn expire_reservation_if_at(
        &mut self,
        reservation_id: u64,
        now: Instant,
    ) -> Result<Option<TargetedLayoutReject>, CoordinatorError> {
        if !self.reservations.contains_key(&reservation_id) {
            return Ok(None);
        }
        self.expire_reservation_at(now)
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

#[derive(Clone)]
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
    close_transport: bool,
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
        if self.close_transport {
            self.transport.close().await;
        }
    }
}

/// Canonical on-stream representation for a layout pane ID.
pub fn pane_wire_id(pane_id: u64) -> Vec<u8> {
    pane_id.to_be_bytes().to_vec()
}

/// Direct pane service for one session member. Its roster is updated from the authoritative
/// layout state before it accepts subscriptions.
#[derive(Clone)]
pub struct PaneServer {
    transport: Transport,
    session_id: Vec<u8>,
    local_peer_id: Vec<u8>,
    members: Arc<Mutex<BTreeMap<Vec<u8>, EndpointAddr>>>,
    panes: Arc<Mutex<BTreeMap<u64, HostPaneChannels>>>,
}

impl PaneServer {
    pub fn from_host_session(host: &HostSession) -> Self {
        Self {
            transport: host.transport.clone(),
            session_id: host.ticket.session_id().to_vec(),
            local_peer_id: host.transport.endpoint_id().as_bytes().to_vec(),
            members: Arc::new(Mutex::new(BTreeMap::new())),
            panes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn new(transport: Transport, session_id: Vec<u8>) -> Result<Self, SessionError> {
        if session_id.is_empty() {
            return Err(SessionError::InvalidPostWelcome);
        }
        Ok(Self {
            local_peer_id: transport.endpoint_id().as_bytes().to_vec(),
            transport,
            session_id,
            members: Arc::new(Mutex::new(BTreeMap::new())),
            panes: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn add_member(
        &self,
        peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
    ) -> Result<(), SessionError> {
        if peer_id.is_empty() || endpoint_addr.is_empty() || peer_id != endpoint_addr.id.as_bytes()
        {
            return Err(SessionError::InvalidPostWelcome);
        }
        self.members
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .insert(peer_id, endpoint_addr);
        Ok(())
    }

    pub fn replace_members(
        &self,
        members: Vec<(Vec<u8>, EndpointAddr)>,
    ) -> Result<(), SessionError> {
        let mut next = BTreeMap::new();
        for (peer_id, endpoint_addr) in members {
            if peer_id.is_empty()
                || endpoint_addr.is_empty()
                || peer_id != endpoint_addr.id.as_bytes()
            {
                return Err(SessionError::InvalidPostWelcome);
            }
            next.insert(peer_id, endpoint_addr);
        }
        *self.members.lock().map_err(|_| SessionError::PeerTask)? = next;
        Ok(())
    }

    /// Replaces the admission roster from a verified authoritative layout commit. The caller's
    /// layout reconciler is responsible for invoking this before accepting new direct panes.
    pub fn replace_roster_from_layout(&self, state: &LayoutState) -> Result<(), SessionError> {
        let members = state
            .members
            .iter()
            .map(|member| {
                serde_json::from_slice(&member.endpoint_addr)
                    .map(|endpoint_addr| (member.peer_id.clone(), endpoint_addr))
                    .map_err(|_| SessionError::InvalidPostWelcome)
            })
            .collect::<Result<Vec<(Vec<u8>, EndpointAddr)>, _>>();
        let members = members?;
        self.replace_members(members)
    }

    pub fn register_pane(
        &self,
        descriptor: PaneDescriptor,
        channels: HostPaneChannels,
    ) -> Result<(), SessionError> {
        if descriptor.pane_id == 0
            || descriptor.host_peer_id != self.local_peer_id
            || channels.host_peer_id != self.local_peer_id
            || channels.pane_id != pane_wire_id(descriptor.pane_id)
        {
            return Err(SessionError::InvalidPostWelcome);
        }
        self.panes
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .insert(descriptor.pane_id, channels);
        Ok(())
    }

    /// Registers a locally hosted pane after its PTY is ready; the reconciler must remove it when
    /// a later authoritative commit deletes the pane.
    pub fn register_local_pane(
        &self,
        descriptor: PaneDescriptor,
        channels: HostPaneChannels,
    ) -> Result<(), SessionError> {
        self.register_pane(descriptor, channels)
    }

    pub fn remove_pane(&self, pane_id: u64) -> Result<Option<HostPaneChannels>, SessionError> {
        Ok(self
            .panes
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .remove(&pane_id))
    }

    pub fn remove_local_pane(
        &self,
        pane_id: u64,
    ) -> Result<Option<HostPaneChannels>, SessionError> {
        self.remove_pane(pane_id)
    }

    pub async fn accept_one(&self) -> Result<(), SessionError> {
        let incoming = self.transport.accept_incoming().await?;
        self.serve_incoming(incoming).await
    }

    /// Owns an endpoint accept loop and keeps each pane subscription independent from later
    /// arrivals. A runtime that multiplexes layout joins and panes should use
    /// [`IncomingDispatcher`] instead of starting this loop alongside another acceptor.
    pub async fn accept_loop(&self) -> Result<(), SessionError> {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = self.transport.accept_incoming() => {
                    let incoming = incoming?;
                    let server = self.clone();
                    tasks.spawn(async move {
                        let _ = server.serve_incoming(incoming).await;
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    }

    pub async fn serve_incoming(&self, incoming: Incoming) -> Result<(), SessionError> {
        let connection = timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| SessionError::TimedOut("incoming pane connection"))?
            .map_err(SessionError::Incoming)?;
        let result = self.serve_connection(&connection).await;
        if result.is_err() {
            connection.close(0u8.into(), b"");
        }
        result
    }

    async fn serve_connection(&self, connection: &Connection) -> Result<(), SessionError> {
        let (_subscribe_writer, mut subscribe_reader) =
            self.transport.accept_bi(connection).await?;
        let envelope = self.transport.read_frame(&mut subscribe_reader).await?;
        self.serve_subscribe_connection(connection, envelope).await
    }

    async fn serve_subscribe_connection(
        &self,
        connection: &Connection,
        envelope: Envelope,
    ) -> Result<(), SessionError> {
        let remote_peer_id = connection.remote_id().as_bytes().to_vec();
        let subscribe = match envelope.body {
            Some(envelope::Body::PaneSubscribe(subscribe)) => subscribe,
            _ => return Err(SessionError::InvalidPostWelcome),
        };
        if envelope.sender_peer_id != remote_peer_id
            || subscribe.peer_id != remote_peer_id
            || subscribe.session_id != self.session_id
            || !self
                .members
                .lock()
                .map_err(|_| SessionError::PeerTask)?
                .contains_key(&remote_peer_id)
        {
            return Err(SessionError::UnauthenticatedPeer);
        }
        let pane = self
            .panes
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .get(&subscribe.pane_id)
            .cloned()
            .ok_or(SessionError::InvalidPostWelcome)?;
        if pane.host_peer_id != self.local_peer_id
            || pane.pane_id != pane_wire_id(subscribe.pane_id)
        {
            return Err(SessionError::InvalidPostWelcome);
        }
        serve_direct_pane_streams(&self.transport, connection, remote_peer_id, pane).await
    }

    pub async fn close(&self) {
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
    Coordinator(CoordinatorError),
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
            Self::Coordinator(error) => write!(formatter, "layout coordinator error: {error}"),
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
            Self::Coordinator(error) => Some(error),
        }
    }
}

impl From<TransportError> for SessionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<CoordinatorError> for SessionError {
    fn from(error: CoordinatorError) -> Self {
        Self::Coordinator(error)
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

    /// Compatibility entry point for serving direct layout panes from a coordinator host.
    pub fn pane_server(&self) -> PaneServer {
        PaneServer::from_host_session(self)
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
        let (mut send, mut recv) = self.transport.accept_bi(connection).await?;
        let envelope = self.transport.read_frame(&mut recv).await?;
        self.handshake_join(connection, &mut send, envelope).await
    }

    async fn handshake_join(
        &self,
        connection: &Connection,
        send: &mut SendStream,
        envelope: Envelope,
    ) -> Result<JoinReceipt, SessionError> {
        let remote_id = connection.remote_id();
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
                send,
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

/// A coordinator that keeps the shared layout authoritative while each admitted member has one
/// independent, long-lived control stream. Pane data streams deliberately remain outside this
/// type so they can later connect directly to their pane hosts.
#[derive(Clone)]
pub struct SharedLayoutHost {
    host: HostSession,
    coordinator: Arc<Mutex<LayoutCoordinator>>,
    peers: Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    reservation_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LayoutControlEvent {
    Snapshot(SessionSnapshot),
    Reservation(PaneReservation),
    Commit(LayoutCommit),
    Reject(LayoutReject),
    Disconnected,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LayoutControlQueueError {
    Full,
    Closed,
}

enum LayoutClientMessage {
    Request(LayoutRequest),
    Ready(PaneReady),
    Failed(PaneFailed),
}

const TARGETED_CONTROL_QUEUE_CAPACITY: usize = 16;

#[derive(Clone)]
struct ControlMailbox {
    initial_tx: mpsc::Sender<Envelope>,
    state_tx: watch::Sender<Option<SequencedEnvelope>>,
    targeted_tx: mpsc::Sender<SequencedEnvelope>,
    next_sequence: Arc<AtomicU64>,
}

#[derive(Clone)]
struct SequencedEnvelope {
    sequence: u64,
    envelope: Envelope,
}

impl ControlMailbox {
    fn new() -> (
        Self,
        mpsc::Receiver<Envelope>,
        watch::Receiver<Option<SequencedEnvelope>>,
        mpsc::Receiver<SequencedEnvelope>,
    ) {
        let (initial_tx, initial_rx) = mpsc::channel(1);
        let (state_tx, state_rx) = watch::channel(None);
        let (targeted_tx, targeted_rx) = mpsc::channel(TARGETED_CONTROL_QUEUE_CAPACITY);
        (
            Self {
                initial_tx,
                state_tx,
                targeted_tx,
                next_sequence: Arc::new(AtomicU64::new(1)),
            },
            initial_rx,
            state_rx,
            targeted_rx,
        )
    }

    fn enqueue_initial(&self, envelope: Envelope) -> bool {
        self.initial_tx.try_send(envelope).is_ok()
    }

    fn publish_state(&self, envelope: Envelope) {
        self.state_tx.send_replace(Some(self.sequenced(envelope)));
    }

    fn enqueue_targeted(&self, envelope: Envelope) -> bool {
        self.targeted_tx.try_send(self.sequenced(envelope)).is_ok()
    }

    fn sequenced(&self, envelope: Envelope) -> SequencedEnvelope {
        SequencedEnvelope {
            sequence: self.next_sequence.fetch_add(1, Ordering::SeqCst),
            envelope,
        }
    }
}

struct ControlPeer {
    mailbox: ControlMailbox,
    reader_abort: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    connection: Option<Connection>,
}

impl ControlPeer {
    #[cfg(test)]
    fn new(
        mailbox: ControlMailbox,
        reader_abort: tokio::task::AbortHandle,
        connection: Option<Connection>,
    ) -> Self {
        Self {
            mailbox,
            reader_abort: Arc::new(Mutex::new(Some(reader_abort))),
            connection,
        }
    }

    fn pending_reader(mailbox: ControlMailbox, connection: Connection) -> Self {
        Self {
            mailbox,
            reader_abort: Arc::new(Mutex::new(None)),
            connection: Some(connection),
        }
    }

    fn shutdown(self) {
        if let Ok(mut slot) = self.reader_abort.lock()
            && let Some(reader_abort) = slot.take()
        {
            reader_abort.abort();
        }
        if let Some(connection) = self.connection {
            connection.close(0u8.into(), b"");
        }
    }
}

trait ControlFrameSink: Send {
    fn write_control<'a>(
        &'a mut self,
        envelope: &'a Envelope,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

impl ControlFrameSink for crate::transport::FrameWriter {
    fn write_control<'a>(
        &'a mut self,
        envelope: &'a Envelope,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move { self.write_next(envelope).await.is_ok() })
    }
}

/// The member side of a shared-layout control connection.
pub struct SharedLayoutMember {
    pub peer_id: Vec<u8>,
    pub coordinator_peer_id: Vec<u8>,
    pub events: mpsc::Receiver<LayoutControlEvent>,
    outbound: mpsc::Sender<LayoutClientMessage>,
    transport: Transport,
    connection: Connection,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl SharedLayoutMember {
    /// Creates the member-side direct pane service; callers supply the session ID from their
    /// authenticated join ticket and keep its roster synchronized with layout commits.
    pub fn pane_server(&self, session_id: Vec<u8>) -> Result<PaneServer, SessionError> {
        PaneServer::new(self.transport.clone(), session_id)
    }

    pub fn try_request(&self, request: LayoutRequest) -> Result<(), LayoutControlQueueError> {
        self.outbound
            .try_send(LayoutClientMessage::Request(request))
            .map_err(layout_queue_error)
    }

    pub fn try_ready(&self, ready: PaneReady) -> Result<(), LayoutControlQueueError> {
        self.outbound
            .try_send(LayoutClientMessage::Ready(ready))
            .map_err(layout_queue_error)
    }

    pub fn try_failed(&self, failed: PaneFailed) -> Result<(), LayoutControlQueueError> {
        self.outbound
            .try_send(LayoutClientMessage::Failed(failed))
            .map_err(layout_queue_error)
    }

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

impl Drop for SharedLayoutMember {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.connection.close(0u8.into(), b"");
    }
}

impl SharedLayoutHost {
    pub fn new(host: HostSession, grid_rows: u16, grid_cols: u16) -> Result<Self, SessionError> {
        Self::with_reservation_timeout(host, grid_rows, grid_cols, DEFAULT_RESERVATION_TIMEOUT)
    }

    pub fn with_reservation_timeout(
        host: HostSession,
        grid_rows: u16,
        grid_cols: u16,
        reservation_timeout: Duration,
    ) -> Result<Self, SessionError> {
        let coordinator_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let coordinator = LayoutCoordinator::with_reservation_timeout(
            coordinator_peer_id,
            host.ticket().endpoint_addr().clone(),
            grid_rows,
            grid_cols,
            reservation_timeout,
            Instant::now(),
        )?;
        Ok(Self {
            host,
            coordinator: Arc::new(Mutex::new(coordinator)),
            peers: Arc::new(Mutex::new(BTreeMap::new())),
            reservation_timeout,
        })
    }

    pub fn ticket(&self) -> &JoinTicket {
        self.host.ticket()
    }

    /// Creates the coordinator-owned pane registry. The runtime must keep this registry's roster
    /// and local pane registrations synchronized from authoritative layout commits.
    pub fn pane_server(&self) -> PaneServer {
        self.host.pane_server()
    }

    /// Creates the only incoming acceptor for an endpoint that serves both layout control and
    /// direct panes. Do not run `accept_one_member` or `PaneServer::accept_loop` beside it.
    pub fn incoming_dispatcher(
        &self,
        panes: PaneServer,
    ) -> Result<IncomingDispatcher, SessionError> {
        IncomingDispatcher::new(self.clone(), panes)
    }

    /// Accept one authenticated member and then its persistent control stream.
    pub async fn accept_one_member(&self) -> Result<JoinReceipt, SessionError> {
        let incoming = self.host.accept_incoming().await?;
        let connection = timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| SessionError::TimedOut("incoming connection"))?
            .map_err(SessionError::Incoming)?;
        let result = async {
            let (mut join_writer, mut join_reader) =
                self.host.transport.accept_bi(&connection).await?;
            let envelope = self.host.transport.read_frame(&mut join_reader).await?;
            self.accept_join_connection(connection.clone(), &mut join_writer, envelope)
                .await
        }
        .await;
        if result.is_err() {
            connection.close(0u8.into(), b"");
        }
        result
    }

    async fn accept_join_connection(
        &self,
        connection: Connection,
        join_writer: &mut SendStream,
        envelope: Envelope,
    ) -> Result<JoinReceipt, SessionError> {
        let mut admitted_peer_id = None;
        let result = async {
            let receipt = self
                .host
                .handshake_join(&connection, join_writer, envelope)
                .await?;
            {
                let mut coordinator_guard = self
                    .coordinator
                    .lock()
                    .map_err(|_| SessionError::PeerTask)?;
                let membership = coordinator_guard.admit(
                    receipt.admitted_peer_id.clone(),
                    receipt.endpoint_addr.clone(),
                )?;

                // Existing members see the authoritative membership commit before the joining
                // member receives its snapshot. The joiner snapshot already contains that commit.
                self.broadcast_commit(membership.commit.clone());
                if let Some(reject) = membership.invalidated_reservation {
                    self.send_reject(reject);
                }
            }
            admitted_peer_id = Some(receipt.admitted_peer_id.clone());

            let (writer, reader) = self.transport_open_control(&connection).await?;
            let peer_id = receipt.admitted_peer_id.clone();
            let (mailbox, initial_rx, state_rx, targeted_rx) = ControlMailbox::new();
            let peer = ControlPeer::pending_reader(mailbox.clone(), connection.clone());
            let reader_abort = peer.reader_abort.clone();
            self.peers
                .lock()
                .map_err(|_| SessionError::PeerTask)?
                .insert(peer_id.clone(), peer);
            let snapshot = self
                .coordinator
                .lock()
                .map_err(|_| SessionError::PeerTask)?
                .session_snapshot()?;
            mailbox
                .enqueue_initial(coordinator_envelope(
                    self.host.ticket().endpoint_addr().id.as_bytes(),
                    envelope::Body::SessionSnapshot(snapshot),
                ))
                .then_some(())
                .ok_or(SessionError::PeerTask)?;

            let reader_task = tokio::spawn(layout_host_reader_task(
                reader,
                peer_id,
                self.host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                self.coordinator.clone(),
                self.peers.clone(),
                self.reservation_timeout,
            ));
            if let Ok(mut slot) = reader_abort.lock() {
                *slot = Some(reader_task.abort_handle());
            }
            tokio::spawn(layout_peer_writer_task(
                writer,
                initial_rx,
                state_rx,
                targeted_rx,
                receipt.admitted_peer_id.clone(),
                self.peers.clone(),
                Some((
                    self.coordinator.clone(),
                    self.host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                )),
            ));
            Ok(receipt)
        }
        .await;
        if result.is_err() {
            if let Some(peer_id) = admitted_peer_id {
                disconnect_or_remove(
                    &self.peers,
                    Some(&(
                        self.coordinator.clone(),
                        self.host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                    )),
                    &peer_id,
                );
            }
            connection.close(0u8.into(), b"");
        }
        result
    }

    pub async fn close(&self) {
        self.host.close().await;
    }

    async fn transport_open_control(
        &self,
        connection: &Connection,
    ) -> Result<(crate::transport::FrameWriter, crate::transport::FrameReader), SessionError> {
        self.host
            .transport
            .open_framed_bi(connection)
            .await
            .map_err(Into::into)
    }

    fn broadcast_commit(&self, commit: LayoutCommit) {
        broadcast_envelope(
            &self.peers,
            coordinator_envelope(
                self.host.ticket().endpoint_addr().id.as_bytes(),
                envelope::Body::LayoutCommit(commit),
            ),
        );
    }

    fn send_reject(&self, targeted: TargetedLayoutReject) {
        send_to_peer(
            &self.peers,
            &targeted.peer_id,
            coordinator_envelope(
                self.host.ticket().endpoint_addr().id.as_bytes(),
                envelope::Body::LayoutReject(targeted.reject),
            ),
        );
    }
}

/// Sole endpoint-level acceptor for a shared-layout runtime. The caller owns roster replacement
/// and local pane registration on `PaneServer`; this type only authenticates and routes each new
/// connection.
#[derive(Clone)]
pub struct IncomingDispatcher {
    layout: SharedLayoutHost,
    panes: PaneServer,
}

impl IncomingDispatcher {
    pub fn new(layout: SharedLayoutHost, panes: PaneServer) -> Result<Self, SessionError> {
        if panes.local_peer_id != layout.host.transport.endpoint_id().as_bytes()
            || panes.session_id.as_slice() != layout.ticket().session_id()
        {
            return Err(SessionError::InvalidPostWelcome);
        }
        Ok(Self { layout, panes })
    }

    /// Continually accepts connections and dispatches each to an independently managed task.
    pub async fn accept_loop(&self) -> Result<(), SessionError> {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = self.layout.host.accept_incoming() => {
                    let incoming = incoming?;
                    let dispatcher = self.clone();
                    tasks.spawn(async move {
                        let _ = dispatcher.dispatch_incoming(incoming).await;
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    }

    async fn dispatch_incoming(&self, incoming: Incoming) -> Result<(), SessionError> {
        let connection = timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| SessionError::TimedOut("incoming connection"))?
            .map_err(SessionError::Incoming)?;
        let result = async {
            let (mut first_writer, mut first_reader) =
                self.layout.host.transport.accept_bi(&connection).await?;
            let envelope = self
                .layout
                .host
                .transport
                .read_frame(&mut first_reader)
                .await?;
            match envelope.body.as_ref() {
                Some(envelope::Body::Join(_)) => self
                    .layout
                    .accept_join_connection(connection.clone(), &mut first_writer, envelope)
                    .await
                    .map(|_| ()),
                Some(envelope::Body::PaneSubscribe(_)) => {
                    drop(first_writer);
                    self.panes
                        .serve_subscribe_connection(&connection, envelope)
                        .await
                }
                _ => Err(SessionError::InvalidPostWelcome),
            }
        }
        .await;
        if result.is_err() {
            connection.close(0u8.into(), b"");
        }
        result
    }
}

fn layout_queue_error<T>(error: mpsc::error::TrySendError<T>) -> LayoutControlQueueError {
    match error {
        mpsc::error::TrySendError::Full(_) => LayoutControlQueueError::Full,
        mpsc::error::TrySendError::Closed(_) => LayoutControlQueueError::Closed,
    }
}

fn coordinator_envelope(sender_peer_id: &[u8], body: envelope::Body) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: sender_peer_id.to_vec(),
        body: Some(body),
    }
}

fn broadcast_envelope(peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>, envelope: Envelope) {
    let peers = match peers.lock() {
        Ok(peers) => peers,
        Err(_) => return,
    };
    for peer in peers.values() {
        // Full-state broadcasts are revisioned and supersede older ones, so each peer needs only
        // the latest state while its writer catches up.
        peer.mailbox.publish_state(envelope.clone());
    }
}

fn send_to_peer(
    peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    peer_id: &[u8],
    envelope: Envelope,
) {
    let failed_peer = match peers.lock() {
        Ok(mut peers) => match peers.get(peer_id) {
            Some(peer) if peer.mailbox.enqueue_targeted(envelope) => None,
            Some(_) => peers.remove(peer_id),
            None => None,
        },
        Err(_) => return,
    };
    if let Some(peer) = failed_peer {
        peer.shutdown();
    }
}

fn remove_control_peer(peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>, peer_id: &[u8]) {
    let peer = peers
        .lock()
        .ok()
        .and_then(|mut peers| peers.remove(peer_id));
    if let Some(peer) = peer {
        peer.shutdown();
    }
}

fn disconnect_or_remove(
    peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    coordinator: Option<&(Arc<Mutex<LayoutCoordinator>>, Vec<u8>)>,
    peer_id: &[u8],
) {
    let Some((coordinator, coordinator_peer_id)) = coordinator else {
        remove_control_peer(peers, peer_id);
        return;
    };
    let Ok(mut coordinator) = coordinator.lock() else {
        remove_control_peer(peers, peer_id);
        return;
    };
    remove_control_peer(peers, peer_id);
    if let Ok(change) = coordinator.remove_member(peer_id) {
        broadcast_envelope(
            peers,
            coordinator_envelope(
                coordinator_peer_id,
                envelope::Body::LayoutCommit(change.commit),
            ),
        );
        if let Some(reject) = change.invalidated_reservation {
            send_to_peer(
                peers,
                &reject.peer_id,
                coordinator_envelope(
                    coordinator_peer_id,
                    envelope::Body::LayoutReject(reject.reject),
                ),
            );
        }
    }
}

/// Join a coordinator and establish the persistent member-to-coordinator layout stream.
pub async fn join_layout(
    transport: Transport,
    ticket: JoinTicket,
) -> Result<SharedLayoutMember, SessionError> {
    let connection = transport.connect(ticket.endpoint_addr().clone()).await?;
    let result = async {
        let receipt = join_handshake(&transport, &connection, &ticket).await?;
        let (writer, reader) = transport.accept_framed_bi(&connection).await?;
        let (events_tx, events) = mpsc::channel(128);
        let (outbound, outbound_rx) = mpsc::channel(64);
        let peer_id = transport.endpoint_id().as_bytes().to_vec();
        let coordinator_peer_id = receipt.coordinator_peer_id;
        let tasks = vec![
            tokio::spawn(layout_member_reader_task(
                reader,
                events_tx,
                coordinator_peer_id.clone(),
            )),
            tokio::spawn(layout_member_writer_task(
                writer,
                outbound_rx,
                peer_id.clone(),
            )),
        ];
        Ok(SharedLayoutMember {
            peer_id,
            coordinator_peer_id,
            events,
            outbound,
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

async fn layout_peer_writer_task<W>(
    mut writer: W,
    mut initial_rx: mpsc::Receiver<Envelope>,
    mut state_rx: watch::Receiver<Option<SequencedEnvelope>>,
    mut targeted_rx: mpsc::Receiver<SequencedEnvelope>,
    peer_id: Vec<u8>,
    peers: Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    coordinator: Option<(Arc<Mutex<LayoutCoordinator>>, Vec<u8>)>,
) where
    W: ControlFrameSink + 'static,
{
    let Some(initial) = initial_rx.recv().await else {
        disconnect_or_remove(&peers, coordinator.as_ref(), &peer_id);
        return;
    };
    if !writer.write_control(&initial).await {
        disconnect_or_remove(&peers, coordinator.as_ref(), &peer_id);
        return;
    }
    let mut targeted_open = true;
    let mut state_open = true;
    let mut pending_targeted = None;
    let mut pending_state = None;
    loop {
        if pending_targeted.is_none() {
            match targeted_rx.try_recv() {
                Ok(targeted) => pending_targeted = Some(targeted),
                Err(mpsc::error::TryRecvError::Disconnected) => targeted_open = false,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
        if pending_state.is_none() && state_open && state_rx.has_changed().unwrap_or(false) {
            pending_state = state_rx.borrow_and_update().clone();
        }

        let next = match (&pending_state, &pending_targeted) {
            (Some(state), Some(targeted)) if state.sequence < targeted.sequence => {
                pending_state.take()
            }
            (Some(_), Some(_)) | (None, Some(_)) => pending_targeted.take(),
            (Some(_), None) => pending_state.take(),
            (None, None) => {
                tokio::select! {
                    targeted = targeted_rx.recv(), if targeted_open => match targeted {
                        Some(targeted) => pending_targeted = Some(targeted),
                        None => targeted_open = false,
                    },
                    changed = state_rx.changed(), if state_open => match changed {
                        Ok(()) => pending_state = state_rx.borrow_and_update().clone(),
                        Err(_) => state_open = false,
                    },
                    else => return,
                }
                continue;
            }
        };
        let Some(next) = next else {
            continue;
        };
        if !writer.write_control(&next.envelope).await {
            disconnect_or_remove(&peers, coordinator.as_ref(), &peer_id);
            return;
        }
    }
}

async fn layout_member_writer_task(
    mut writer: crate::transport::FrameWriter,
    mut outbound: mpsc::Receiver<LayoutClientMessage>,
    peer_id: Vec<u8>,
) {
    while let Some(message) = outbound.recv().await {
        let body = match message {
            LayoutClientMessage::Request(request) => envelope::Body::LayoutRequest(request),
            LayoutClientMessage::Ready(ready) => envelope::Body::PaneReady(ready),
            LayoutClientMessage::Failed(failed) => envelope::Body::PaneFailed(failed),
        };
        if writer
            .write_next(&coordinator_envelope(&peer_id, body))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn layout_member_reader_task(
    mut reader: crate::transport::FrameReader,
    events_tx: mpsc::Sender<LayoutControlEvent>,
    coordinator_peer_id: Vec<u8>,
) {
    while let Ok(Some(envelope)) = reader.read_next().await {
        if envelope.sender_peer_id != coordinator_peer_id {
            break;
        }
        let event = match envelope.body {
            Some(envelope::Body::SessionSnapshot(snapshot)) => {
                LayoutControlEvent::Snapshot(snapshot)
            }
            Some(envelope::Body::PaneReservation(reservation)) => {
                LayoutControlEvent::Reservation(reservation)
            }
            Some(envelope::Body::LayoutCommit(commit)) => LayoutControlEvent::Commit(commit),
            Some(envelope::Body::LayoutReject(reject)) => LayoutControlEvent::Reject(reject),
            _ => break,
        };
        if events_tx.send(event).await.is_err() {
            return;
        }
    }
    let _ = events_tx.send(LayoutControlEvent::Disconnected).await;
}

async fn layout_host_reader_task(
    mut reader: crate::transport::FrameReader,
    peer_id: Vec<u8>,
    coordinator_peer_id: Vec<u8>,
    coordinator: Arc<Mutex<LayoutCoordinator>>,
    peers: Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    reservation_timeout: Duration,
) {
    while let Ok(Some(envelope)) = reader.read_next().await {
        if envelope.sender_peer_id != peer_id {
            break;
        }
        match envelope.body {
            Some(envelope::Body::LayoutRequest(request)) => {
                let mut coordinator_guard = match coordinator.lock() {
                    Ok(coordinator) => coordinator,
                    Err(_) => break,
                };
                let response = coordinator_guard.handle_request(&peer_id, request);
                let reservation = match &response {
                    CoordinatorResponse::Reservation(reservation) => {
                        Some(reservation.reservation_id)
                    }
                    _ => None,
                };
                dispatch_coordinator_response(&peers, &peer_id, &coordinator_peer_id, response);
                drop(coordinator_guard);
                if let Some(reservation_id) = reservation {
                    tokio::spawn(reservation_expiry_task(
                        reservation_id,
                        reservation_timeout,
                        coordinator.clone(),
                        peers.clone(),
                        coordinator_peer_id.clone(),
                    ));
                }
            }
            Some(envelope::Body::PaneReady(ready)) => {
                let mut coordinator_guard = match coordinator.lock() {
                    Ok(coordinator) => coordinator,
                    Err(_) => break,
                };
                let response = coordinator_guard.handle_pane_ready(&peer_id, ready);
                dispatch_coordinator_response(&peers, &peer_id, &coordinator_peer_id, response);
                drop(coordinator_guard);
            }
            Some(envelope::Body::PaneFailed(failed)) => {
                let mut coordinator_guard = match coordinator.lock() {
                    Ok(coordinator) => coordinator,
                    Err(_) => break,
                };
                let reject = coordinator_guard.handle_pane_failed(&peer_id, failed);
                send_to_peer(
                    &peers,
                    &reject.peer_id,
                    coordinator_envelope(
                        &coordinator_peer_id,
                        envelope::Body::LayoutReject(reject.reject),
                    ),
                );
                drop(coordinator_guard);
            }
            _ => break,
        }
    }
    disconnect_or_remove(&peers, Some(&(coordinator, coordinator_peer_id)), &peer_id);
}

async fn reservation_expiry_task(
    reservation_id: u64,
    reservation_timeout: Duration,
    coordinator: Arc<Mutex<LayoutCoordinator>>,
    peers: Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    coordinator_peer_id: Vec<u8>,
) {
    tokio::time::sleep(reservation_timeout).await;
    let Ok(mut coordinator) = coordinator.lock() else {
        return;
    };
    let Ok(Some(reject)) = coordinator.expire_reservation_if_at(reservation_id, Instant::now())
    else {
        return;
    };
    send_to_peer(
        &peers,
        &reject.peer_id,
        coordinator_envelope(
            &coordinator_peer_id,
            envelope::Body::LayoutReject(reject.reject),
        ),
    );
}

fn dispatch_coordinator_response(
    peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    requester_peer_id: &[u8],
    coordinator_peer_id: &[u8],
    response: CoordinatorResponse,
) {
    match response {
        CoordinatorResponse::Reservation(reservation) => send_to_peer(
            peers,
            requester_peer_id,
            coordinator_envelope(
                coordinator_peer_id,
                envelope::Body::PaneReservation(reservation),
            ),
        ),
        CoordinatorResponse::Commit(commit) => broadcast_envelope(
            peers,
            coordinator_envelope(coordinator_peer_id, envelope::Body::LayoutCommit(commit)),
        ),
        CoordinatorResponse::Reject(reject) => send_to_peer(
            peers,
            requester_peer_id,
            coordinator_envelope(coordinator_peer_id, envelope::Body::LayoutReject(reject)),
        ),
    }
}

fn join_peer_task(
    result: Result<Result<(), SessionError>, tokio::task::JoinError>,
) -> Result<(), SessionError> {
    result.map_err(|_| SessionError::PeerTask)?
}

async fn serve_direct_pane_streams(
    transport: &Transport,
    connection: &Connection,
    remote_peer_id: Vec<u8>,
    pane: HostPaneChannels,
) -> Result<(), SessionError> {
    let (screen_writer, _) = transport.open_framed_bi(connection).await?;
    let screen_task = tokio::spawn(screen_writer_task(
        screen_writer,
        pane.screen_rx,
        pane.pane_id.clone(),
        pane.host_peer_id.clone(),
    ));
    let (lease_writer, _) = match transport.open_framed_bi(connection).await {
        Ok(streams) => streams,
        Err(error) => {
            screen_task.abort();
            return Err(error.into());
        }
    };
    let lease_task = tokio::spawn(lease_writer_task(
        pane.host_peer_id.clone(),
        pane.lease_rx,
        pane.pane_id.clone(),
        lease_writer,
    ));
    let control_transport = transport.clone();
    let control_connection = connection.clone();
    let control_task = tokio::spawn(async move {
        let (_, control_reader) = control_transport
            .accept_framed_bi_when_ready(&control_connection)
            .await?;
        control_reader_task(
            control_reader,
            remote_peer_id,
            pane.pane_id,
            pane.control_tx,
        )
        .await
    });
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
            close_transport: true,
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

/// Subscribe directly to a pane's registered owner. The caller retains ownership of `transport`
/// so it can keep independent pane connections open to multiple hosts.
pub async fn subscribe_pane(
    transport: Transport,
    session_id: Vec<u8>,
    host_endpoint: EndpointAddr,
    descriptor: PaneDescriptor,
) -> Result<GuestPane, SessionError> {
    if session_id.is_empty()
        || descriptor.pane_id == 0
        || descriptor.host_peer_id != host_endpoint.id.as_bytes()
    {
        return Err(SessionError::InvalidPostWelcome);
    }
    let connection = transport.connect(host_endpoint.clone()).await?;
    let result = async {
        if connection.remote_id() != host_endpoint.id {
            return Err(SessionError::UnauthenticatedPeer);
        }
        let peer_id = transport.endpoint_id().as_bytes().to_vec();
        let (mut subscribe_writer, _subscribe_reader) = transport.open_bi(&connection).await?;
        transport
            .write_frame(
                &mut subscribe_writer,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    sender_peer_id: peer_id.clone(),
                    body: Some(envelope::Body::PaneSubscribe(PaneSubscribe {
                        session_id,
                        peer_id: peer_id.clone(),
                        pane_id: descriptor.pane_id,
                    })),
                },
            )
            .await?;
        let (_screen_writer, screen_reader) = transport.accept_framed_bi(&connection).await?;
        let (_lease_writer, lease_reader) = transport.accept_framed_bi(&connection).await?;
        let (control_writer, _control_reader) = transport.open_framed_bi(&connection).await?;
        let (events_tx, events) = mpsc::channel(128);
        let (take_control_tx, take_control_rx) = mpsc::channel(16);
        let (input_tx, input_rx) = mpsc::channel(256);
        let pane_id = pane_wire_id(descriptor.pane_id);
        let host_peer_id = descriptor.host_peer_id;
        let tasks = vec![
            tokio::spawn(guest_screen_reader_task(
                screen_reader,
                events_tx.clone(),
                pane_id.clone(),
                host_peer_id.clone(),
            )),
            tokio::spawn(guest_lease_reader_task(
                lease_reader,
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
            close_transport: false,
            tasks,
        })
    }
    .await;
    if result.is_err() {
        connection.close(0u8.into(), b"");
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

#[cfg(test)]
mod control_queue_tests {
    use std::{future::Future, pin::Pin};

    use super::*;
    use tokio::sync::oneshot;

    struct FailingWriter;

    impl ControlFrameSink for FailingWriter {
        fn write_control<'a>(
            &'a mut self,
            _: &'a Envelope,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            Box::pin(async { false })
        }
    }

    struct StalledWriter {
        started: Option<oneshot::Sender<()>>,
        release: Option<oneshot::Receiver<()>>,
    }

    impl ControlFrameSink for StalledWriter {
        fn write_control<'a>(
            &'a mut self,
            _: &'a Envelope,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            Box::pin(async move {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                match self.release.take() {
                    Some(release) => release.await.is_ok(),
                    None => true,
                }
            })
        }
    }

    struct RecordingWriter(mpsc::UnboundedSender<Envelope>);

    impl ControlFrameSink for RecordingWriter {
        fn write_control<'a>(
            &'a mut self,
            envelope: &'a Envelope,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            let sent = self.0.send(envelope.clone()).is_ok();
            Box::pin(async move { sent })
        }
    }

    fn commit(revision: u64) -> Envelope {
        coordinator_envelope(
            b"coordinator",
            envelope::Body::LayoutCommit(LayoutCommit {
                revision,
                state: None,
            }),
        )
    }

    #[tokio::test]
    async fn failed_writer_removes_the_peer_and_aborts_its_reader() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (mailbox, initial_rx, state_rx, targeted_rx) = ControlMailbox::new();
        let reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"slow".to_vec(),
            ControlPeer::new(mailbox.clone(), reader.abort_handle(), None),
        );
        let commit = coordinator_envelope(
            b"coordinator",
            envelope::Body::LayoutReject(LayoutReject {
                request_id: 9,
                reason: LayoutRejectReason::Stale as i32,
            }),
        );
        assert!(mailbox.enqueue_initial(commit), "initial frame queues");

        layout_peer_writer_task(
            FailingWriter,
            initial_rx,
            state_rx,
            targeted_rx,
            b"slow".to_vec(),
            peers.clone(),
            None,
        )
        .await;

        assert!(!peers.lock().unwrap().contains_key(b"slow".as_slice()));
        tokio::task::yield_now().await;
        assert!(reader.is_finished());
        assert!(!mailbox.enqueue_targeted(coordinator_envelope(
            b"coordinator",
            envelope::Body::LayoutReject(LayoutReject {
                request_id: 10,
                reason: LayoutRejectReason::Stale as i32,
            }),
        )));
    }

    #[tokio::test]
    async fn stalled_peer_coalesces_commits_while_a_healthy_peer_receives_the_latest() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (slow_mailbox, slow_initial_rx, slow_state_rx, slow_targeted_rx) =
            ControlMailbox::new();
        let observed_slow_state = slow_state_rx.clone();
        let slow_reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"slow".to_vec(),
            ControlPeer::new(slow_mailbox, slow_reader.abort_handle(), None),
        );
        let (healthy_mailbox, healthy_initial_rx, healthy_state_rx, healthy_targeted_rx) =
            ControlMailbox::new();
        let healthy_reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"healthy".to_vec(),
            ControlPeer::new(healthy_mailbox, healthy_reader.abort_handle(), None),
        );
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        assert!(
            peers
                .lock()
                .unwrap()
                .get(b"slow".as_slice())
                .expect("slow peer")
                .mailbox
                .enqueue_initial(commit(1)),
            "slow initial"
        );
        assert!(
            peers
                .lock()
                .unwrap()
                .get(b"healthy".as_slice())
                .expect("healthy peer")
                .mailbox
                .enqueue_initial(commit(1)),
            "healthy initial"
        );
        let slow_task = tokio::spawn(layout_peer_writer_task(
            StalledWriter {
                started: Some(started_tx),
                release: Some(release_rx),
            },
            slow_initial_rx,
            slow_state_rx,
            slow_targeted_rx,
            b"slow".to_vec(),
            peers.clone(),
            None,
        ));
        let (healthy_tx, mut healthy_rx) = mpsc::unbounded_channel();
        let healthy_task = tokio::spawn(layout_peer_writer_task(
            RecordingWriter(healthy_tx),
            healthy_initial_rx,
            healthy_state_rx,
            healthy_targeted_rx,
            b"healthy".to_vec(),
            peers.clone(),
            None,
        ));

        broadcast_envelope(&peers, commit(2));
        started_rx
            .await
            .expect("slow writer begins its first commit");
        broadcast_envelope(&peers, commit(3));
        broadcast_envelope(&peers, commit(4));
        assert!(matches!(
            observed_slow_state
                .borrow()
                .as_ref()
                .map(|state| &state.envelope),
            Some(Envelope {
                body: Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 4,
                    ..
                })),
                ..
            })
        ));
        loop {
            let envelope = healthy_rx.recv().await.expect("healthy delivery");
            if matches!(
                envelope.body,
                Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 4,
                    ..
                }))
            ) {
                break;
            }
        }

        let _ = release_tx.send(());
        remove_control_peer(&peers, b"slow");
        remove_control_peer(&peers, b"healthy");
        slow_task.abort();
        healthy_task.abort();
    }

    #[tokio::test]
    async fn targeted_queue_overflow_closes_the_peer_before_it_can_keep_reading() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (mailbox, _initial_rx, _state_rx, targeted_rx) = ControlMailbox::new();
        let reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"slow".to_vec(),
            ControlPeer::new(mailbox.clone(), reader.abort_handle(), None),
        );
        for request_id in 1..=TARGETED_CONTROL_QUEUE_CAPACITY {
            send_to_peer(
                &peers,
                b"slow",
                coordinator_envelope(
                    b"coordinator",
                    envelope::Body::LayoutReject(LayoutReject {
                        request_id: request_id as u64,
                        reason: LayoutRejectReason::Stale as i32,
                    }),
                ),
            );
        }
        assert!(peers.lock().unwrap().contains_key(b"slow".as_slice()));
        send_to_peer(
            &peers,
            b"slow",
            coordinator_envelope(
                b"coordinator",
                envelope::Body::LayoutReject(LayoutReject {
                    request_id: 99,
                    reason: LayoutRejectReason::Stale as i32,
                }),
            ),
        );

        assert!(!peers.lock().unwrap().contains_key(b"slow".as_slice()));
        tokio::task::yield_now().await;
        assert!(reader.is_finished());
        drop(targeted_rx);
        assert!(!mailbox.enqueue_targeted(coordinator_envelope(
            b"coordinator",
            envelope::Body::LayoutReject(LayoutReject {
                request_id: 100,
                reason: LayoutRejectReason::Stale as i32,
            }),
        )));
    }

    #[tokio::test]
    async fn initial_snapshot_is_first_when_a_commit_arrives_before_the_writer_starts() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (mailbox, initial_rx, state_rx, targeted_rx) = ControlMailbox::new();
        let reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"member".to_vec(),
            ControlPeer::new(mailbox.clone(), reader.abort_handle(), None),
        );
        let snapshot = coordinator_envelope(
            b"coordinator",
            envelope::Body::SessionSnapshot(SessionSnapshot { state: None }),
        );
        assert!(
            mailbox.enqueue_initial(snapshot.clone()),
            "initial snapshot queues"
        );
        mailbox.publish_state(commit(2));
        let (recorded_tx, mut recorded_rx) = mpsc::unbounded_channel();
        let writer_task = tokio::spawn(layout_peer_writer_task(
            RecordingWriter(recorded_tx),
            initial_rx,
            state_rx,
            targeted_rx,
            b"member".to_vec(),
            peers.clone(),
            None,
        ));

        assert_eq!(recorded_rx.recv().await, Some(snapshot));
        assert!(matches!(
            recorded_rx.recv().await,
            Some(Envelope {
                body: Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 2,
                    ..
                })),
                ..
            })
        ));
        remove_control_peer(&peers, b"member");
        writer_task.abort();
    }

    #[tokio::test]
    async fn targeted_frame_precedes_later_coalesced_state_updates() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (mailbox, initial_rx, state_rx, targeted_rx) = ControlMailbox::new();
        let reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"member".to_vec(),
            ControlPeer::new(mailbox.clone(), reader.abort_handle(), None),
        );
        assert!(mailbox.enqueue_initial(commit(1)), "initial frame queues");
        mailbox.publish_state(commit(2));
        let reject = coordinator_envelope(
            b"coordinator",
            envelope::Body::LayoutReject(LayoutReject {
                request_id: 7,
                reason: LayoutRejectReason::Stale as i32,
            }),
        );
        assert!(
            mailbox.enqueue_targeted(reject.clone()),
            "targeted frame queues"
        );
        mailbox.publish_state(commit(3));
        mailbox.publish_state(commit(4));
        let (recorded_tx, mut recorded_rx) = mpsc::unbounded_channel();
        let writer_task = tokio::spawn(layout_peer_writer_task(
            RecordingWriter(recorded_tx),
            initial_rx,
            state_rx,
            targeted_rx,
            b"member".to_vec(),
            peers.clone(),
            None,
        ));

        let _initial = recorded_rx.recv().await.expect("initial output");
        assert_eq!(recorded_rx.recv().await, Some(reject));
        assert!(matches!(
            recorded_rx.recv().await,
            Some(Envelope {
                body: Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 4,
                    ..
                })),
                ..
            })
        ));
        remove_control_peer(&peers, b"member");
        writer_task.abort();
    }
}
