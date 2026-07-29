//! Printable, reusable p2pmux session tickets.

use std::{fmt, str::FromStr};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

/// The verbose original encoding: base64 of JSON. Still parsed, never emitted.
pub const TICKET_PREFIX: &str = "p2pmux-v1:";
/// The second encoding: base64 of a postcard-encoded [`EndpointAddr`] alone. Still parsed,
/// never emitted; see [`TICKET_PREFIX_V3`] for why it had to be replaced.
pub const TICKET_PREFIX_V2: &str = "p2pmux-v2:";
/// The current encoding: base64 of a postcard-encoded [`TicketBody`].
///
/// v1 and v2 both defined `session_id` to *be* the coordinator's endpoint public key —
/// v1 restated it, v2 saved space by deriving it. That made the join credential a public
/// value: an endpoint id is presented in the TLS handshake and published to discovery, so
/// anyone who learned it could mint a working ticket and walk into the session. There was
/// no secret to hold.
///
/// v3 carries 32 independent random bytes instead. Knowing who the coordinator is no
/// longer implies permission to join, which is what makes refusing a peer meaningful —
/// see `SessionLock`. It also gives revocation and per-pane permissions something to name
/// later, additively.
pub const TICKET_PREFIX_V3: &str = "p2pmux-v3:";
pub const TICKET_VERSION: u8 = 1;
pub const MAX_TICKET_PAYLOAD_BYTES: usize = 16 * 1024;

/// Whether `input` is a ticket rather than a short local join code.
pub fn looks_like_ticket(input: &str) -> bool {
    input.starts_with(TICKET_PREFIX)
        || input.starts_with(TICKET_PREFIX_V2)
        || input.starts_with(TICKET_PREFIX_V3)
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
    MissingAddresses,
    Random,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TicketError {
    MissingPrefix,
    PayloadTooLarge,
    MalformedBase64,
    MalformedPayload,
    UnsupportedVersion,
    InvalidSessionId,
    MissingAddresses,
    Random,
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
            Self::MissingAddresses => TicketErrorClass::MissingAddresses,
            Self::Random => TicketErrorClass::Random,
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
            Self::MissingAddresses => "ticket has no transport addresses",
            Self::Random => "could not generate a session secret",
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

/// The v3 wire body: a secret the holder must present, plus where to present it.
#[derive(Serialize, Deserialize)]
struct TicketBody {
    session_id: [u8; 32],
    endpoint_addr: EndpointAddr,
}

impl JoinTicket {
    /// Mint a ticket for a session, with a freshly generated session secret.
    pub fn mint(endpoint_addr: EndpointAddr) -> Result<Self, TicketError> {
        let mut session_id = [0_u8; 32];
        getrandom::fill(&mut session_id).map_err(|_| TicketError::Random)?;
        Self::from_parts(session_id.to_vec(), endpoint_addr)
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
        Ok(Self {
            session_id,
            endpoint_addr,
        })
    }

    /// True when this ticket's secret is merely the coordinator's public key.
    ///
    /// That is what a legacy v1/v2 ticket decodes to, and it means the ticket grants
    /// nothing a passive observer could not derive for itself.
    pub fn secret_is_public(&self) -> bool {
        self.session_id == *self.endpoint_addr.id.as_bytes()
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
        let body = TicketBody {
            session_id: self.session_id,
            endpoint_addr: self.endpoint_addr.clone(),
        };
        let bytes = postcard::to_allocvec(&body).expect("ticket body should serialize");
        write!(
            formatter,
            "{TICKET_PREFIX_V3}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        )
    }
}

impl FromStr for JoinTicket {
    type Err = TicketError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if let Some(encoded) = input.strip_prefix(TICKET_PREFIX_V3) {
            let body: TicketBody = postcard::from_bytes(&decode_payload(encoded)?)
                .map_err(|_| TicketError::MalformedPayload)?;
            return Self::from_parts(body.session_id.to_vec(), body.endpoint_addr);
        }
        if let Some(encoded) = input.strip_prefix(TICKET_PREFIX_V2) {
            let endpoint_addr: EndpointAddr = postcard::from_bytes(&decode_payload(encoded)?)
                .map_err(|_| TicketError::MalformedPayload)?;
            // v2 carried no session ID because it *was* the endpoint id. Such a ticket still
            // parses, but it can only match a session minted under the old rule, so in
            // practice it now fails at the join check rather than here.
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
