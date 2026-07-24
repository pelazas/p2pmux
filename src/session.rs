//! Authenticated Join/Welcome session handshakes over Iroh bi-streams.

use std::{error::Error, fmt};

use iroh::{
    EndpointId,
    endpoint::{ConnectingError, Connection, Incoming},
};
use tokio::time::timeout;

use crate::{
    protocol::{Envelope, Join, PROTOCOL_VERSION, Welcome, envelope},
    ticket::{JoinTicket, TicketError},
    transport::{HANDSHAKE_TIMEOUT, Transport, TransportError},
};

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
            | Self::UnauthenticatedPeer => None,
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
        match self.handle_connection(&connection).await {
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

    async fn handle_connection(
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

async fn join_connected(
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
