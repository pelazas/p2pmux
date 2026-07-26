//! Authenticated Join/Welcome session handshakes over Iroh bi-streams.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{ConnectingError, Connection, Incoming, SendStream},
};
use tokio::{
    sync::{broadcast, mpsc, watch},
    time::{interval, timeout},
};

use crate::{
    layout::{
        Axis, LayoutError, LayoutSnapshot, Member, NewPanePosition as LayoutNewPanePosition, Node,
        Pane, SessionState, Tab,
    },
    lease::LeaseState,
    protocol::{
        AgentRoster, ControlLease, CreatePane, CreateTab, DeletePane, DeleteTab, Delta, Envelope,
        Input, Join, LayoutCommit, LayoutNode, LayoutReject, LayoutRejectReason, LayoutRequest,
        LayoutSplit, LayoutState, MemberDescriptor, NewPanePosition as ProtocolNewPanePosition,
        PROTOCOL_VERSION, PaneDescriptor, PaneFailed, PaneReady, PaneReservation, PaneSubscribe,
        ReleaseControl, SessionSnapshot, SetSplitRatio, Snapshot, SplitAxis, TabDescriptor,
        TakeControl, UpdatePaneGrids, Welcome, envelope,
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
    agent_rosters: BTreeMap<Vec<u8>, AgentRoster>,
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
        Self::new_with_display_name(
            coordinator_peer_id,
            endpoint_addr,
            String::new(),
            grid_rows,
            grid_cols,
        )
    }

    pub fn new_with_display_name(
        coordinator_peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
        display_name: String,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<Self, CoordinatorError> {
        Self::with_reservation_timeout_and_display_name(
            coordinator_peer_id,
            endpoint_addr,
            display_name,
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
        now: Instant,
    ) -> Result<Self, CoordinatorError> {
        Self::with_reservation_timeout_and_display_name(
            coordinator_peer_id,
            endpoint_addr,
            String::new(),
            grid_rows,
            grid_cols,
            reservation_timeout,
            now,
        )
    }

    pub fn with_reservation_timeout_and_display_name(
        coordinator_peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
        display_name: String,
        grid_rows: u16,
        grid_cols: u16,
        reservation_timeout: Duration,
        _now: Instant,
    ) -> Result<Self, CoordinatorError> {
        let endpoint_addr = serialized_endpoint(&coordinator_peer_id, endpoint_addr)?;
        Ok(Self {
            state: SessionState::new_with_display_name(
                coordinator_peer_id,
                endpoint_addr,
                display_name,
                grid_rows,
                grid_cols,
            )?,
            reservations: BTreeMap::new(),
            agent_rosters: BTreeMap::new(),
            reservation_timeout,
        })
    }

    pub fn session_snapshot(&self) -> Result<SessionSnapshot, CoordinatorError> {
        Ok(SessionSnapshot {
            state: Some(self.protocol_layout_state()?),
        })
    }

    /// Return the cached full-replacement rosters in deterministic host order.
    pub fn agent_rosters(&self) -> Vec<AgentRoster> {
        self.agent_rosters.values().cloned().collect()
    }

    /// Accept a host's latest full roster after checking the authoritative layout.
    ///
    /// The authenticated connection identity always wins over the message's claimed host ID.
    pub fn accept_agent_roster(
        &mut self,
        authenticated_peer_id: &[u8],
        mut roster: AgentRoster,
    ) -> Option<AgentRoster> {
        if !self
            .state
            .members()
            .iter()
            .any(|member| member.peer_id == authenticated_peer_id)
        {
            return None;
        }
        if roster.entries.iter().any(|entry| {
            self.state
                .pane(entry.pane_id)
                .is_none_or(|pane| pane.host_peer_id != authenticated_peer_id)
        }) {
            return None;
        }
        if self
            .agent_rosters
            .get(authenticated_peer_id)
            .is_some_and(|current| roster.generation <= current.generation)
        {
            return None;
        }
        roster.host_peer_id = authenticated_peer_id.to_vec();
        self.agent_rosters
            .insert(authenticated_peer_id.to_vec(), roster.clone());
        Some(roster)
    }

    pub fn admit(
        &mut self,
        peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
    ) -> Result<MembershipChange, CoordinatorError> {
        self.admit_with_display_name(peer_id, endpoint_addr, String::new())
    }

    pub fn admit_with_display_name(
        &mut self,
        peer_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
        display_name: String,
    ) -> Result<MembershipChange, CoordinatorError> {
        let endpoint_addr = serialized_endpoint(&peer_id, endpoint_addr)?;
        let invalidated = self.state.add_member_with_display_name(
            self.state.revision(),
            peer_id,
            endpoint_addr,
            display_name,
        )?;
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
        self.prune_agent_rosters();
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
            + usize::from(request.delete_tab.is_some())
            + usize::from(request.set_split_ratio.is_some())
            + usize::from(request.update_pane_grids.is_some());
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
        } else if let Some(ratio) = request.set_split_ratio {
            self.set_split_ratio(authenticated_peer_id, request.base_revision, ratio)
        } else if let Some(grids) = request.update_pane_grids {
            self.update_pane_grids(authenticated_peer_id, request.base_revision, grids)
        } else {
            Err(LayoutError::InvalidSnapshot)
        };

        match result {
            Ok(response) => {
                if matches!(response, CoordinatorResponse::Commit(_)) {
                    self.prune_agent_rosters();
                }
                response
            }
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
        let position = protocol_pane_position(create.position)?;
        let (grid_rows, grid_cols) = protocol_grid(create.grid_rows, create.grid_cols)?;
        if create.target_pane_id == 0 {
            return Err(LayoutError::InvalidSnapshot);
        }
        let reservation = self.state.reserve_pane_at(
            authenticated_peer_id,
            base_revision,
            create.target_pane_id,
            axis,
            position,
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

    fn set_split_ratio(
        &mut self,
        authenticated_peer_id: &[u8],
        base_revision: u64,
        ratio: SetSplitRatio,
    ) -> Result<CoordinatorResponse, LayoutError> {
        let axis = protocol_axis(ratio.axis)?;
        let first_share_bps =
            u16::try_from(ratio.first_share_bps).map_err(|_| LayoutError::InvalidSplitRatio)?;
        self.state.set_split_ratio(
            authenticated_peer_id,
            base_revision,
            ratio.pane_id,
            axis,
            first_share_bps,
        )?;
        Ok(CoordinatorResponse::Commit(
            self.layout_commit().map_err(layout_error)?,
        ))
    }

    fn update_pane_grids(
        &mut self,
        authenticated_peer_id: &[u8],
        base_revision: u64,
        update: UpdatePaneGrids,
    ) -> Result<CoordinatorResponse, LayoutError> {
        let grids = update
            .panes
            .into_iter()
            .map(|pane| {
                Ok((
                    pane.pane_id,
                    u16::try_from(pane.grid_rows).map_err(|_| LayoutError::InvalidGrid)?,
                    u16::try_from(pane.grid_cols).map_err(|_| LayoutError::InvalidGrid)?,
                ))
            })
            .collect::<Result<Vec<_>, LayoutError>>()?;
        self.state
            .update_pane_grids(authenticated_peer_id, base_revision, &grids)?;
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

    fn prune_agent_rosters(&mut self) {
        self.agent_rosters.retain(|host_peer_id, roster| {
            if !self
                .state
                .members()
                .iter()
                .any(|member| member.peer_id == *host_peer_id)
            {
                return false;
            }
            roster.entries.retain(|entry| {
                self.state
                    .pane(entry.pane_id)
                    .is_some_and(|pane| pane.host_peer_id == *host_peer_id)
            });
            true
        });
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

fn protocol_pane_position(position: Option<i32>) -> Result<LayoutNewPanePosition, LayoutError> {
    match position {
        None => Ok(LayoutNewPanePosition::Second),
        Some(value) if value == ProtocolNewPanePosition::Second as i32 => {
            Ok(LayoutNewPanePosition::Second)
        }
        Some(value) if value == ProtocolNewPanePosition::First as i32 => {
            Ok(LayoutNewPanePosition::First)
        }
        Some(_) => Err(LayoutError::InvalidSnapshot),
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
                display_name: member.display_name,
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

                title: None,
            })
            .collect(),
        tabs: snapshot
            .tabs
            .into_iter()
            .map(|tab| TabDescriptor {
                tab_id: tab.tab_id,
                root: Some(protocol_node(tab.root)),

                title: None,
            })
            .collect(),
    }
}

/// Converts an authoritative wire layout into the I/O-free model consumed by the renderer.
///
/// The conversion deliberately reuses the same validation as coordinator state so a malformed
/// peer message can never become a partially rendered local layout.
pub fn layout_snapshot_from_state(state: &LayoutState) -> Result<LayoutSnapshot, LayoutError> {
    let members = state
        .members
        .iter()
        .map(|member| Member {
            peer_id: member.peer_id.clone(),
            endpoint_addr: member.endpoint_addr.clone(),
            display_name: member.display_name.clone(),
        })
        .collect();
    let panes = state
        .panes
        .iter()
        .map(|pane| {
            let (grid_rows, grid_cols) = protocol_grid(pane.grid_rows, pane.grid_cols)?;
            Ok((
                pane.pane_id,
                Pane {
                    pane_id: pane.pane_id,
                    host_peer_id: pane.host_peer_id.clone(),
                    grid_rows,
                    grid_cols,
                    title: None,
                },
            ))
        })
        .collect::<Result<_, LayoutError>>()?;
    let tabs = state
        .tabs
        .iter()
        .map(|tab| {
            Ok(Tab {
                tab_id: tab.tab_id,
                root: layout_node_from_protocol(
                    tab.root.as_ref().ok_or(LayoutError::InvalidSnapshot)?,
                )?,
                title: None,
            })
        })
        .collect::<Result<_, LayoutError>>()?;
    let snapshot = LayoutSnapshot {
        revision: state.revision,
        members,
        tabs,
        panes,
    };
    SessionState::validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn layout_node_from_protocol(node: &LayoutNode) -> Result<Node, LayoutError> {
    match (&node.leaf_pane_id, &node.split) {
        (Some(pane_id), None) if *pane_id != 0 => Ok(Node::Leaf { pane_id: *pane_id }),
        (None, Some(split)) => Ok(Node::Split {
            axis: protocol_axis(split.axis)?,
            first_share_bps: split
                .first_share_bps
                .map(|share| u16::try_from(share).map_err(|_| LayoutError::InvalidSnapshot))
                .transpose()?
                .unwrap_or(crate::layout::DEFAULT_FIRST_SHARE_BPS),
            first: Box::new(layout_node_from_protocol(
                split.first.as_ref().ok_or(LayoutError::InvalidSnapshot)?,
            )?),
            second: Box::new(layout_node_from_protocol(
                split.second.as_ref().ok_or(LayoutError::InvalidSnapshot)?,
            )?),
        }),
        _ => Err(LayoutError::InvalidSnapshot),
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
            first_share_bps,
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
                first_share_bps: Some(u32::from(first_share_bps)),
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
        LayoutError::UnknownPane { .. }
        | LayoutError::NoMatchingSplit { .. }
        | LayoutError::UnknownTab { .. } => LayoutRejectReason::UnknownId,
        LayoutError::LastPaneInTab { .. } | LayoutError::LastTab => {
            LayoutRejectReason::LastPaneOrTab
        }
        LayoutError::ReservationPending
        | LayoutError::UnknownReservation { .. }
        | LayoutError::ReservationCreatorMismatch
        | LayoutError::ReservationInvalid => LayoutRejectReason::ReservationFailure,
        LayoutError::InvalidPeerId
        | LayoutError::InvalidEndpointAddress
        | LayoutError::InvalidDisplayName
        | LayoutError::InvalidTitle
        | LayoutError::InvalidGrid
        | LayoutError::InvalidSplitRatio
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
    ReleaseControl {
        peer_id: Vec<u8>,
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
    control_tx: mpsc::Sender<GuestControlCommand>,
}

enum GuestControlCommand {
    TakeControl(u64),
    Input(u64, Vec<u8>),
    ReleaseControl,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ControlQueueError {
    Full,
    Closed,
}

impl GuestControlSender {
    pub fn try_take_control(&self, known_lease_epoch: u64) -> Result<(), ControlQueueError> {
        self.control_tx
            .try_send(GuestControlCommand::TakeControl(known_lease_epoch))
            .map_err(queue_error)
    }

    pub fn try_input(&self, lease_epoch: u64, data: Vec<u8>) -> Result<(), ControlQueueError> {
        self.control_tx
            .try_send(GuestControlCommand::Input(lease_epoch, data))
            .map_err(queue_error)
    }

    pub fn try_release_control(&self) -> Result<(), ControlQueueError> {
        self.control_tx
            .try_send(GuestControlCommand::ReleaseControl)
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
    registry: Arc<Mutex<PaneRegistry>>,
    next_subscription_id: Arc<AtomicU64>,
    service_errors: broadcast::Sender<SessionServiceError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionServiceError {
    PaneConnection,
    IncomingDispatcherConnection,
}

#[derive(Default)]
struct PaneRegistry {
    members: BTreeMap<Vec<u8>, EndpointAddr>,
    panes: BTreeMap<u64, HostPaneChannels>,
    subscriptions: BTreeMap<u64, BTreeMap<u64, PaneSubscription>>,
}

struct PaneSubscription {
    connection: Connection,
    peer_id: Vec<u8>,
    active: Arc<AtomicBool>,
}

impl PaneServer {
    pub fn from_host_session(host: &HostSession) -> Self {
        let (service_errors, _) = broadcast::channel(32);
        Self {
            transport: host.transport.clone(),
            session_id: host.ticket.session_id().to_vec(),
            local_peer_id: host.transport.endpoint_id().as_bytes().to_vec(),
            registry: Arc::new(Mutex::new(PaneRegistry::default())),
            next_subscription_id: Arc::new(AtomicU64::new(1)),
            service_errors,
        }
    }

    pub fn new(transport: Transport, session_id: Vec<u8>) -> Result<Self, SessionError> {
        if session_id.is_empty() {
            return Err(SessionError::InvalidPostWelcome);
        }
        let (service_errors, _) = broadcast::channel(32);
        Ok(Self {
            local_peer_id: transport.endpoint_id().as_bytes().to_vec(),
            transport,
            session_id,
            registry: Arc::new(Mutex::new(PaneRegistry::default())),
            next_subscription_id: Arc::new(AtomicU64::new(1)),
            service_errors,
        })
    }

    /// Receives unexpected service failures; malformed and unauthenticated peers are rejected
    /// without creating operational noise.
    pub fn subscribe_errors(&self) -> broadcast::Receiver<SessionServiceError> {
        self.service_errors.subscribe()
    }

    fn report_service_result(
        &self,
        source: SessionServiceError,
        result: &Result<(), SessionError>,
    ) {
        if result
            .as_ref()
            .is_err_and(|error| !expected_service_error(error))
        {
            let _ = self.service_errors.send(source);
        }
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
        self.registry
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .members
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
        let revoked = {
            let mut registry = self.registry.lock().map_err(|_| SessionError::PeerTask)?;
            registry.members = next.clone();
            let mut revoked = Vec::new();
            for subscribers in registry.subscriptions.values_mut() {
                subscribers.retain(|_, subscription| {
                    if next.contains_key(&subscription.peer_id) {
                        true
                    } else {
                        subscription.active.store(false, Ordering::Release);
                        revoked.push(subscription.connection.clone());
                        false
                    }
                });
            }
            registry
                .subscriptions
                .retain(|_, subscribers| !subscribers.is_empty());
            revoked
        };
        for connection in revoked {
            connection.close(0u8.into(), b"member departed");
        }
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
        self.registry
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .panes
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
        let (pane, subscriptions) = {
            let mut registry = self.registry.lock().map_err(|_| SessionError::PeerTask)?;
            (
                registry.panes.remove(&pane_id),
                registry.subscriptions.remove(&pane_id),
            )
        };
        for subscription in subscriptions
            .into_iter()
            .flat_map(|subscribers| subscribers.into_values())
        {
            subscription.active.store(false, Ordering::Release);
            subscription.connection.close(0u8.into(), b"pane removed");
        }
        Ok(pane)
    }

    pub fn remove_local_pane(
        &self,
        pane_id: u64,
    ) -> Result<Option<HostPaneChannels>, SessionError> {
        self.remove_pane(pane_id)
    }

    /// Reports whether this member is currently serving the pane. Runtime lifecycle checks use
    /// this to verify that a committed deletion revokes direct subscriptions before PTY teardown.
    pub fn has_registered_pane(&self, pane_id: u64) -> Result<bool, SessionError> {
        Ok(self
            .registry
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .panes
            .contains_key(&pane_id))
    }

    pub async fn accept_one(&self) -> Result<(), SessionError> {
        let incoming = self.transport.accept_incoming().await?;
        self.serve_incoming(incoming).await
    }

    /// Owns an endpoint accept loop and keeps each pane subscription independent from later
    /// arrivals. A runtime that multiplexes layout joins and panes should use
    /// [`IncomingDispatcher`] instead of starting this loop alongside another acceptor.
    pub async fn accept_loop(&self) -> Result<(), SessionError> {
        self.accept_loop_with_timeout(HANDSHAKE_TIMEOUT).await
    }

    /// Testable accept-loop variant. Idle accept timeouts are retried; closure and other
    /// transport errors end the loop.
    pub async fn accept_loop_with_timeout(
        &self,
        accept_timeout: Duration,
    ) -> Result<(), SessionError> {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = self.transport.accept_incoming_with_timeout(accept_timeout) => {
                    let incoming = match incoming {
                        Ok(incoming) => incoming,
                        Err(TransportError::TimedOut("incoming accept")) => continue,
                        Err(error) => return Err(error.into()),
                    };
                    let server = self.clone();
                    tasks.spawn(async move {
                        let result = server.serve_incoming(incoming).await;
                        server.report_service_result(SessionServiceError::PaneConnection, &result);
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
        {
            return Err(SessionError::UnauthenticatedPeer);
        }
        let subscription_id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        if subscription_id == 0 {
            return Err(SessionError::PeerTask);
        }
        let active = Arc::new(AtomicBool::new(true));
        let pane = {
            let mut registry = self.registry.lock().map_err(|_| SessionError::PeerTask)?;
            if !registry.members.contains_key(&remote_peer_id) {
                return Err(SessionError::UnauthenticatedPeer);
            }
            let pane = registry
                .panes
                .get(&subscribe.pane_id)
                .cloned()
                .ok_or(SessionError::InvalidPostWelcome)?;
            registry
                .subscriptions
                .entry(subscribe.pane_id)
                .or_default()
                .insert(
                    subscription_id,
                    PaneSubscription {
                        connection: connection.clone(),
                        peer_id: remote_peer_id.clone(),
                        active: active.clone(),
                    },
                );
            pane
        };
        if pane.host_peer_id != self.local_peer_id
            || pane.pane_id != pane_wire_id(subscribe.pane_id)
        {
            return Err(SessionError::InvalidPostWelcome);
        }
        let result =
            serve_direct_pane_streams(&self.transport, connection, remote_peer_id, pane, active)
                .await;
        if let Ok(mut registry) = self.registry.lock()
            && let Some(subscribers) = registry.subscriptions.get_mut(&subscribe.pane_id)
        {
            subscribers.remove(&subscription_id);
            if subscribers.is_empty() {
                registry.subscriptions.remove(&subscribe.pane_id);
            }
        }
        if is_normal_peer_disconnect(&result) {
            Ok(())
        } else {
            result
        }
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
    pub display_name: String,
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
            let (control_writer, control_reader) =
                match self.transport.open_framed_bi(&connection).await {
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
                None,
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
            display_name: join.display_name,
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
    pane_server: PaneServer,
    coordinator: Arc<Mutex<LayoutCoordinator>>,
    peers: Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    reservation_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LayoutControlEvent {
    Snapshot(SessionSnapshot),
    AgentRoster(AgentRoster),
    Reservation(PaneReservation),
    Commit(LayoutCommit),
    Reject(LayoutReject),
    Disconnected,
}

/// Applies authoritative control-plane layout state to a member's local pane registry before the
/// runtime attempts direct subscriptions described by that state.
#[derive(Clone)]
pub struct PaneLayoutReconciler {
    panes: PaneServer,
}

impl PaneLayoutReconciler {
    pub fn new(panes: PaneServer) -> Self {
        Self { panes }
    }

    pub fn apply(&self, event: &LayoutControlEvent) -> Result<(), SessionError> {
        let state = match event {
            LayoutControlEvent::Snapshot(snapshot) => snapshot.state.as_ref(),
            LayoutControlEvent::Commit(commit) => commit.state.as_ref(),
            LayoutControlEvent::AgentRoster(_)
            | LayoutControlEvent::Reservation(_)
            | LayoutControlEvent::Reject(_)
            | LayoutControlEvent::Disconnected => None,
        };
        match state {
            Some(state) => self.panes.replace_roster_from_layout(state),
            None => Ok(()),
        }
    }
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
    AgentRoster(AgentRoster),
}

const TARGETED_CONTROL_QUEUE_CAPACITY: usize = 16;

#[derive(Clone)]
struct ControlMailbox {
    initial_tx: mpsc::Sender<Envelope>,
    state_tx: watch::Sender<Option<SequencedEnvelope>>,
    roster_tx: watch::Sender<Option<SequencedEnvelope>>,
    targeted_tx: mpsc::Sender<SequencedEnvelope>,
    next_sequence: Arc<AtomicU64>,
}

struct ControlMailboxReceivers {
    initial_rx: mpsc::Receiver<Envelope>,
    state_rx: watch::Receiver<Option<SequencedEnvelope>>,
    roster_rx: watch::Receiver<Option<SequencedEnvelope>>,
    targeted_rx: mpsc::Receiver<SequencedEnvelope>,
}

#[derive(Clone)]
struct SequencedEnvelope {
    sequence: u64,
    envelope: Envelope,
}

impl ControlMailbox {
    fn new() -> (Self, ControlMailboxReceivers) {
        let (initial_tx, initial_rx) = mpsc::channel(33);
        let (state_tx, state_rx) = watch::channel(None);
        let (roster_tx, roster_rx) = watch::channel(None);
        let (targeted_tx, targeted_rx) = mpsc::channel(TARGETED_CONTROL_QUEUE_CAPACITY);
        (
            Self {
                initial_tx,
                state_tx,
                roster_tx,
                targeted_tx,
                next_sequence: Arc::new(AtomicU64::new(1)),
            },
            ControlMailboxReceivers {
                initial_rx,
                state_rx,
                roster_rx,
                targeted_rx,
            },
        )
    }

    fn enqueue_initial(&self, envelope: Envelope) -> bool {
        self.initial_tx.try_send(envelope).is_ok()
    }

    fn publish_state(&self, envelope: Envelope) {
        self.state_tx.send_replace(Some(self.sequenced(envelope)));
    }

    fn publish_roster(&self, envelope: Envelope) {
        self.roster_tx.send_replace(Some(self.sequenced(envelope)));
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

    /// The endpoint that owns this member's locally hosted panes and outbound subscriptions.
    pub fn transport(&self) -> Transport {
        self.transport.clone()
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

    /// Publish this member's full hosted-agent replacement to the coordinator.
    pub fn try_agent_roster(&self, roster: AgentRoster) -> Result<(), LayoutControlQueueError> {
        self.outbound
            .try_send(LayoutClientMessage::AgentRoster(roster))
            .map_err(layout_queue_error)
    }

    /// Closes only the persistent coordinator-control connection. Direct pane subscriptions use
    /// separate connections and must be revoked by the coordinator's authoritative roster update.
    pub fn disconnect_control(&self) {
        self.connection.close(0u8.into(), b"control disconnected");
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
        Self::with_display_name(host, String::new(), grid_rows, grid_cols)
    }

    pub fn with_display_name(
        host: HostSession,
        display_name: String,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<Self, SessionError> {
        Self::with_reservation_timeout_and_display_name(
            host,
            display_name,
            grid_rows,
            grid_cols,
            DEFAULT_RESERVATION_TIMEOUT,
        )
    }

    pub fn with_reservation_timeout(
        host: HostSession,
        grid_rows: u16,
        grid_cols: u16,
        reservation_timeout: Duration,
    ) -> Result<Self, SessionError> {
        Self::with_reservation_timeout_and_display_name(
            host,
            String::new(),
            grid_rows,
            grid_cols,
            reservation_timeout,
        )
    }

    pub fn with_reservation_timeout_and_display_name(
        host: HostSession,
        display_name: String,
        grid_rows: u16,
        grid_cols: u16,
        reservation_timeout: Duration,
    ) -> Result<Self, SessionError> {
        let pane_server = host.pane_server();
        let coordinator_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let coordinator = LayoutCoordinator::with_reservation_timeout_and_display_name(
            coordinator_peer_id,
            host.ticket().endpoint_addr().clone(),
            display_name,
            grid_rows,
            grid_cols,
            reservation_timeout,
            Instant::now(),
        )?;
        Ok(Self {
            host,
            pane_server,
            coordinator: Arc::new(Mutex::new(coordinator)),
            peers: Arc::new(Mutex::new(BTreeMap::new())),
            reservation_timeout,
        })
    }

    pub fn ticket(&self) -> &JoinTicket {
        self.host.ticket()
    }

    pub fn address_ready(&self) -> bool {
        self.host.address_ready()
    }

    /// Cloned endpoint used for direct subscriptions to panes owned by other members.
    pub fn transport(&self) -> Transport {
        self.host.transport.clone()
    }

    /// Returns the coordinator's current full layout for its own local renderer.
    pub fn session_snapshot(&self) -> Result<SessionSnapshot, SessionError> {
        self.coordinator
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .session_snapshot()
            .map_err(|_| SessionError::InvalidPostWelcome)
    }

    /// Publish the coordinator host's own full agent roster through the same relay path.
    pub fn publish_local_agent_roster(&self, roster: AgentRoster) -> Result<(), SessionError> {
        let peer_id = self.host.transport.endpoint_id().as_bytes().to_vec();
        let accepted = self
            .coordinator
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .accept_agent_roster(&peer_id, roster);
        if let Some(roster) = accepted {
            broadcast_roster(
                &self.peers,
                coordinator_envelope(&peer_id, envelope::Body::AgentRoster(roster)),
            );
        }
        Ok(())
    }

    /// Applies a request made by the coordinator process itself. Remote peers receive the same
    /// commit as they would for a member-originated request; the caller applies the returned
    /// response to its own renderer.
    pub fn handle_local_request(
        &self,
        request: LayoutRequest,
    ) -> Result<CoordinatorResponse, SessionError> {
        let peer_id = self.host.transport.endpoint_id().as_bytes().to_vec();
        let response = self
            .coordinator
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .handle_request(&peer_id, request);
        self.publish_local_response(&response)?;
        Ok(response)
    }

    /// Commits a coordinator-local reservation only after its PTY has been registered.
    pub fn handle_local_ready(
        &self,
        ready: PaneReady,
    ) -> Result<CoordinatorResponse, SessionError> {
        let peer_id = self.host.transport.endpoint_id().as_bytes().to_vec();
        let response = self
            .coordinator
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .handle_pane_ready(&peer_id, ready);
        self.publish_local_response(&response)?;
        Ok(response)
    }

    /// Cancels a failed coordinator-local spawn and makes the corresponding rejection available
    /// to the local runtime without advertising an unready pane.
    pub fn handle_local_failed(&self, failed: PaneFailed) -> Result<LayoutReject, SessionError> {
        let peer_id = self.host.transport.endpoint_id().as_bytes().to_vec();
        let rejection = self
            .coordinator
            .lock()
            .map_err(|_| SessionError::PeerTask)?
            .handle_pane_failed(&peer_id, failed)
            .reject;
        Ok(rejection)
    }

    fn publish_local_response(&self, response: &CoordinatorResponse) -> Result<(), SessionError> {
        if let CoordinatorResponse::Commit(commit) = response {
            let state = commit
                .state
                .as_ref()
                .ok_or(SessionError::InvalidPostWelcome)?;
            self.pane_server.replace_roster_from_layout(state)?;
            self.broadcast_commit(commit.clone());
        }
        Ok(())
    }

    /// Creates the coordinator-owned pane registry. The runtime must keep this registry's roster
    /// and local pane registrations synchronized from authoritative layout commits.
    pub fn pane_server(&self) -> PaneServer {
        self.pane_server.clone()
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
                let membership = coordinator_guard.admit_with_display_name(
                    receipt.admitted_peer_id.clone(),
                    receipt.endpoint_addr.clone(),
                    receipt.display_name.clone(),
                )?;

                let state = membership
                    .commit
                    .state
                    .as_ref()
                    .ok_or(SessionError::InvalidPostWelcome)?;
                self.pane_server.replace_roster_from_layout(state)?;

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
            let (mailbox, receivers) = ControlMailbox::new();
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
            let rosters = self
                .coordinator
                .lock()
                .map_err(|_| SessionError::PeerTask)?
                .agent_rosters();
            for roster in rosters {
                mailbox
                    .enqueue_initial(coordinator_envelope(
                        self.host.ticket().endpoint_addr().id.as_bytes(),
                        envelope::Body::AgentRoster(roster),
                    ))
                    .then_some(())
                    .ok_or(SessionError::PeerTask)?;
            }

            let reader_task = tokio::spawn(layout_host_reader_task(
                reader,
                peer_id,
                self.host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                self.coordinator.clone(),
                self.peers.clone(),
                self.pane_server.clone(),
                self.reservation_timeout,
            ));
            if let Ok(mut slot) = reader_abort.lock() {
                *slot = Some(reader_task.abort_handle());
            }
            tokio::spawn(layout_peer_writer_task(
                writer,
                receivers,
                receipt.admitted_peer_id.clone(),
                self.peers.clone(),
                Some((
                    self.coordinator.clone(),
                    self.host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                    self.pane_server.clone(),
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
                        self.pane_server.clone(),
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
        self.accept_loop_with_timeout(HANDSHAKE_TIMEOUT).await
    }

    pub async fn accept_loop_with_timeout(
        &self,
        accept_timeout: Duration,
    ) -> Result<(), SessionError> {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = self.layout.host.transport.accept_incoming_with_timeout(accept_timeout) => {
                    let incoming = match incoming {
                        Ok(incoming) => incoming,
                        Err(TransportError::TimedOut("incoming accept")) => continue,
                        Err(error) => return Err(error.into()),
                    };
                    let dispatcher = self.clone();
                    tasks.spawn(async move {
                        let result = dispatcher.dispatch_incoming(incoming).await;
                        dispatcher.panes.report_service_result(
                            SessionServiceError::IncomingDispatcherConnection,
                            &result,
                        );
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

fn expected_service_error(error: &SessionError) -> bool {
    matches!(
        error,
        SessionError::TimedOut(_)
            | SessionError::InvalidJoin
            | SessionError::InvalidJoinEndpointAddress
            | SessionError::InvalidWelcome
            | SessionError::UnauthenticatedPeer
            | SessionError::InvalidPostWelcome
            | SessionError::PeerTask
    )
}

fn is_normal_peer_disconnect(result: &Result<(), SessionError>) -> bool {
    matches!(
        result,
        Err(SessionError::Transport(TransportError::StreamRead(_)))
    )
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

fn broadcast_roster(peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>, envelope: Envelope) {
    let peers = match peers.lock() {
        Ok(peers) => peers,
        Err(_) => return,
    };
    for peer in peers.values() {
        // Roster updates supersede only older rosters, never pending layout commits.
        peer.mailbox.publish_roster(envelope.clone());
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

type CoordinatorDeparture = (Arc<Mutex<LayoutCoordinator>>, Vec<u8>, PaneServer);

fn disconnect_or_remove(
    peers: &Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    coordinator: Option<&CoordinatorDeparture>,
    peer_id: &[u8],
) {
    let Some((coordinator, coordinator_peer_id, pane_server)) = coordinator else {
        remove_control_peer(peers, peer_id);
        return;
    };
    let Ok(mut coordinator) = coordinator.lock() else {
        remove_control_peer(peers, peer_id);
        return;
    };
    remove_control_peer(peers, peer_id);
    if let Ok(change) = coordinator.remove_member(peer_id) {
        if let Some(state) = change.commit.state.as_ref() {
            let _ = pane_server.replace_roster_from_layout(state);
        }
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
    join_layout_with_display_name(transport, ticket, String::new()).await
}

pub async fn join_layout_with_display_name(
    transport: Transport,
    ticket: JoinTicket,
    display_name: String,
) -> Result<SharedLayoutMember, SessionError> {
    let connection = transport.connect(ticket.endpoint_addr().clone()).await?;
    let result = async {
        let receipt =
            join_handshake_with_display_name(&transport, &connection, &ticket, display_name)
                .await?;
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
    ControlMailboxReceivers {
        mut initial_rx,
        mut state_rx,
        mut roster_rx,
        mut targeted_rx,
    }: ControlMailboxReceivers,
    peer_id: Vec<u8>,
    peers: Arc<Mutex<BTreeMap<Vec<u8>, ControlPeer>>>,
    coordinator: Option<CoordinatorDeparture>,
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
    while let Ok(initial) = initial_rx.try_recv() {
        if !writer.write_control(&initial).await {
            disconnect_or_remove(&peers, coordinator.as_ref(), &peer_id);
            return;
        }
    }
    let mut targeted_open = true;
    let mut state_open = true;
    let mut roster_open = true;
    let mut pending_targeted = None;
    let mut pending_state = None;
    let mut pending_roster = None;
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
        if pending_roster.is_none() && roster_open && roster_rx.has_changed().unwrap_or(false) {
            pending_roster = roster_rx.borrow_and_update().clone();
        }

        let next = match (
            pending_state.as_ref().map(|item| item.sequence),
            pending_roster.as_ref().map(|item| item.sequence),
            pending_targeted.as_ref().map(|item| item.sequence),
        ) {
            (None, None, None) => {
                tokio::select! {
                    targeted = targeted_rx.recv(), if targeted_open => match targeted {
                        Some(targeted) => pending_targeted = Some(targeted),
                        None => targeted_open = false,
                    },
                    changed = state_rx.changed(), if state_open => match changed {
                        Ok(()) => pending_state = state_rx.borrow_and_update().clone(),
                        Err(_) => state_open = false,
                    },
                    changed = roster_rx.changed(), if roster_open => match changed {
                        Ok(()) => pending_roster = roster_rx.borrow_and_update().clone(),
                        Err(_) => roster_open = false,
                    },
                    else => return,
                }
                continue;
            }
            (state, roster, targeted) => {
                let state = state.unwrap_or(u64::MAX);
                let roster = roster.unwrap_or(u64::MAX);
                let targeted = targeted.unwrap_or(u64::MAX);
                if targeted <= state && targeted <= roster {
                    pending_targeted.take()
                } else if state <= roster {
                    pending_state.take()
                } else {
                    pending_roster.take()
                }
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
            LayoutClientMessage::AgentRoster(roster) => envelope::Body::AgentRoster(roster),
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
            Some(envelope::Body::AgentRoster(roster)) => LayoutControlEvent::AgentRoster(roster),
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
    pane_server: PaneServer,
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
                dispatch_coordinator_response(
                    &peers,
                    &peer_id,
                    &coordinator_peer_id,
                    &pane_server,
                    response,
                );
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
                dispatch_coordinator_response(
                    &peers,
                    &peer_id,
                    &coordinator_peer_id,
                    &pane_server,
                    response,
                );
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
            Some(envelope::Body::AgentRoster(roster)) => {
                let accepted = match coordinator.lock() {
                    Ok(mut coordinator) => coordinator.accept_agent_roster(&peer_id, roster),
                    Err(_) => break,
                };
                if let Some(roster) = accepted {
                    broadcast_roster(
                        &peers,
                        coordinator_envelope(
                            &coordinator_peer_id,
                            envelope::Body::AgentRoster(roster),
                        ),
                    );
                }
            }
            _ => break,
        }
    }
    disconnect_or_remove(
        &peers,
        Some(&(coordinator, coordinator_peer_id, pane_server)),
        &peer_id,
    );
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
    pane_server: &PaneServer,
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
        CoordinatorResponse::Commit(commit) => {
            if let Some(state) = commit.state.as_ref() {
                let _ = pane_server.replace_roster_from_layout(state);
            }
            broadcast_envelope(
                peers,
                coordinator_envelope(coordinator_peer_id, envelope::Body::LayoutCommit(commit)),
            )
        }
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
    active: Arc<AtomicBool>,
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
            Some(active),
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
                            kitty_keyboard_active: frame.kitty_keyboard_active,
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
                kitty_keyboard_active: frame.kitty_keyboard_active,
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
    active: Option<Arc<AtomicBool>>,
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
            Some(envelope::Body::ReleaseControl(release)) if release.pane_id == pane_id => {
                HostControlEvent::ReleaseControl {
                    peer_id: peer_id.clone(),
                }
            }
            _ => return Err(SessionError::InvalidPostWelcome),
        };
        if active
            .as_ref()
            .is_some_and(|active| !active.load(Ordering::Acquire))
        {
            return Err(SessionError::InvalidPostWelcome);
        }
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
        let (control_writer, control_reader) = transport.accept_framed_bi(&connection).await?;
        let (events_tx, events) = mpsc::channel(128);
        let (control_tx, control_rx) = mpsc::channel(256);
        let peer_id = transport.endpoint_id().as_bytes().to_vec();
        let pane_id = DEFAULT_PANE_ID.to_vec();
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
                control_rx,
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
                control_tx,
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
        let (control_tx, control_rx) = mpsc::channel(256);
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
                control_rx,
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
                control_tx,
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
    join_handshake_with_display_name(transport, connection, ticket, String::new()).await
}

async fn join_handshake_with_display_name(
    transport: &Transport,
    connection: &Connection,
    ticket: &JoinTicket,
    display_name: String,
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
                    display_name,
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
        display_name: String::new(),
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
    mut control_rx: mpsc::Receiver<GuestControlCommand>,
    peer_id: Vec<u8>,
    pane_id: Vec<u8>,
) {
    while let Some(command) = control_rx.recv().await {
        let body = match command {
            GuestControlCommand::TakeControl(known_lease_epoch) => {
                envelope::Body::TakeControl(TakeControl {
                    pane_id: pane_id.clone(),
                    requester_peer_id: peer_id.clone(),
                    known_lease_epoch,
                })
            }
            GuestControlCommand::Input(lease_epoch, data) => envelope::Body::Input(Input {
                pane_id: pane_id.clone(),
                lease_epoch,
                data,
            }),
            GuestControlCommand::ReleaseControl => envelope::Body::ReleaseControl(ReleaseControl {
                pane_id: pane_id.clone(),
            }),
        };
        if writer
            .write_next(&Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: peer_id.clone(),
                body: Some(body),
            })
            .await
            .is_err()
        {
            return;
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
    use crate::protocol::{AgentRosterEntry, AgentRosterState};
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

    fn roster(generation: u64) -> Envelope {
        coordinator_envelope(
            b"coordinator",
            envelope::Body::AgentRoster(AgentRoster {
                host_peer_id: b"host".to_vec(),
                generation,
                entries: vec![AgentRosterEntry {
                    pane_id: 1,
                    agent_kind: String::from("codex"),
                    cwd: String::from("/repo"),
                    state: AgentRosterState::Working as i32,
                    working_since_unix_ms: 0,
                }],
            }),
        )
    }

    #[tokio::test]
    async fn failed_writer_removes_the_peer_and_aborts_its_reader() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (mailbox, receivers) = ControlMailbox::new();
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
            receivers,
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
        let (slow_mailbox, slow_receivers) = ControlMailbox::new();
        let observed_slow_state = slow_receivers.state_rx.clone();
        let slow_reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"slow".to_vec(),
            ControlPeer::new(slow_mailbox, slow_reader.abort_handle(), None),
        );
        let (healthy_mailbox, healthy_receivers) = ControlMailbox::new();
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
            slow_receivers,
            b"slow".to_vec(),
            peers.clone(),
            None,
        ));
        let (healthy_tx, mut healthy_rx) = mpsc::unbounded_channel();
        let healthy_task = tokio::spawn(layout_peer_writer_task(
            RecordingWriter(healthy_tx),
            healthy_receivers,
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
        let (mailbox, receivers) = ControlMailbox::new();
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
        drop(receivers.targeted_rx);
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
        let (mailbox, receivers) = ControlMailbox::new();
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
            receivers,
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
        let (mailbox, receivers) = ControlMailbox::new();
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
            receivers,
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

    #[tokio::test]
    async fn roster_watch_does_not_replace_a_pending_layout_commit() {
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let (mailbox, receivers) = ControlMailbox::new();
        let reader = tokio::spawn(std::future::pending::<()>());
        peers.lock().unwrap().insert(
            b"member".to_vec(),
            ControlPeer::new(mailbox.clone(), reader.abort_handle(), None),
        );
        assert!(mailbox.enqueue_initial(commit(1)), "initial frame queues");
        mailbox.publish_state(commit(2));
        mailbox.publish_roster(roster(1));
        mailbox.publish_state(commit(3));

        let (recorded_tx, mut recorded_rx) = mpsc::unbounded_channel();
        let writer_task = tokio::spawn(layout_peer_writer_task(
            RecordingWriter(recorded_tx),
            receivers,
            b"member".to_vec(),
            peers.clone(),
            None,
        ));

        assert!(matches!(
            recorded_rx.recv().await,
            Some(Envelope {
                body: Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 1,
                    ..
                })),
                ..
            })
        ));
        let first = recorded_rx.recv().await.expect("first coalesced update");
        let second = recorded_rx.recv().await.expect("second coalesced update");
        assert!(matches!(
            (first.body, second.body),
            (
                Some(envelope::Body::AgentRoster(_)),
                Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 3,
                    ..
                }))
            ) | (
                Some(envelope::Body::LayoutCommit(LayoutCommit {
                    revision: 3,
                    ..
                })),
                Some(envelope::Body::AgentRoster(_))
            )
        ));
        remove_control_peer(&peers, b"member");
        writer_task.abort();
    }

    #[tokio::test]
    async fn unexpected_service_failures_are_observable_but_rejections_are_suppressed() {
        let server = PaneServer::new(Transport::bind().await.expect("transport"), vec![1])
            .expect("pane server");
        let mut errors = server.subscribe_errors();
        let operational = Err(SessionError::Transport(TransportError::Closed));
        server.report_service_result(SessionServiceError::PaneConnection, &operational);
        assert_eq!(
            errors.recv().await.expect("operational failure"),
            SessionServiceError::PaneConnection
        );
        let rejected = Err(SessionError::UnauthenticatedPeer);
        server.report_service_result(SessionServiceError::PaneConnection, &rejected);
        assert!(
            timeout(Duration::from_millis(10), errors.recv())
                .await
                .is_err()
        );
        server.close().await;
    }
}
