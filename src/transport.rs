//! Bounded Iroh endpoint operations for the p2pmux handshake transport.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, EndpointId, TransportAddr,
    endpoint::{
        ClosedStream, ConnectingError, Connection, ConnectionError, Incoming, ReadError,
        ReadToEndError, RecvStream, SendStream, WriteError, presets,
    },
};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::protocol::{Envelope, MAX_FRAME_BYTES, ProtocolError, decode_frame, encode_frame};

pub const ALPN: &[u8] = b"p2pmux/2";
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often an observed connection is re-sampled for its selected path and RTT.
///
/// Iroh can also push path changes as they happen, but a status bar does not need that
/// resolution and polling keeps the observer free of borrows on the connection, which is
/// what lets it live in a plain spawned task.
const PATH_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Reported RTTs are rounded up to this many milliseconds. See `sample_path`.
const RTT_ROUNDING_MS: u64 = 5;

/// Which kind of network path a peer's traffic is actually taking right now.
///
/// This is the single most useful fact when an internet session misbehaves: it separates
/// "holepunching failed and we are bouncing off a relay in another country" from "the
/// path is fine and the problem is ours". Without it, every failure looks the same.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// A holepunched, peer-to-peer UDP path.
    Direct,
    /// Traffic is being forwarded by a relay server.
    Relayed,
    /// A custom transport, which p2pmux does not currently configure.
    Other,
}

impl PathKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relayed => "relayed",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for PathKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The live path to one remote peer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PeerPath {
    /// The remote endpoint's public key, so a caller can match this to a roster entry.
    pub peer_id: Vec<u8>,
    pub kind: PathKind,
    /// Round-trip estimate for the selected path, absent until QUIC has measured one.
    pub rtt_ms: Option<u64>,
}

impl PeerPath {
    /// `direct 34ms`, or just `direct` before the first RTT estimate exists.
    ///
    /// A measured but sub-millisecond path reports `<1ms` rather than `0ms`: a loopback or
    /// same-LAN link really is that fast, and printing `0ms` reads as a broken measurement
    /// instead of a very good one.
    pub fn summary(&self) -> String {
        match self.rtt_ms {
            Some(0) => format!("{} <1ms", self.kind),
            Some(rtt) => format!("{} {rtt}ms", self.kind),
            None => self.kind.to_string(),
        }
    }
}

/// One line describing the session's connectivity, or `None` when no peer is connected.
///
/// Reports the *worst* path rather than an average: one peer stuck on a relay is the
/// thing worth seeing, and averaging would hide it behind everybody else's direct links.
/// A relayed path is always worse than a direct one; among equals, higher RTT wins.
pub fn link_summary(paths: &[PeerPath]) -> Option<String> {
    let worst = paths.iter().max_by_key(|path| {
        (
            u8::from(path.kind == PathKind::Relayed),
            path.rtt_ms.unwrap_or(0),
        )
    })?;
    Some(match paths.len() {
        1 => worst.summary(),
        count => format!("{} ×{count}", worst.summary()),
    })
}

/// Live per-peer path state, shared between the observer tasks and whoever renders it.
#[derive(Clone, Default)]
pub struct PathRegistry {
    entries: Arc<Mutex<BTreeMap<[u8; 32], (PathKind, Option<u64>)>>>,
}

impl PathRegistry {
    fn record(&self, peer: [u8; 32], sample: (PathKind, Option<u64>)) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(peer, sample);
        }
    }

    fn forget(&self, peer: &[u8; 32]) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(peer);
        }
    }

    /// Every peer with a live connection, ordered by peer id so the display is stable.
    pub fn snapshot(&self) -> Vec<PeerPath> {
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        entries
            .iter()
            .map(|(peer_id, (kind, rtt_ms))| PeerPath {
                peer_id: peer_id.to_vec(),
                kind: *kind,
                rtt_ms: *rtt_ms,
            })
            .collect()
    }
}

