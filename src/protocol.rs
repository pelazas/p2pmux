//! Versioned, length-delimited messages for the future pane transport.

use prost::Message;
use std::fmt;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_ENVELOPE_BYTES: usize = 1_048_560;
pub const MAX_PEER_ID_BYTES: usize = 64;
pub const MAX_SESSION_ID_BYTES: usize = 64;
pub const MAX_PANE_ID_BYTES: usize = 64;
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
    #[prost(oneof = "envelope::Body", tags = "10, 11, 12, 13, 14, 15, 16")]
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
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Join {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub peer_id: Vec<u8>,
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
    }

    Ok(())
}

fn validate_id(field: &'static str, bytes: &[u8], limit: usize) -> Result<(), ProtocolError> {
    if bytes.is_empty() {
        return Err(ProtocolError::EmptyField(field));
    }
    validate_field_size(field, bytes.len(), limit)
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
