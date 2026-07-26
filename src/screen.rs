//! Fixed-grid vt100 state codec used only by the host and guest renderers.

use std::{fmt, sync::Arc};

use crate::{
    kitty_keyboard::KittyKeyboardTracker,
    protocol::{MAX_DELTA_BYTES, MAX_SNAPSHOT_BYTES},
};

pub const SCREEN_CODEC_VERSION: u8 = 1;
pub(crate) const SCROLLBACK_LINES: usize = 10_000;
const SNAPSHOT_HEADER_BYTES: usize = 5;

#[derive(Clone, Debug)]
pub struct ScreenFrame {
    pub sequence: u64,
    pub base_sequence: u64,
    pub snapshot: Arc<[u8]>,
    pub delta: Arc<[u8]>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ScreenError {
    InvalidDimensions,
    MalformedSnapshot,
    UnsupportedVersion(u8),
    SnapshotTooLarge(usize),
    DeltaTooLarge(usize),
    InvalidSequence,
    SequenceExhausted,
}

impl fmt::Display for ScreenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => formatter.write_str("screen dimensions must be nonzero"),
            Self::MalformedSnapshot => formatter.write_str("malformed screen snapshot"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported screen codec version {version}")
            }
            Self::SnapshotTooLarge(size) => {
                write!(formatter, "screen snapshot exceeds cap: {size}")
            }
            Self::DeltaTooLarge(size) => write!(formatter, "screen delta exceeds cap: {size}"),
            Self::InvalidSequence => formatter.write_str("invalid screen sequence"),
            Self::SequenceExhausted => formatter.write_str("screen sequence exhausted"),
        }
    }
}

impl std::error::Error for ScreenError {}

pub struct HostScreen {
    parser: vt100::Parser,
    previous: vt100::Screen,
    current: ScreenFrame,
    kitty_keyboard: KittyKeyboardTracker,
}

impl HostScreen {
    pub fn new(rows: u16, cols: u16) -> Result<Self, ScreenError> {
        validate_dimensions(rows, cols)?;
        let parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        let previous = parser.screen().clone();
        let snapshot = snapshot_payload(parser.screen())?;
        Ok(Self {
            parser,
            previous,
            current: ScreenFrame {
                sequence: 1,
                base_sequence: 0,
                snapshot,
                delta: Arc::from([]),
            },
            kitty_keyboard: KittyKeyboardTracker::default(),
        })
    }

    pub fn process_pty(&mut self, bytes: &[u8]) -> Result<ScreenFrame, ScreenError> {
        self.kitty_keyboard.observe(bytes);
        self.parser.process(bytes);
        let sequence = self
            .current
            .sequence
            .checked_add(1)
            .ok_or(ScreenError::SequenceExhausted)?;
        let snapshot = snapshot_payload(self.parser.screen())?;
        let delta = self.parser.screen().state_diff(&self.previous);
        if delta.len() > MAX_DELTA_BYTES {
            return Err(ScreenError::DeltaTooLarge(delta.len()));
        }
        let frame = ScreenFrame {
            sequence,
            base_sequence: self.current.sequence,
            snapshot,
            delta: Arc::from(delta),
        };
        self.previous = self.parser.screen().clone();
        self.current = frame.clone();
        Ok(frame)
    }

    /// Resize the parser and force consumers to replace rather than delta-apply their screen.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<ScreenFrame, ScreenError> {
        validate_dimensions(rows, cols)?;
        let sequence = self
            .current
            .sequence
            .checked_add(1)
            .ok_or(ScreenError::SequenceExhausted)?;
        self.parser.screen_mut().set_size(rows, cols);
        self.previous = self.parser.screen().clone();
        let frame = ScreenFrame {
            sequence,
            base_sequence: 0,
            snapshot: snapshot_payload(self.parser.screen())?,
            delta: Arc::from([]),
        };
        self.current = frame.clone();
        Ok(frame)
    }

    pub fn current_frame(&self) -> &ScreenFrame {
        &self.current
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn kitty_keyboard_active(&self) -> bool {
        self.kitty_keyboard.active()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ApplyDelta {
    Applied,
    NeedsSnapshot,
}

pub struct GuestScreen {
    parser: Option<vt100::Parser>,
    sequence: Option<u64>,
}

impl Default for GuestScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestScreen {
    pub fn new() -> Self {
        Self {
            parser: None,
            sequence: None,
        }
    }

    pub fn apply_snapshot(&mut self, sequence: u64, payload: &[u8]) -> Result<(), ScreenError> {
        if sequence == 0 {
            return Err(ScreenError::InvalidSequence);
        }
        if payload.len() > MAX_SNAPSHOT_BYTES {
            return Err(ScreenError::SnapshotTooLarge(payload.len()));
        }
        let (rows, cols, state) = decode_snapshot(payload)?;
        let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        parser.process(state);
        self.parser = Some(parser);
        self.sequence = Some(sequence);
        Ok(())
    }

    pub fn apply_delta(
        &mut self,
        base_sequence: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<ApplyDelta, ScreenError> {
        if payload.len() > MAX_DELTA_BYTES {
            return Err(ScreenError::DeltaTooLarge(payload.len()));
        }
        if base_sequence == 0 || sequence <= base_sequence {
            return Err(ScreenError::InvalidSequence);
        }
        if self.sequence != Some(base_sequence) {
            return Ok(ApplyDelta::NeedsSnapshot);
        }
        let parser = self.parser.as_mut().ok_or(ScreenError::MalformedSnapshot)?;
        parser.process(payload);
        self.sequence = Some(sequence);
        Ok(ApplyDelta::Applied)
    }

    pub fn screen(&self) -> Option<&vt100::Screen> {
        self.parser.as_ref().map(vt100::Parser::screen)
    }

    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }
}

fn snapshot_payload(screen: &vt100::Screen) -> Result<Arc<[u8]>, ScreenError> {
    let (rows, cols) = screen.size();
    validate_dimensions(rows, cols)?;
    let state = screen.state_formatted();
    let size = SNAPSHOT_HEADER_BYTES
        .checked_add(state.len())
        .ok_or(ScreenError::SnapshotTooLarge(usize::MAX))?;
    if size > MAX_SNAPSHOT_BYTES {
        return Err(ScreenError::SnapshotTooLarge(size));
    }
    let mut payload = Vec::with_capacity(size);
    payload.push(SCREEN_CODEC_VERSION);
    payload.extend_from_slice(&rows.to_be_bytes());
    payload.extend_from_slice(&cols.to_be_bytes());
    payload.extend_from_slice(&state);
    Ok(Arc::from(payload))
}

fn decode_snapshot(payload: &[u8]) -> Result<(u16, u16, &[u8]), ScreenError> {
    if payload.len() < SNAPSHOT_HEADER_BYTES {
        return Err(ScreenError::MalformedSnapshot);
    }
    if payload[0] != SCREEN_CODEC_VERSION {
        return Err(ScreenError::UnsupportedVersion(payload[0]));
    }
    let rows = u16::from_be_bytes([payload[1], payload[2]]);
    let cols = u16::from_be_bytes([payload[3], payload[4]]);
    validate_dimensions(rows, cols)?;
    Ok((rows, cols, &payload[SNAPSHOT_HEADER_BYTES..]))
}

fn validate_dimensions(rows: u16, cols: u16) -> Result<(), ScreenError> {
    if rows == 0 || cols == 0 {
        return Err(ScreenError::InvalidDimensions);
    }
    Ok(())
}
