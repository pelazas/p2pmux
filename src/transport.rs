//! Bounded Iroh endpoint operations for the p2pmux handshake transport.

use std::{error::Error, fmt, time::Duration};

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{ConnectingError, Connection, Incoming, presets},
};
use tokio::time::timeout;

pub const ALPN: &[u8] = b"p2pmux/1";
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Transport {
    endpoint: Endpoint,
}

#[derive(Debug)]
pub enum TransportError {
    Bind(iroh::endpoint::BindError),
    Connect(iroh::endpoint::ConnectError),
    Accept(ConnectingError),
    Closed,
    TimedOut(&'static str),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(error) => write!(formatter, "failed to bind Iroh endpoint: {error}"),
            Self::Connect(error) => write!(formatter, "failed to connect Iroh endpoint: {error}"),
            Self::Accept(error) => write!(formatter, "failed to accept Iroh connection: {error}"),
            Self::Closed => formatter.write_str("Iroh endpoint is closed"),
            Self::TimedOut(operation) => write!(formatter, "Iroh {operation} timed out"),
        }
    }
}

impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            Self::Connect(error) => Some(error),
            Self::Accept(error) => Some(error),
            Self::Closed | Self::TimedOut(_) => None,
        }
    }
}

impl Transport {
    pub async fn bind() -> Result<Self, TransportError> {
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(TransportError::Bind)?;
        Ok(Self { endpoint })
    }

    pub fn from_endpoint(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub async fn wait_until_online(&self) -> bool {
        timeout(HANDSHAKE_TIMEOUT, self.endpoint.online())
            .await
            .is_ok()
    }

    pub async fn accept_incoming(&self) -> Result<Incoming, TransportError> {
        timeout(HANDSHAKE_TIMEOUT, self.endpoint.accept())
            .await
            .map_err(|_| TransportError::TimedOut("incoming accept"))?
            .ok_or(TransportError::Closed)
    }

    pub async fn connect(&self, remote: EndpointAddr) -> Result<Connection, TransportError> {
        timeout(HANDSHAKE_TIMEOUT, self.endpoint.connect(remote, ALPN))
            .await
            .map_err(|_| TransportError::TimedOut("connect"))?
            .map_err(TransportError::Connect)
    }

    pub async fn accept_connection(&self) -> Result<Connection, TransportError> {
        let incoming = self.accept_incoming().await?;
        timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| TransportError::TimedOut("connection handshake"))?
            .map_err(TransportError::Accept)
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}
