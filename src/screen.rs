//! Fixed-grid vt100 state codec used only by the host and guest renderers.

use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

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

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
/// Give up on a synchronized update after this long and show what arrived.
const MAX_SYNC_HOLD: Duration = Duration::from_millis(150);
/// Cap held bytes so a stuck application cannot buffer without bound.
const MAX_SYNC_BYTES: usize = 4 * 1024 * 1024;
/// Flush a held partial marker whose continuation never arrives.
const MAX_CARRY_HOLD: Duration = Duration::from_millis(25);

/// Applications wrap atomic redraws in DEC private mode 2026 (synchronized
/// output). This gate holds a pane's PTY bytes from the begin marker until the
/// end marker so the screen is never parsed — and thus never rendered or
/// broadcast — mid-redraw. The markers themselves are stripped; time and size
/// caps bound how long a misbehaving application can freeze the pane.
#[derive(Default)]
pub struct SyncGate {
    /// Bytes held while a synchronized update is open.
    held: Vec<u8>,
    /// How much of `held` was already scanned for the end marker.
    scanned: usize,
    /// Trailing bytes that may be a split begin marker, kept until the next chunk.
    carry: Vec<u8>,
    carry_since: Option<Instant>,
    active_since: Option<Instant>,
}

impl SyncGate {
    /// Feeds raw PTY bytes and returns the prefix that is safe to parse now.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let mut input = std::mem::take(&mut self.carry);
        self.carry_since = None;
        input.extend_from_slice(bytes);
        let mut ready = Vec::new();
        let mut cursor = 0;
        loop {
            if self.active_since.is_some() {
                self.held.extend_from_slice(&input[cursor..]);
                let scan_from = self.scanned.saturating_sub(SYNC_END.len() - 1);
                let found = find_subsequence(&self.held[scan_from..], SYNC_END)
                    .map(|offset| scan_from + offset);
                self.scanned = self.held.len();
                if let Some(end) = found {
                    let remainder = self.held.split_off(end + SYNC_END.len());
                    ready.extend_from_slice(&self.held[..end]);
                    self.held.clear();
                    self.reset_hold();
                    input = remainder;
                    cursor = 0;
                    continue;
                }
                if self.hold_expired(now) {
                    ready.append(&mut self.held);
                    self.reset_hold();
                }
                break;
            }
            match find_subsequence(&input[cursor..], SYNC_BEGIN) {
                Some(offset) => {
                    ready.extend_from_slice(&input[cursor..cursor + offset]);
                    cursor += offset + SYNC_BEGIN.len();
                    self.active_since = Some(now);
                }
                None => {
                    let tail = &input[cursor..];
                    let keep = partial_marker_suffix(tail, SYNC_BEGIN);
                    ready.extend_from_slice(&tail[..tail.len() - keep]);
                    if keep > 0 {
                        self.carry.extend_from_slice(&tail[tail.len() - keep..]);
                        self.carry_since = Some(now);
                    }
                    break;
                }
            }
        }
        ready
    }

    /// Returns bytes whose hold deadline passed. Call when no new output arrived.
    pub fn flush_stale(&mut self, now: Instant) -> Vec<u8> {
        if self.active_since.is_some() && self.hold_expired(now) {
            self.reset_hold();
            return std::mem::take(&mut self.held);
        }
        if self
            .carry_since
            .is_some_and(|since| now.duration_since(since) >= MAX_CARRY_HOLD)
        {
            self.carry_since = None;
            return std::mem::take(&mut self.carry);
        }
        Vec::new()
    }

    fn hold_expired(&self, now: Instant) -> bool {
        self.held.len() > MAX_SYNC_BYTES
            || self
                .active_since
                .is_some_and(|since| now.duration_since(since) >= MAX_SYNC_HOLD)
    }

    fn reset_hold(&mut self) {
        self.active_since = None;
        self.scanned = 0;
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Length of the longest strict marker prefix that the slice ends with.
fn partial_marker_suffix(tail: &[u8], marker: &[u8]) -> usize {
    let max = tail.len().min(marker.len() - 1);
    (1..=max)
        .rev()
        .find(|&len| tail[tail.len() - len..] == marker[..len])
        .unwrap_or(0)
}

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
        // A large batch can outgrow the wire delta cap; fall back to the
        // snapshot-only frame shape (as resize does) so consumers replace
        // their screen instead of patching it.
        let (base_sequence, delta) = if delta.len() > MAX_DELTA_BYTES {
            (0, Vec::new())
        } else {
            (self.current.sequence, delta)
        };
        let frame = ScreenFrame {
            sequence,
            base_sequence,
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
    use std::time::{Duration, Instant};

    use super::{HostScreen, MAX_SYNC_BYTES, MAX_SYNC_HOLD, SyncGate};

    #[test]
    fn sync_gate_passes_unmarked_output_through() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"plain output", now), b"plain output");
    }

    #[test]
    fn sync_gate_holds_between_markers_and_strips_them() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"pre\x1b[?2026hheld", now), b"pre");
        assert_eq!(gate.feed(b" more", now), b"");
        assert_eq!(gate.feed(b"\x1b[?2026lpost", now), b"held morepost");
    }

    #[test]
    fn sync_gate_handles_complete_update_in_one_chunk() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(
            gate.feed(b"a\x1b[?2026hb\x1b[?2026lc", now),
            b"abc".to_vec()
        );
    }

    #[test]
    fn sync_gate_reassembles_marker_split_across_chunks() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"pre\x1b[?20", now), b"pre");
        assert_eq!(gate.feed(b"26hheld\x1b[?2026lpost", now), b"heldpost");
    }

    #[test]
    fn sync_gate_flushes_stale_carry() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"tail\x1b[?2", now), b"tail");
        assert_eq!(gate.flush_stale(now), b"");
        let later = now + Duration::from_millis(30);
        assert_eq!(gate.flush_stale(later), b"\x1b[?2");
    }

    #[test]
    fn sync_gate_gives_up_after_hold_deadline() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"\x1b[?2026hstuck", now), b"");
        assert_eq!(gate.flush_stale(now), b"");
        let later = now + MAX_SYNC_HOLD;
        assert_eq!(gate.flush_stale(later), b"stuck");
        assert_eq!(gate.feed(b"after", later), b"after");
    }

    #[test]
    fn sync_gate_gives_up_past_size_cap() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"\x1b[?2026h", now), b"");
        let big = vec![b'x'; MAX_SYNC_BYTES + 1];
        assert_eq!(gate.feed(&big, now), big);
        assert_eq!(gate.feed(b"after", now), b"after");
    }

    #[test]
    fn sync_gate_handles_back_to_back_updates() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(
            gate.feed(b"\x1b[?2026ha\x1b[?2026l\x1b[?2026hb\x1b[?2026lc", now),
            b"abc".to_vec()
        );
    }

    #[test]
    fn sync_gate_passes_end_marker_without_begin_through() {
        let mut gate = SyncGate::default();
        let now = Instant::now();
        assert_eq!(gate.feed(b"a\x1b[?2026lb", now), b"a\x1b[?2026lb");
    }

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
