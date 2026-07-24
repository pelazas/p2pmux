//! Versioned, length-delimited messages for the future pane transport.

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_ENVELOPE_BYTES: usize = 1_048_560;
pub const MAX_PEER_ID_BYTES: usize = 64;
pub const MAX_SESSION_ID_BYTES: usize = 64;
pub const MAX_PANE_ID_BYTES: usize = 64;
pub const MAX_INPUT_BYTES: usize = 8 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
pub const MAX_DELTA_BYTES: usize = 64 * 1024;

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
