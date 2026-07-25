//! Versioned, length-delimited messages for the future pane transport.

use prost::Message;
use std::{collections::HashSet, fmt};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_ENVELOPE_BYTES: usize = 1_048_560;
pub const MAX_PEER_ID_BYTES: usize = 64;
pub const MAX_SESSION_ID_BYTES: usize = 64;
pub const MAX_PANE_ID_BYTES: usize = 64;
pub const MAX_ENDPOINT_ADDR_BYTES: usize = 4096;
pub const MAX_INPUT_BYTES: usize = 8 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
pub const MAX_DELTA_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum ProtocolError {
    FrameTooLarge {
        limit: usize,
        actual: usize,
    },
    MalformedLengthPrefix,
    TruncatedFrame {
        declared: usize,
        available: usize,
    },
    TrailingFrameBytes {
        declared: usize,
        actual: usize,
    },
    Encode(prost::EncodeError),
    Decode(prost::DecodeError),
    UnsupportedVersion(u32),
    MissingBody,
    EmptyField(&'static str),
    FieldTooLarge {
        field: &'static str,
        limit: usize,
        actual: usize,
    },
    InvalidLeaseEpoch(&'static str),
    InvalidScreenSequence(&'static str),
    InvalidLayout(&'static str),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { limit, actual } => {
                write!(formatter, "frame size {actual} exceeds limit {limit}")
            }
            Self::MalformedLengthPrefix => formatter.write_str("malformed frame length prefix"),
            Self::TruncatedFrame {
                declared,
                available,
            } => write!(
                formatter,
                "truncated frame declares {declared} payload bytes but has {available}"
            ),
            Self::TrailingFrameBytes { declared, actual } => write!(
                formatter,
                "frame declares {declared} payload bytes but has {actual}"
            ),
            Self::Encode(error) => write!(formatter, "failed to encode protocol frame: {error}"),
            Self::Decode(error) => write!(formatter, "failed to decode protocol frame: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported protocol version {version}")
            }
            Self::MissingBody => formatter.write_str("protocol envelope is missing a body"),
            Self::EmptyField(field) => write!(formatter, "protocol field {field} is empty"),
            Self::FieldTooLarge {
                field,
                limit,
                actual,
            } => write!(
                formatter,
                "protocol field {field} size {actual} exceeds limit {limit}"
            ),
            Self::InvalidLeaseEpoch(field) => {
                write!(formatter, "protocol lease epoch {field} must be nonzero")
            }
            Self::InvalidScreenSequence(field) => {
                write!(formatter, "protocol screen sequence {field} is invalid")
            }
            Self::InvalidLayout(field) => {
                write!(formatter, "protocol layout field {field} is invalid")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Envelope {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub sender_peer_id: Vec<u8>,
    #[prost(
        oneof = "envelope::Body",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24"
    )]
    pub body: Option<envelope::Body>,
}

pub mod envelope {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Body {
        #[prost(message, tag = "10")]
        Join(super::Join),
        #[prost(message, tag = "11")]
        Welcome(super::Welcome),
        #[prost(message, tag = "12")]
        Input(super::Input),
        #[prost(message, tag = "13")]
        TakeControl(super::TakeControl),
        #[prost(message, tag = "14")]
        ControlLease(super::ControlLease),
        #[prost(message, tag = "15")]
        Snapshot(super::Snapshot),
        #[prost(message, tag = "16")]
        Delta(super::Delta),
        #[prost(message, tag = "17")]
        SessionSnapshot(super::SessionSnapshot),
        #[prost(message, tag = "18")]
        LayoutRequest(super::LayoutRequest),
        #[prost(message, tag = "19")]
        PaneReservation(super::PaneReservation),
        #[prost(message, tag = "20")]
        PaneReady(super::PaneReady),
        #[prost(message, tag = "21")]
        LayoutCommit(super::LayoutCommit),
        #[prost(message, tag = "22")]
        LayoutReject(super::LayoutReject),
        #[prost(message, tag = "23")]
        PaneSubscribe(super::PaneSubscribe),
        #[prost(message, tag = "24")]
        PaneFailed(super::PaneFailed),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Join {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub peer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub endpoint_addr: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Welcome {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub admitted_peer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub coordinator_peer_id: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Input {
    #[prost(bytes = "vec", tag = "1")]
    pub pane_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub lease_epoch: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TakeControl {
    #[prost(bytes = "vec", tag = "1")]
    pub pane_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub requester_peer_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub known_lease_epoch: u64,
    #[prost(bool, tag = "4")]
    pub force: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlLease {
    #[prost(bytes = "vec", tag = "1")]
    pub pane_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub controller_peer_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub lease_epoch: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Snapshot {
    #[prost(bytes = "vec", tag = "1")]
    pub pane_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub host_peer_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub sequence: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub screen: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Delta {
    #[prost(bytes = "vec", tag = "1")]
    pub pane_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub host_peer_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub base_sequence: u64,
    #[prost(uint64, tag = "4")]
    pub sequence: u64,
    #[prost(bytes = "vec", tag = "5")]
    pub changes: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SessionSnapshot {
    #[prost(message, optional, tag = "1")]
    pub state: Option<LayoutState>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LayoutState {
    #[prost(uint64, tag = "1")]
    pub revision: u64,
    #[prost(message, repeated, tag = "2")]
    pub members: Vec<MemberDescriptor>,
    #[prost(message, repeated, tag = "3")]
    pub panes: Vec<PaneDescriptor>,
    #[prost(message, repeated, tag = "4")]
    pub tabs: Vec<TabDescriptor>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct MemberDescriptor {
    #[prost(bytes = "vec", tag = "1")]
    pub peer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub endpoint_addr: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaneDescriptor {
    #[prost(uint64, tag = "1")]
    pub pane_id: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub host_peer_id: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub grid_rows: u32,
    #[prost(uint32, tag = "4")]
    pub grid_cols: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TabDescriptor {
    #[prost(uint64, tag = "1")]
    pub tab_id: u64,
    #[prost(message, optional, tag = "2")]
    pub root: Option<LayoutNode>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LayoutNode {
    #[prost(uint64, optional, tag = "1")]
    pub leaf_pane_id: Option<u64>,
    #[prost(message, optional, tag = "2")]
    pub split: Option<Box<LayoutSplit>>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LayoutSplit {
    #[prost(enumeration = "SplitAxis", optional, tag = "1")]
    pub axis: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub first: Option<LayoutNode>,
    #[prost(message, optional, tag = "3")]
    pub second: Option<LayoutNode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
pub enum SplitAxis {
    LeftRight = 0,
    TopBottom = 1,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LayoutRequest {
    #[prost(uint64, tag = "1")]
    pub request_id: u64,
    #[prost(uint64, tag = "2")]
    pub base_revision: u64,
    #[prost(message, optional, tag = "3")]
    pub create_pane: Option<CreatePane>,
    #[prost(message, optional, tag = "4")]
    pub delete_pane: Option<DeletePane>,
    #[prost(message, optional, tag = "5")]
    pub create_tab: Option<CreateTab>,
    #[prost(message, optional, tag = "6")]
    pub delete_tab: Option<DeleteTab>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CreatePane {
    #[prost(uint64, tag = "1")]
    pub target_pane_id: u64,
    #[prost(enumeration = "SplitAxis", optional, tag = "2")]
    pub axis: Option<i32>,
    #[prost(uint32, tag = "3")]
    pub grid_rows: u32,
    #[prost(uint32, tag = "4")]
    pub grid_cols: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DeletePane {
    #[prost(uint64, tag = "1")]
    pub pane_id: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CreateTab {
    #[prost(uint32, tag = "1")]
    pub grid_rows: u32,
    #[prost(uint32, tag = "2")]
    pub grid_cols: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DeleteTab {
    #[prost(uint64, tag = "1")]
    pub tab_id: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaneReservation {
    #[prost(uint64, tag = "1")]
    pub reservation_id: u64,
    #[prost(uint64, tag = "2")]
    pub pane_id: u64,
    #[prost(uint64, optional, tag = "3")]
    pub tab_id: Option<u64>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaneReady {
    #[prost(uint64, tag = "1")]
    pub reservation_id: u64,
    #[prost(uint64, tag = "2")]
    pub base_revision: u64,
    #[prost(uint64, tag = "3")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaneFailed {
    #[prost(uint64, tag = "1")]
    pub reservation_id: u64,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(uint64, tag = "3")]
    pub base_revision: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LayoutCommit {
    #[prost(uint64, tag = "1")]
    pub revision: u64,
    #[prost(message, optional, tag = "2")]
    pub state: Option<LayoutState>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LayoutReject {
    #[prost(uint64, tag = "1")]
    pub request_id: u64,
    #[prost(enumeration = "LayoutRejectReason", tag = "2")]
    pub reason: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
pub enum LayoutRejectReason {
    Stale = 1,
    NotHost = 2,
    Limit = 3,
    Malformed = 4,
    MixedTab = 5,
    UnknownId = 6,
    LastPaneOrTab = 7,
    ReservationFailure = 8,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaneSubscribe {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub peer_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub pane_id: u64,
}

pub fn encode_frame(envelope: &Envelope) -> Result<Vec<u8>, ProtocolError> {
    validate_envelope(envelope)?;

    let payload_len = envelope.encoded_len();
    if payload_len > MAX_ENVELOPE_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            limit: MAX_ENVELOPE_BYTES,
            actual: payload_len,
        });
    }

    let frame_len = encoded_varint_len(payload_len as u64)
        .checked_add(payload_len)
        .ok_or(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: payload_len,
        })?;
    if frame_len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: frame_len,
        });
    }

    let mut frame = Vec::with_capacity(frame_len);
    envelope
        .encode_length_delimited(&mut frame)
        .map_err(ProtocolError::Encode)?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: frame.len(),
        });
    }

    Ok(frame)
}

pub fn decode_frame(frame: &[u8]) -> Result<Envelope, ProtocolError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: frame.len(),
        });
    }

    let (declared, prefix_len) = decode_length_prefix(frame)?;
    if declared > MAX_ENVELOPE_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            limit: MAX_ENVELOPE_BYTES,
            actual: declared,
        });
    }

    let frame_len = prefix_len
        .checked_add(declared)
        .ok_or(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: declared,
        })?;
    let available = frame.len().saturating_sub(prefix_len);
    if frame.len() < frame_len {
        return Err(ProtocolError::TruncatedFrame {
            declared,
            available,
        });
    }
    if frame.len() > frame_len {
        return Err(ProtocolError::TrailingFrameBytes {
            declared,
            actual: available,
        });
    }

    let envelope =
        Envelope::decode(&frame[prefix_len..frame_len]).map_err(ProtocolError::Decode)?;
    validate_envelope(&envelope)?;
    Ok(envelope)
}

fn decode_length_prefix(frame: &[u8]) -> Result<(usize, usize), ProtocolError> {
    let mut value = 0_u64;

    for index in 0..10 {
        let byte = *frame
            .get(index)
            .ok_or(ProtocolError::MalformedLengthPrefix)?;
        let bits = u64::from(byte & 0x7f);
        if index == 9 && bits > 1 {
            return Err(ProtocolError::MalformedLengthPrefix);
        }
        value |= bits << (index * 7);
        if byte & 0x80 == 0 {
            let declared =
                usize::try_from(value).map_err(|_| ProtocolError::MalformedLengthPrefix)?;
            return Ok((declared, index + 1));
        }
    }

    Err(ProtocolError::MalformedLengthPrefix)
}

fn encoded_varint_len(mut value: u64) -> usize {
    let mut length = 1;
    while value >= 0x80 {
        value >>= 7;
        length += 1;
    }
    length
}

fn validate_envelope(envelope: &Envelope) -> Result<(), ProtocolError> {
    if envelope.version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(envelope.version));
    }
    validate_id(
        "envelope.sender_peer_id",
        &envelope.sender_peer_id,
        MAX_PEER_ID_BYTES,
    )?;

    let body = envelope.body.as_ref().ok_or(ProtocolError::MissingBody)?;
    match body {
        envelope::Body::Join(join) => {
            validate_id("join.session_id", &join.session_id, MAX_SESSION_ID_BYTES)?;
            validate_id("join.peer_id", &join.peer_id, MAX_PEER_ID_BYTES)?;
            validate_id(
                "join.endpoint_addr",
                &join.endpoint_addr,
                MAX_ENDPOINT_ADDR_BYTES,
            )?;
        }
        envelope::Body::Welcome(welcome) => {
            validate_id(
                "welcome.session_id",
                &welcome.session_id,
                MAX_SESSION_ID_BYTES,
            )?;
            validate_id(
                "welcome.admitted_peer_id",
                &welcome.admitted_peer_id,
                MAX_PEER_ID_BYTES,
            )?;
            validate_id(
                "welcome.coordinator_peer_id",
                &welcome.coordinator_peer_id,
                MAX_PEER_ID_BYTES,
            )?;
        }
        envelope::Body::Input(input) => {
            validate_id("input.pane_id", &input.pane_id, MAX_PANE_ID_BYTES)?;
            if input.lease_epoch == 0 {
                return Err(ProtocolError::InvalidLeaseEpoch("input.lease_epoch"));
            }
            validate_field_size("input.data", input.data.len(), MAX_INPUT_BYTES)?;
        }
        envelope::Body::TakeControl(take_control) => {
            validate_id(
                "take_control.pane_id",
                &take_control.pane_id,
                MAX_PANE_ID_BYTES,
            )?;
            validate_id(
                "take_control.requester_peer_id",
                &take_control.requester_peer_id,
                MAX_PEER_ID_BYTES,
            )?;
        }
        envelope::Body::ControlLease(control_lease) => {
            validate_id(
                "control_lease.pane_id",
                &control_lease.pane_id,
                MAX_PANE_ID_BYTES,
            )?;
            validate_id(
                "control_lease.controller_peer_id",
                &control_lease.controller_peer_id,
                MAX_PEER_ID_BYTES,
            )?;
            if control_lease.lease_epoch == 0 {
                return Err(ProtocolError::InvalidLeaseEpoch(
                    "control_lease.lease_epoch",
                ));
            }
        }
        envelope::Body::Snapshot(snapshot) => {
            validate_id("snapshot.pane_id", &snapshot.pane_id, MAX_PANE_ID_BYTES)?;
            validate_id(
                "snapshot.host_peer_id",
                &snapshot.host_peer_id,
                MAX_PEER_ID_BYTES,
            )?;
            if snapshot.sequence == 0 {
                return Err(ProtocolError::InvalidScreenSequence("snapshot.sequence"));
            }
            validate_field_size("snapshot.screen", snapshot.screen.len(), MAX_SNAPSHOT_BYTES)?;
        }
        envelope::Body::Delta(delta) => {
            validate_id("delta.pane_id", &delta.pane_id, MAX_PANE_ID_BYTES)?;
            validate_id("delta.host_peer_id", &delta.host_peer_id, MAX_PEER_ID_BYTES)?;
            if delta.base_sequence == 0 {
                return Err(ProtocolError::InvalidScreenSequence("delta.base_sequence"));
            }
            if delta.sequence <= delta.base_sequence {
                return Err(ProtocolError::InvalidScreenSequence("delta.sequence"));
            }
            validate_field_size("delta.changes", delta.changes.len(), MAX_DELTA_BYTES)?;
        }
        envelope::Body::SessionSnapshot(snapshot) => {
            validate_layout_state(
                snapshot
                    .state
                    .as_ref()
                    .ok_or(ProtocolError::InvalidLayout("session_snapshot.state"))?,
            )?;
        }
        envelope::Body::LayoutRequest(request) => validate_layout_request(request)?,
        envelope::Body::PaneReservation(reservation) => {
            validate_nonzero(
                "pane_reservation.reservation_id",
                reservation.reservation_id,
            )?;
            validate_nonzero("pane_reservation.pane_id", reservation.pane_id)?;
            if let Some(tab_id) = reservation.tab_id {
                validate_nonzero("pane_reservation.tab_id", tab_id)?;
            }
        }
        envelope::Body::PaneReady(ready) => {
            validate_nonzero("pane_ready.reservation_id", ready.reservation_id)?;
            validate_nonzero("pane_ready.base_revision", ready.base_revision)?;
            validate_nonzero("pane_ready.request_id", ready.request_id)?;
        }
        envelope::Body::PaneFailed(failed) => {
            validate_nonzero("pane_failed.reservation_id", failed.reservation_id)?;
            validate_nonzero("pane_failed.request_id", failed.request_id)?;
            validate_nonzero("pane_failed.base_revision", failed.base_revision)?;
        }
        envelope::Body::LayoutCommit(commit) => {
            validate_nonzero("layout_commit.revision", commit.revision)?;
            let state = commit
                .state
                .as_ref()
                .ok_or(ProtocolError::InvalidLayout("layout_commit.state"))?;
            validate_layout_state(state)?;
            if state.revision != commit.revision {
                return Err(ProtocolError::InvalidLayout("layout_commit.revision"));
            }
        }
        envelope::Body::LayoutReject(reject) => {
            validate_nonzero("layout_reject.request_id", reject.request_id)?;
            LayoutRejectReason::try_from(reject.reason)
                .map_err(|_| ProtocolError::InvalidLayout("layout_reject.reason"))?;
        }
        envelope::Body::PaneSubscribe(subscribe) => {
            validate_id(
                "pane_subscribe.session_id",
                &subscribe.session_id,
                MAX_SESSION_ID_BYTES,
            )?;
            validate_id(
                "pane_subscribe.peer_id",
                &subscribe.peer_id,
                MAX_PEER_ID_BYTES,
            )?;
            validate_nonzero("pane_subscribe.pane_id", subscribe.pane_id)?;
        }
    }

    Ok(())
}

fn validate_id(field: &'static str, bytes: &[u8], limit: usize) -> Result<(), ProtocolError> {
    if bytes.is_empty() {
        return Err(ProtocolError::EmptyField(field));
    }
    validate_field_size(field, bytes.len(), limit)
}

fn validate_nonzero(field: &'static str, value: u64) -> Result<(), ProtocolError> {
    if value == 0 {
        return Err(ProtocolError::InvalidLayout(field));
    }
    Ok(())
}

fn validate_grid(field: &'static str, rows: u32, cols: u32) -> Result<(), ProtocolError> {
    if rows == 0 || cols == 0 || rows > u32::from(u16::MAX) || cols > u32::from(u16::MAX) {
        return Err(ProtocolError::InvalidLayout(field));
    }
    Ok(())
}

fn validate_axis(field: &'static str, axis: Option<i32>) -> Result<(), ProtocolError> {
    let axis = axis.ok_or(ProtocolError::InvalidLayout(field))?;
    SplitAxis::try_from(axis).map_err(|_| ProtocolError::InvalidLayout(field))?;
    Ok(())
}

fn validate_layout_request(request: &LayoutRequest) -> Result<(), ProtocolError> {
    validate_nonzero("layout_request.request_id", request.request_id)?;
    validate_nonzero("layout_request.base_revision", request.base_revision)?;

    let actions = usize::from(request.create_pane.is_some())
        + usize::from(request.delete_pane.is_some())
        + usize::from(request.create_tab.is_some())
        + usize::from(request.delete_tab.is_some());
    if actions != 1 {
        return Err(ProtocolError::InvalidLayout("layout_request.action"));
    }

    if let Some(create_pane) = &request.create_pane {
        validate_nonzero(
            "layout_request.create_pane.target_pane_id",
            create_pane.target_pane_id,
        )?;
        validate_axis("layout_request.create_pane.axis", create_pane.axis)?;
        validate_grid(
            "layout_request.create_pane.grid",
            create_pane.grid_rows,
            create_pane.grid_cols,
        )?;
    }
    if let Some(delete_pane) = &request.delete_pane {
        validate_nonzero("layout_request.delete_pane.pane_id", delete_pane.pane_id)?;
    }
    if let Some(create_tab) = &request.create_tab {
        validate_grid(
            "layout_request.create_tab.grid",
            create_tab.grid_rows,
            create_tab.grid_cols,
        )?;
    }
    if let Some(delete_tab) = &request.delete_tab {
        validate_nonzero("layout_request.delete_tab.tab_id", delete_tab.tab_id)?;
    }
    Ok(())
}

fn validate_layout_state(state: &LayoutState) -> Result<(), ProtocolError> {
    validate_nonzero("layout_state.revision", state.revision)?;
    if !(1..=8).contains(&state.members.len()) {
        return Err(ProtocolError::InvalidLayout("layout_state.members"));
    }
    if !(1..=72).contains(&state.panes.len()) {
        return Err(ProtocolError::InvalidLayout("layout_state.panes"));
    }
    if !(1..=9).contains(&state.tabs.len()) {
        return Err(ProtocolError::InvalidLayout("layout_state.tabs"));
    }

    let mut member_ids = HashSet::with_capacity(state.members.len());
    for member in &state.members {
        validate_id(
            "layout_state.member.peer_id",
            &member.peer_id,
            MAX_PEER_ID_BYTES,
        )?;
        validate_id(
            "layout_state.member.endpoint_addr",
            &member.endpoint_addr,
            MAX_ENDPOINT_ADDR_BYTES,
        )?;
        if !member_ids.insert(member.peer_id.as_slice()) {
            return Err(ProtocolError::InvalidLayout("layout_state.member.peer_id"));
        }
    }

    let mut pane_ids = HashSet::with_capacity(state.panes.len());
    for pane in &state.panes {
        validate_nonzero("layout_state.pane.pane_id", pane.pane_id)?;
        validate_id(
            "layout_state.pane.host_peer_id",
            &pane.host_peer_id,
            MAX_PEER_ID_BYTES,
        )?;
        validate_grid("layout_state.pane.grid", pane.grid_rows, pane.grid_cols)?;
        if !pane_ids.insert(pane.pane_id) {
            return Err(ProtocolError::InvalidLayout("layout_state.pane.pane_id"));
        }
        if !member_ids.contains(pane.host_peer_id.as_slice()) {
            return Err(ProtocolError::InvalidLayout(
                "layout_state.pane.host_peer_id",
            ));
        }
    }

    let mut tab_ids = HashSet::with_capacity(state.tabs.len());
    let mut leaf_pane_ids = HashSet::with_capacity(state.panes.len());
    for tab in &state.tabs {
        validate_nonzero("layout_state.tab.tab_id", tab.tab_id)?;
        if !tab_ids.insert(tab.tab_id) {
            return Err(ProtocolError::InvalidLayout("layout_state.tab.tab_id"));
        }
        let root = tab
            .root
            .as_ref()
            .ok_or(ProtocolError::InvalidLayout("layout_state.tab.root"))?;
        let mut leaves = 0;
        validate_layout_node(root, 0, &mut leaves, &mut leaf_pane_ids)?;
    }
    if leaf_pane_ids != pane_ids {
        return Err(ProtocolError::InvalidLayout("layout_state.panes"));
    }
    Ok(())
}

fn validate_layout_node(
    node: &LayoutNode,
    depth: usize,
    leaves: &mut usize,
    leaf_pane_ids: &mut HashSet<u64>,
) -> Result<(), ProtocolError> {
    if depth > 4 {
        return Err(ProtocolError::InvalidLayout("layout_state.node.depth"));
    }
    match (node.leaf_pane_id, node.split.as_ref()) {
        (Some(pane_id), None) => {
            validate_nonzero("layout_state.node.leaf_pane_id", pane_id)?;
            if !leaf_pane_ids.insert(pane_id) {
                return Err(ProtocolError::InvalidLayout(
                    "layout_state.node.leaf_pane_id",
                ));
            }
            *leaves += 1;
            if *leaves > 8 {
                return Err(ProtocolError::InvalidLayout("layout_state.node.leaves"));
            }
        }
        (None, Some(split)) => {
            validate_axis("layout_state.node.split.axis", split.axis)?;
            let first = split.first.as_ref().ok_or(ProtocolError::InvalidLayout(
                "layout_state.node.split.first",
            ))?;
            let second = split.second.as_ref().ok_or(ProtocolError::InvalidLayout(
                "layout_state.node.split.second",
            ))?;
            validate_layout_node(first, depth + 1, leaves, leaf_pane_ids)?;
            validate_layout_node(second, depth + 1, leaves, leaf_pane_ids)?;
        }
        _ => return Err(ProtocolError::InvalidLayout("layout_state.node.kind")),
    }
    Ok(())
}

fn validate_field_size(
    field: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), ProtocolError> {
    if actual > limit {
        return Err(ProtocolError::FieldTooLarge {
            field,
            limit,
            actual,
        });
    }
    Ok(())
}
