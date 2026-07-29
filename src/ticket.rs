//! Printable, reusable p2pmux session tickets.

use std::{fmt, str::FromStr};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

/// The verbose original encoding: base64 of JSON. Still parsed, never emitted.
pub const TICKET_PREFIX: &str = "p2pmux-v1:";
/// The current encoding: base64 of a postcard-encoded [`EndpointAddr`].
///
/// v1 spent roughly a third of its length restating `session_id` as a JSON array of 32 decimal
/// numbers, even though [`JoinTicket::from_parts`] requires it to equal `endpoint_addr.id`. v2
/// carries the addresses alone and derives the session ID, which takes a 373-character ticket
/// under 130. Should the two ever stop being the same value — decoupling them is the obvious
/// hardening step, since today the join credential is the coordinator's public key — this
/// becomes a genuine field again and needs a v3 prefix rather than a silent format change.
pub const TICKET_PREFIX_V2: &str = "p2pmux-v2:";
pub const TICKET_VERSION: u8 = 1;
pub const MAX_TICKET_PAYLOAD_BYTES: usize = 16 * 1024;

/// Whether `input` is a ticket rather than a short local join code.
pub fn looks_like_ticket(input: &str) -> bool {
    input.starts_with(TICKET_PREFIX) || input.starts_with(TICKET_PREFIX_V2)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinTicket {
    session_id: [u8; 32],
    endpoint_addr: EndpointAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketErrorClass {
    MissingPrefix,
    PayloadTooLarge,
    MalformedBase64,
    MalformedPayload,
    UnsupportedVersion,
    InvalidSessionId,
    SessionMismatch,
    MissingAddresses,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TicketError {
    MissingPrefix,
    PayloadTooLarge,
    MalformedBase64,
    MalformedPayload,
    UnsupportedVersion,
    InvalidSessionId,
    SessionMismatch,
    MissingAddresses,
}

impl TicketError {
    pub fn class(&self) -> TicketErrorClass {
        match self {
            Self::MissingPrefix => TicketErrorClass::MissingPrefix,
            Self::PayloadTooLarge => TicketErrorClass::PayloadTooLarge,
            Self::MalformedBase64 => TicketErrorClass::MalformedBase64,
            Self::MalformedPayload => TicketErrorClass::MalformedPayload,
            Self::UnsupportedVersion => TicketErrorClass::UnsupportedVersion,
            Self::InvalidSessionId => TicketErrorClass::InvalidSessionId,
            Self::SessionMismatch => TicketErrorClass::SessionMismatch,
            Self::MissingAddresses => TicketErrorClass::MissingAddresses,
        }
    }
}

impl fmt::Display for TicketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingPrefix => "invalid ticket format",
            Self::PayloadTooLarge => "ticket payload is too large",
            Self::MalformedBase64 => "ticket payload encoding is invalid",
            Self::MalformedPayload => "ticket payload is invalid",
            Self::UnsupportedVersion => "ticket version is unsupported",
            Self::InvalidSessionId => "ticket session ID is invalid",
            Self::SessionMismatch => "ticket session and endpoint IDs do not match",
            Self::MissingAddresses => "ticket has no transport addresses",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for TicketError {}

#[derive(Serialize, Deserialize)]
struct TicketPayload {
    version: u8,
    session_id: Vec<u8>,
    endpoint_addr: EndpointAddr,
}

impl JoinTicket {
    pub fn mint(endpoint_addr: EndpointAddr) -> Result<Self, TicketError> {
        Self::from_parts(endpoint_addr.id.as_bytes().to_vec(), endpoint_addr)
    }

    pub fn from_parts(
        session_id: Vec<u8>,
        endpoint_addr: EndpointAddr,
    ) -> Result<Self, TicketError> {
        let session_id: [u8; 32] = session_id
            .as_slice()
            .try_into()
            .map_err(|_| TicketError::InvalidSessionId)?;
        if endpoint_addr.is_empty() {
            return Err(TicketError::MissingAddresses);
        }
        if session_id != *endpoint_addr.id.as_bytes() {
            return Err(TicketError::SessionMismatch);
        }
        Ok(Self {
            session_id,
            endpoint_addr,
        })
    }

    pub fn session_id(&self) -> &[u8; 32] {
        &self.session_id
    }

    pub fn endpoint_addr(&self) -> &EndpointAddr {
        &self.endpoint_addr
    }
}

impl fmt::Display for JoinTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes =
            postcard::to_allocvec(&self.endpoint_addr).expect("endpoint address should serialize");
        write!(
            formatter,
            "{TICKET_PREFIX_V2}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        )
    }
}

impl FromStr for JoinTicket {
    type Err = TicketError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if let Some(encoded) = input.strip_prefix(TICKET_PREFIX_V2) {
            let endpoint_addr: EndpointAddr = postcard::from_bytes(&decode_payload(encoded)?)
                .map_err(|_| TicketError::MalformedPayload)?;
            // v2 does not carry the session ID; `from_parts` re-derives the invariant v1 stated.
            return Self::from_parts(endpoint_addr.id.as_bytes().to_vec(), endpoint_addr);
        }
        let encoded = input
            .strip_prefix(TICKET_PREFIX)
            .ok_or(TicketError::MissingPrefix)?;
        let payload: TicketPayload = serde_json::from_slice(&decode_payload(encoded)?)
            .map_err(|_| TicketError::MalformedPayload)?;
        if payload.version != TICKET_VERSION {
            return Err(TicketError::UnsupportedVersion);
        }
        Self::from_parts(payload.session_id, payload.endpoint_addr)
    }
}

/// Decode a ticket body, refusing anything too large to be one before allocating it.
fn decode_payload(encoded: &str) -> Result<Vec<u8>, TicketError> {
    if encoded.len() > MAX_TICKET_PAYLOAD_BYTES.div_ceil(3) * 4 {
        return Err(TicketError::PayloadTooLarge);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TicketError::MalformedBase64)?;
    if bytes.len() > MAX_TICKET_PAYLOAD_BYTES {
        return Err(TicketError::PayloadTooLarge);
    }
    Ok(bytes)
}
