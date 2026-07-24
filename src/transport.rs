//! Bounded Iroh endpoint operations for the p2pmux handshake transport.

use std::{error::Error, fmt, time::Duration};

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{
        ClosedStream, ConnectingError, Connection, ConnectionError, Incoming, ReadToEndError,
        RecvStream, SendStream, WriteError, presets,
    },
};
use tokio::time::timeout;

use crate::protocol::{Envelope, MAX_FRAME_BYTES, ProtocolError, decode_frame, encode_frame};

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
    Stream(ConnectionError),
    Write(WriteError),
    Read(ReadToEndError),
    Finish(ClosedStream),
    Protocol(ProtocolError),
    Closed,
    TimedOut(&'static str),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(error) => write!(formatter, "failed to bind Iroh endpoint: {error}"),
            Self::Connect(error) => write!(formatter, "failed to connect Iroh endpoint: {error}"),
            Self::Accept(error) => write!(formatter, "failed to accept Iroh connection: {error}"),
            Self::Stream(error) => write!(formatter, "Iroh stream operation failed: {error}"),
            Self::Write(error) => write!(formatter, "Iroh stream write failed: {error}"),
            Self::Read(error) => write!(formatter, "Iroh stream read failed: {error}"),
            Self::Finish(error) => write!(formatter, "Iroh stream finish failed: {error}"),
            Self::Protocol(error) => write!(formatter, "Iroh frame is invalid: {error}"),
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
            Self::Stream(error) => Some(error),
            Self::Write(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Finish(error) => Some(error),
            Self::Protocol(error) => Some(error),
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

    pub async fn open_bi(
        &self,
        connection: &Connection,
    ) -> Result<(SendStream, RecvStream), TransportError> {
        timeout(HANDSHAKE_TIMEOUT, connection.open_bi())
            .await
            .map_err(|_| TransportError::TimedOut("open bi-stream"))?
            .map_err(TransportError::Stream)
    }

    pub async fn accept_bi(
        &self,
        connection: &Connection,
    ) -> Result<(SendStream, RecvStream), TransportError> {
        timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| TransportError::TimedOut("accept bi-stream"))?
            .map_err(TransportError::Stream)
    }

    pub async fn write_frame(
        &self,
        send: &mut SendStream,
        envelope: &Envelope,
    ) -> Result<(), TransportError> {
        let frame = encode_frame(envelope).map_err(TransportError::Protocol)?;
        timeout(HANDSHAKE_TIMEOUT, send.write_all(&frame))
            .await
            .map_err(|_| TransportError::TimedOut("frame write"))?
            .map_err(TransportError::Write)?;
        send.finish().map_err(TransportError::Finish)
    }

    pub async fn read_frame(&self, recv: &mut RecvStream) -> Result<Envelope, TransportError> {
        let frame = timeout(HANDSHAKE_TIMEOUT, recv.read_to_end(MAX_FRAME_BYTES))
            .await
            .map_err(|_| TransportError::TimedOut("frame read"))?
            .map_err(TransportError::Read)?;
        decode_frame(&frame).map_err(TransportError::Protocol)
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}
