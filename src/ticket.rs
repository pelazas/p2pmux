//! Printable, reusable p2pmux session tickets.

use std::{fmt, str::FromStr};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

pub const TICKET_PREFIX: &str = "p2pmux-v1:";
pub const TICKET_VERSION: u8 = 1;
pub const MAX_TICKET_PAYLOAD_BYTES: usize = 16 * 1024;

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
        if endpoint_addr.is_empty() {
            return Err(TicketError::MissingAddresses);
        }
        Ok(Self {
            session_id: *endpoint_addr.id.as_bytes(),
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
        let payload = TicketPayload {
            version: TICKET_VERSION,
            session_id: self.session_id.to_vec(),
            endpoint_addr: self.endpoint_addr.clone(),
        };
        let json = serde_json::to_vec(&payload).expect("ticket payload should serialize");
        write!(formatter, "{TICKET_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }
}

impl FromStr for JoinTicket {
    type Err = TicketError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let encoded = input
            .strip_prefix(TICKET_PREFIX)
            .ok_or(TicketError::MissingPrefix)?;
        if encoded.len() > MAX_TICKET_PAYLOAD_BYTES.div_ceil(3) * 4 {
            return Err(TicketError::PayloadTooLarge);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| TicketError::MalformedBase64)?;
        if bytes.len() > MAX_TICKET_PAYLOAD_BYTES {
            return Err(TicketError::PayloadTooLarge);
        }
        let payload: TicketPayload =
            serde_json::from_slice(&bytes).map_err(|_| TicketError::MalformedPayload)?;
        if payload.version != TICKET_VERSION {
            return Err(TicketError::UnsupportedVersion);
        }
        let session_id: [u8; 32] = payload
            .session_id
            .as_slice()
            .try_into()
            .map_err(|_| TicketError::InvalidSessionId)?;
        if payload.endpoint_addr.is_empty() {
            return Err(TicketError::MissingAddresses);
        }
        if session_id != *payload.endpoint_addr.id.as_bytes() {
            return Err(TicketError::SessionMismatch);
        }
        Ok(Self {
            session_id,
            endpoint_addr: payload.endpoint_addr,
        })
    }
}
