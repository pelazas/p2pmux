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
    pub kitty_keyboard_active: bool,
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
    history_floor: u64,
    history_end: u64,
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
                kitty_keyboard_active: false,
            },
            kitty_keyboard: KittyKeyboardTracker::default(),
            history_floor: 0,
            history_end: 0,
        })
    }

    pub fn process_pty(&mut self, bytes: &[u8]) -> Result<ScreenFrame, ScreenError> {
        let before_history = screen_scrollback_len(self.parser.screen());
        self.kitty_keyboard.observe(bytes);
        self.parser.process(bytes);
        let after_history = screen_scrollback_len(self.parser.screen());
        let appended = after_history.saturating_sub(before_history).max(
            usize::from(before_history == SCROLLBACK_LINES && after_history == SCROLLBACK_LINES)
                * bytes.iter().filter(|byte| **byte == b'\n').count(),
        );
        self.history_end = self.history_end.saturating_add(appended as u64);
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
            kitty_keyboard_active: self.kitty_keyboard.active(),
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
        self.history_floor = self.history_end;
        let frame = ScreenFrame {
            sequence,
            base_sequence: 0,
            snapshot: snapshot_payload(self.parser.screen())?,
            delta: Arc::from([]),
            kitty_keyboard_active: self.current.kitty_keyboard_active,
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

    /// Returns available rows and the monotonic end position without formatting rows.
    pub fn history_metadata(&self) -> (u64, u64) {
        if self.parser.screen().alternate_screen() {
            return (0, 0);
        }
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(SCROLLBACK_LINES);
        let retained = screen.scrollback() as u64;
        let first = self
            .history_end
            .saturating_sub(retained)
            .max(self.history_floor);
        (self.history_end.saturating_sub(first), self.history_end)
    }

    /// Returns a bounded history window. `offset` skips newest rows; returned rows are oldest to
    /// newest.
    pub fn visual_scrollback_window(
        &self,
        offset: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> (u64, Vec<Vec<u8>>) {
        let (total_rows, history_end) = self.history_metadata();
        if total_rows == 0 {
            return (0, Vec::new());
        }
        let offset = offset.min(total_rows);
        let last = total_rows.saturating_sub(offset);
        let first = last.saturating_sub(max_rows as u64);
        let mut rows = Vec::new();
        let mut bytes = 0_usize;
        let retained = screen_scrollback_len(self.parser.screen()) as u64;
        let retained_start = history_end.saturating_sub(retained);
        let available_start = history_end.saturating_sub(total_rows);
        let mut screen = self.parser.screen().clone();
        for row in first..last {
            let absolute = available_start.saturating_add(row);
            let physical = absolute.saturating_sub(retained_start);
            screen.set_scrollback(retained.saturating_sub(physical) as usize);
            let Some(formatted) = screen.rows_formatted(0, screen.size().1).next() else {
                continue;
            };
            if bytes.saturating_add(formatted.len()) > max_bytes {
                break;
            }
            bytes = bytes.saturating_add(formatted.len());
            rows.push(formatted);
        }
        (total_rows, rows)
    }

    /// Compatibility helper for callers that need the newest window.
    pub fn visual_scrollback(&self, max_rows: usize, max_bytes: usize) -> (usize, Vec<Vec<u8>>) {
        let (total, rows) = self.visual_scrollback_window(0, max_rows, max_bytes);
        (total as usize, rows)
    }

    pub fn take_kitty_keyboard_query_reply(&mut self) -> Option<Vec<u8>> {
        self.kitty_keyboard.take_query_reply()
    }
}

fn screen_scrollback_len(screen: &vt100::Screen) -> usize {
    let mut screen = screen.clone();
    screen.set_scrollback(SCROLLBACK_LINES);
    screen.scrollback()
}

#[derive(Debug, Eq, PartialEq)]
pub enum ApplyDelta {
    Applied,
    NeedsSnapshot,
}

pub struct GuestScreen {
    parser: Option<vt100::Parser>,
    sequence: Option<u64>,
    kitty_keyboard_active: bool,
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
            kitty_keyboard_active: false,
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

    pub fn set_kitty_keyboard_active(&mut self, active: bool) {
        self.kitty_keyboard_active = active;
    }

    pub fn kitty_keyboard_active(&self) -> bool {
        self.kitty_keyboard_active
    }
}

/// Encodes a complete fixed-grid screen for a fresh local or remote renderer.
pub fn snapshot_payload(screen: &vt100::Screen) -> Result<Arc<[u8]>, ScreenError> {
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

#[cfg(test)]
mod tests {
    use super::HostScreen;

    #[test]
    fn visual_scrollback_is_bounded_and_resets_on_resize_or_alternate_screen() {
        let mut screen = HostScreen::new(1, 3).unwrap();
        screen.process_pty(b"a\r\nb\r\nc").unwrap();
        let (total, rows) = screen.visual_scrollback(1, 1024);
        assert!(total >= 1);
        assert_eq!(rows.len(), 1);

        screen.resize(2, 3).unwrap();
        assert_eq!(screen.visual_scrollback(10, 1024), (0, vec![]));

        screen.process_pty(b"\x1b[?1049h").unwrap();
        assert_eq!(screen.visual_scrollback(10, 1024), (0, vec![]));
    }

    #[test]
    fn visual_scrollback_window_skips_newest_rows() {
        let mut screen = HostScreen::new(1, 8).unwrap();
        screen.process_pty(b"one\r\ntwo\r\nthree").unwrap();

        let (total, newest) = screen.visual_scrollback_window(0, 1, 1024);
        let (_, older) = screen.visual_scrollback_window(1, 1, 1024);
        assert!(total >= 2);
        assert_eq!(newest.len(), 1);
        assert_eq!(older.len(), 1);
        assert_ne!(newest, older);
    }
}
