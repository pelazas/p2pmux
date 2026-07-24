//! Bounded Iroh endpoint operations for the p2pmux handshake transport.

use std::{error::Error, fmt, time::Duration};

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{
        ClosedStream, ConnectingError, Connection, ConnectionError, Incoming, ReadError,
        ReadToEndError, RecvStream, SendStream, WriteError, presets,
    },
};
use tokio::time::timeout;

use crate::protocol::{Envelope, MAX_FRAME_BYTES, ProtocolError, decode_frame, encode_frame};

pub const ALPN: &[u8] = b"p2pmux/1";
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
    StreamRead(ReadError),
    Finish(ClosedStream),
    Protocol(ProtocolError),
    TruncatedStreamFrame,
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
            Self::StreamRead(error) => write!(formatter, "Iroh stream read failed: {error}"),
            Self::Finish(error) => write!(formatter, "Iroh stream finish failed: {error}"),
            Self::Protocol(error) => write!(formatter, "Iroh frame is invalid: {error}"),
            Self::TruncatedStreamFrame => {
                formatter.write_str("Iroh stream ended with a truncated frame")
            }
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
            Self::StreamRead(error) => Some(error),
            Self::Finish(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Closed | Self::TimedOut(_) | Self::TruncatedStreamFrame => None,
        }
    }
}

/// Incremental reader for a long-lived sequence of validated protocol frames.
pub struct FrameReader {
    recv: RecvStream,
    pending: Vec<u8>,
}

/// Long-lived writer that owns exactly one QUIC send stream.
pub struct FrameWriter {
    send: SendStream,
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
        timeout(CONNECT_TIMEOUT, self.endpoint.online())
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
        timeout(CONNECT_TIMEOUT, self.endpoint.connect(remote, ALPN))
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

    pub async fn open_framed_bi(
        &self,
        connection: &Connection,
    ) -> Result<(FrameWriter, FrameReader), TransportError> {
        let (send, recv) = self.open_bi(connection).await?;
        Ok((FrameWriter { send }, FrameReader::new(recv)))
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

    pub async fn accept_framed_bi(
        &self,
        connection: &Connection,
    ) -> Result<(FrameWriter, FrameReader), TransportError> {
        let (send, recv) = self.accept_bi(connection).await?;
        Ok((FrameWriter { send }, FrameReader::new(recv)))
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

impl FrameReader {
    fn new(recv: RecvStream) -> Self {
        Self {
            recv,
            pending: Vec::new(),
        }
    }

    pub async fn read_next(&mut self) -> Result<Option<Envelope>, TransportError> {
        loop {
            if let Some(frame_len) = complete_frame_len(&self.pending)? {
                let frame = self.pending.drain(..frame_len).collect::<Vec<_>>();
                return decode_frame(&frame)
                    .map(Some)
                    .map_err(TransportError::Protocol);
            }
            if self.pending.len() >= MAX_FRAME_BYTES {
                return Err(TransportError::Protocol(ProtocolError::FrameTooLarge {
                    limit: MAX_FRAME_BYTES,
                    actual: self.pending.len(),
                }));
            }
            let maximum = (MAX_FRAME_BYTES - self.pending.len()).min(16 * 1024);
            match self
                .recv
                .read_chunk(maximum)
                .await
                .map_err(TransportError::StreamRead)?
            {
                Some(chunk) => self.pending.extend_from_slice(&chunk),
                None if self.pending.is_empty() => return Ok(None),
                None => return Err(TransportError::TruncatedStreamFrame),
            }
        }
    }
}

impl FrameWriter {
    pub async fn write_next(&mut self, envelope: &Envelope) -> Result<(), TransportError> {
        let frame = encode_frame(envelope).map_err(TransportError::Protocol)?;
        timeout(HANDSHAKE_TIMEOUT, self.send.write_all(&frame))
            .await
            .map_err(|_| TransportError::TimedOut("frame write"))?
            .map_err(TransportError::Write)
    }

    pub fn finish(mut self) -> Result<(), TransportError> {
        self.send.finish().map_err(TransportError::Finish)
    }
}

fn complete_frame_len(pending: &[u8]) -> Result<Option<usize>, TransportError> {
    let Some((declared, prefix_len)) = stream_length_prefix(pending)? else {
        return Ok(None);
    };
    if declared > crate::protocol::MAX_ENVELOPE_BYTES {
        return Err(TransportError::Protocol(ProtocolError::FrameTooLarge {
            limit: crate::protocol::MAX_ENVELOPE_BYTES,
            actual: declared,
        }));
    }
    let frame_len = prefix_len.checked_add(declared).ok_or_else(|| {
        TransportError::Protocol(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: declared,
        })
    })?;
    if frame_len > MAX_FRAME_BYTES {
        return Err(TransportError::Protocol(ProtocolError::FrameTooLarge {
            limit: MAX_FRAME_BYTES,
            actual: frame_len,
        }));
    }
    Ok((pending.len() >= frame_len).then_some(frame_len))
}

fn stream_length_prefix(pending: &[u8]) -> Result<Option<(usize, usize)>, TransportError> {
    let mut value = 0_u64;
    for index in 0..10 {
        let Some(byte) = pending.get(index).copied() else {
            return Ok(None);
        };
        let bits = u64::from(byte & 0x7f);
        if index == 9 && bits > 1 {
            return Err(TransportError::Protocol(
                ProtocolError::MalformedLengthPrefix,
            ));
        }
        value |= bits << (index * 7);
        if byte & 0x80 == 0 {
            let declared = usize::try_from(value)
                .map_err(|_| TransportError::Protocol(ProtocolError::MalformedLengthPrefix))?;
            return Ok(Some((declared, index + 1)));
        }
    }
    Err(TransportError::Protocol(
        ProtocolError::MalformedLengthPrefix,
    ))
}