/// The selected path's kind and RTT, or `None` before any path has been selected.
fn sample_path(connection: &Connection) -> Option<(PathKind, Option<u64>)> {
    let paths = connection.paths();
    let selected = paths.iter().find(|path| path.is_selected())?;
    let kind = match selected.remote_addr() {
        TransportAddr::Ip(_) => PathKind::Direct,
        TransportAddr::Relay(_) => PathKind::Relayed,
        _ => PathKind::Other,
    };
    // A zero RTT means QUIC has not measured one yet; reporting it as `0ms` would read
    // as a suspiciously perfect link rather than as missing data.
    //
    // Rounded to 5ms because this value is republished to the client whenever it changes:
    // raw millisecond jitter would push an IPC message every sample for a number nobody
    // reads that precisely.
    let rtt = selected.rtt();
    let rtt_ms = (!rtt.is_zero()).then(|| {
        let millis = rtt.as_millis().min(u128::from(u64::MAX)) as u64;
        millis.div_ceil(RTT_ROUNDING_MS) * RTT_ROUNDING_MS
    });
    Some((kind, rtt_ms))
}

#[derive(Clone)]
pub struct Transport {
    endpoint: Endpoint,
    paths: PathRegistry,
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
        Ok(Self::from_endpoint(endpoint))
    }

    pub fn from_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            paths: PathRegistry::default(),
        }
    }

    /// Live path state for every peer this transport is currently connected to.
    pub fn paths(&self) -> Vec<PeerPath> {
        self.paths.snapshot()
    }

    /// Start reporting the selected path and RTT for one connection.
    ///
    /// Outbound connections are observed automatically by [`Transport::connect`]; an
    /// accepted connection has to be handed here explicitly, because it is produced by
    /// awaiting an [`Incoming`] rather than by this type.
    ///
    /// The observer stops and forgets the peer when the connection closes, so a stale
    /// `direct 20ms` can never outlive the link it described.
    pub fn observe(&self, connection: &Connection) {
        let peer = *connection.remote_id().as_bytes();
        let registry = self.paths.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            loop {
                if let Some(sample) = sample_path(&connection) {
                    registry.record(peer, sample);
                }
                tokio::select! {
                    _ = tokio::time::sleep(PATH_SAMPLE_INTERVAL) => {}
                    _ = connection.closed() => break,
                }
            }
            registry.forget(&peer);
        });
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
        self.accept_incoming_with_timeout(HANDSHAKE_TIMEOUT).await
    }

    pub async fn accept_incoming_with_timeout(
        &self,
        accept_timeout: Duration,
    ) -> Result<Incoming, TransportError> {
        timeout(accept_timeout, self.endpoint.accept())
            .await
            .map_err(|_| TransportError::TimedOut("incoming accept"))?
            .ok_or(TransportError::Closed)
    }

    pub async fn connect(&self, remote: EndpointAddr) -> Result<Connection, TransportError> {
        let connection = timeout(CONNECT_TIMEOUT, self.endpoint.connect(remote, ALPN))
            .await
            .map_err(|_| TransportError::TimedOut("connect"))?
            .map_err(TransportError::Connect)?;
        self.observe(&connection);
        Ok(connection)
    }

    pub async fn accept_connection(&self) -> Result<Connection, TransportError> {
        let incoming = self.accept_incoming().await?;
        let connection = timeout(HANDSHAKE_TIMEOUT, incoming)
            .await
            .map_err(|_| TransportError::TimedOut("connection handshake"))?
            .map_err(TransportError::Accept)?;
        self.observe(&connection);
        Ok(connection)
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

    /// Accept a post-handshake control stream without imposing the handshake timeout while idle.
    pub async fn accept_framed_bi_when_ready(
        &self,
        connection: &Connection,
    ) -> Result<(FrameWriter, FrameReader), TransportError> {
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(TransportError::Stream)?;
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
