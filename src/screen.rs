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

/// Rewrites the final byte of `CSI … f` to `CSI … H`.
///
/// HVP (`f`) and CUP (`H`) move the cursor to the same place — the distinction is
/// a DEC historical artifact, and every terminal treats them alike. `vt100`
/// implements `H` and leaves `f` unhandled, so in a p2pmux pane a program that
/// positioned with `f` moved the cursor nowhere at all and drew wherever it had
/// been. The pane was answering "you are at 1;1" to a size probe that had just
/// parked the cursor at `999;999`, which is how this surfaced: a program asking
/// how big its terminal is was told one column.
///
/// A state machine rather than a search, because `f` is an ordinary character
/// everywhere except as a CSI's final byte — including inside the OSC strings
/// agents write window titles with.
#[derive(Debug, Default)]
struct HvpRewriter {
    state: VtState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum VtState {
    #[default]
    Ground,
    Escape,
    /// Inside `CSI`, collecting parameters until the final byte.
    Csi,
    /// Inside an OSC/DCS/APC/PM/SOS string, which ends at BEL or ST.
    StringBody,
    /// Saw `ESC` inside such a string: `\` ends it, anything else begins a new escape.
    StringEscape,
}

impl HvpRewriter {
    /// Borrows unless a rewrite is actually needed, which is almost always.
    fn feed<'a>(&mut self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        let mut owned: Option<Vec<u8>> = None;
        for (index, &byte) in bytes.iter().enumerate() {
            self.state = match self.state {
                VtState::Ground if byte == 0x1b => VtState::Escape,
                VtState::Ground => VtState::Ground,
                VtState::Escape => Self::after_escape(byte),
                VtState::Csi if (0x40..=0x7e).contains(&byte) => {
                    if byte == b'f' {
                        owned.get_or_insert_with(|| bytes.to_vec())[index] = b'H';
                    }
                    VtState::Ground
                }
                VtState::Csi => VtState::Csi,
                VtState::StringBody => match byte {
                    0x07 => VtState::Ground,
                    0x1b => VtState::StringEscape,
                    _ => VtState::StringBody,
                },
                VtState::StringEscape if byte == b'\\' => VtState::Ground,
                VtState::StringEscape => Self::after_escape(byte),
            };
        }
        match owned {
            Some(bytes) => std::borrow::Cow::Owned(bytes),
            None => std::borrow::Cow::Borrowed(bytes),
        }
    }

    fn after_escape(byte: u8) -> VtState {
        match byte {
            b'[' => VtState::Csi,
            // OSC, DCS, APC, PM, SOS — everything that runs until a string terminator.
            b']' | b'P' | b'^' | b'_' | b'X' => VtState::StringBody,
            _ => VtState::Ground,
        }
    }
}

/// What a hosted pane's parser hands back: bells, and answers to the questions
/// the program in the pane asks its terminal.
///
/// **Answering matters more than it sounds.** A terminal query is a blocking
/// round trip — the program writes it and reads until the answer arrives. `vt100`
/// implements none of them, so before this a p2pmux pane was simply silent, and
/// anything that asked a question waited forever. `gh secret set` was the report
/// that found it: it asks for the cursor position, gets nothing, and never
/// reaches its own prompt. The same wait is behind any program that measures the
/// terminal by parking the cursor at `999;999` and asking where it ended up.
///
/// Only questions with a truthful answer are answered. The colour queries
/// (`OSC 10`/`OSC 11`) are deliberately left alone: this process cannot see the
/// terminal the human is looking at — for a pane hosted on another member's
/// machine there is no single such terminal — and a made-up background colour
/// makes an application pick a palette against the wrong one. Silence there is a
/// known limitation; inventing an answer would be a bug.
#[derive(Debug, Default)]
pub struct PaneCallbacks {
    count: u64,
    /// Replies to feed back into the pane's PTY, in the order they were asked.
    replies: Vec<u8>,
}

impl PaneCallbacks {
    fn reply(&mut self, bytes: &[u8]) {
        // A program that asks faster than the pane is drained would otherwise
        // grow this without bound. Answers are tiny and a backlog of them is
        // meaningless, so a runaway asker loses its answers rather than the
        // node's memory.
        if self.replies.len() < 4096 {
            self.replies.extend_from_slice(bytes);
        }
    }
}

impl vt100::Callbacks for PaneCallbacks {
    /// Counts standalone audible bells (`^G`).
    ///
    /// This deliberately hangs off the parser rather than scanning the raw PTY bytes for `0x07`:
    /// BEL also terminates an OSC string, and agents set the window title constantly, so a byte
    /// scan would count every title update as a bell. vt100 routes an OSC-terminating BEL through
    /// `osc_dispatch` and only reaches this callback for a standalone bell.
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.count = self.count.saturating_add(1);
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        let first = params.first().and_then(|group| group.first()).copied();
        // Reported one-based, and read from the screen after the batch that
        // asked has been parsed — so the `\x1b[999;999f` in a size probe has
        // already been clamped to the real grid, and the answer states the
        // pane's true size without anyone having to special-case the trick.
        let (row, column) = screen.cursor_position();
        let (rows, columns) = screen.size();
        match (intermediate, final_byte, first) {
            // DSR: cursor position report.
            (None, 'n', Some(6)) => {
                self.reply(format!("\x1b[{};{}R", row + 1, column + 1).as_bytes());
            }
            // DECXCPR: the same, plus a page number, which is always the first.
            (Some(b'?'), 'n', Some(6)) => {
                self.reply(format!("\x1b[?{};{};1R", row + 1, column + 1).as_bytes());
            }
            // DSR: are you there? Reaching this callback is the proof.
            (None, 'n', Some(5)) => self.reply(b"\x1b[0n"),
            // Window manipulation: report the text area in characters.
            (None, 't', Some(18)) => {
                self.reply(format!("\x1b[8;{rows};{columns}t").as_bytes());
            }
            // Primary device attributes. Claimed conservatively — a VT100 with
            // the advanced video option — because an application believes this:
            // claiming a terminal `vt100` cannot render would buy nothing and
            // invite sequences that come out as mojibake.
            (None, 'c', None | Some(0)) => self.reply(b"\x1b[?1;2c"),
            // Secondary device attributes: terminal id 0, version 10, no options.
            (Some(b'>'), 'c', None | Some(0)) => self.reply(b"\x1b[>0;10;0c"),
            _ => {}
        }
    }
}

pub struct HostScreen {
    parser: vt100::Parser<PaneCallbacks>,
    /// Baseline for `state_diff`, held one batch behind `parser`.
    ///
    /// This used to be a clone of the live screen, but `vt100::Screen::clone` copies the
    /// whole row buffer including up to `SCROLLBACK_LINES` retained rows: measured at
    /// 7.9ms for 24x80 and 26ms for 60x200 once scrollback fills, per frame, per pane.
    /// `state_diff` only ever reads `visible_rows()`, so a scrollback-free parser replaying
    /// the same bytes is an equivalent baseline and costs one extra parse of the batch.
    previous: vt100::Parser,
    current: ScreenFrame,
    kitty_keyboard: KittyKeyboardTracker,
    history_end: u64,
    /// Retained scrollback rows, tracked here because reading it from the screen means
    /// moving the scrollback offset, which needs `&mut` that `history_metadata` lacks.
    retained_scrollback: usize,
    /// Carries VT parse state across reads, so a `CSI` split over two of them is
    /// still recognised.
    hvp: HvpRewriter,
}

impl HostScreen {
    pub fn new(rows: u16, cols: u16) -> Result<Self, ScreenError> {
        validate_dimensions(rows, cols)?;
        let parser = vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_LINES,
            PaneCallbacks::default(),
        );
        let snapshot = snapshot_payload(parser.screen())?;
        Ok(Self {
            parser,
            previous: vt100::Parser::new(rows, cols, 0),
            current: ScreenFrame {
                sequence: 1,
                base_sequence: 0,
                snapshot,
                delta: Arc::from([]),
                kitty_keyboard_active: false,
            },
            kitty_keyboard: KittyKeyboardTracker::default(),
            history_end: 0,
            retained_scrollback: 0,
            hvp: HvpRewriter::default(),
        })
    }

    pub fn process_pty(&mut self, bytes: &[u8]) -> Result<ScreenFrame, ScreenError> {
        let before_history = self.retained_scrollback;
        // Both parsers see the same rewritten bytes, or the delta baseline would
        // drift from the live screen by exactly the cursor moves being repaired.
        let bytes = &*self.hvp.feed(bytes);
        self.kitty_keyboard.observe(bytes);
        self.parser.process(bytes);
        let after_history = retained_scrollback_len(self.parser.screen_mut());
        self.retained_scrollback = after_history;
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
        let delta = self.parser.screen().state_diff(self.previous.screen());
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
        // Catch the baseline up to the live screen by replaying the same batch.
        self.previous.process(bytes);
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
        // Read the text out before `set_size` truncates every visible row to the new
        // width. Only a width change can lose anything, and only on the normal screen:
        // an application drawing in the alternate screen owns the whole frame and
        // repaints it on the resize, so replaying the old one would fight its redraw.
        let reflow = (cols != self.parser.screen().size().1
            && !self.parser.screen().alternate_screen())
        .then(|| {
            (
                capture_visible_text(self.parser.screen()),
                self.parser.screen().attributes_formatted(),
            )
        });
        self.parser.screen_mut().set_size(rows, cols);
        if let Some((text, pen)) = reflow {
            self.parser
                .process(&reflow_payload(&text, rows, cols, &pen));
            // Rows the replay pushed past the top are history now, and the monotonic
            // end position has to count them or scrollback addressing drifts.
            let (_, scrolled) = replay_extent(&text, rows, cols);
            self.history_end = self.history_end.saturating_add(scrolled as u64);
        }
        // Replaying the batch keeps the baseline in step everywhere except here: `set_size`
        // reflows against the retained row buffer, which the scrollback-free baseline does
        // not have. Rebuild it from the live visible state instead, exactly as the client
        // rebuilds from the snapshot this frame carries.
        self.previous = vt100::Parser::new(rows, cols, 0);
        self.previous
            .process(&self.parser.screen().state_formatted());
        // The retained rows outlive the resize: `set_size` reflows the visible grid
        // and leaves the scrollback deque alone, so history stays readable at its
        // own width. Dropping it here is what made a pane unscrollable after every
        // window resize, split, or zoom -- and permanently so for a full-frame
        // agent UI, which repaints in place and never scrolls a fresh row in to
        // rebuild what was thrown away.
        self.retained_scrollback = retained_scrollback_len(self.parser.screen_mut());
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

    /// Rows the parser is holding, tracked as output arrives.
    ///
    /// Callers used to read this by cloning the screen and clamping the offset, which
    /// copies every retained row — milliseconds per call once history fills. The count
    /// is already maintained by `process_pty`, so hand it over directly.
    pub fn retained_scrollback(&self) -> usize {
        self.retained_scrollback
    }

    /// Number of audible bells since the last call. An agent that rings when it finishes gives
    /// a far better completion signal than inferring one from how long the pane stayed quiet.
    pub fn take_bell_count(&mut self) -> u64 {
        std::mem::take(&mut self.parser.callbacks_mut().count)
    }

    /// Returns available rows and the monotonic end position without formatting rows.
    pub fn history_metadata(&self) -> (u64, u64) {
        if self.parser.screen().alternate_screen() {
            return (0, 0);
        }
        let retained = self.retained_scrollback as u64;
        let first = self.history_end.saturating_sub(retained);
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
        let retained = self.retained_scrollback as u64;
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

    /// Answers the pane owes its own program, to be written back into its PTY.
    ///
    /// Empty in the ordinary case, so a caller pays a `Vec::is_empty` per batch.
    /// See [`PaneCallbacks`] for why leaving these unanswered wedges a program.
    pub fn take_query_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }
}

/// Retained scrollback rows, read by clamping the scrollback offset and restoring it.
///
/// `set_scrollback` clamps to the retained length and both it and `scrollback` are O(1)
/// field accesses, so this reads the same number the old `screen.clone()` did without
/// copying the row buffer.
fn retained_scrollback_len(screen: &mut vt100::Screen) -> usize {
    let restore = screen.scrollback();
    screen.set_scrollback(SCROLLBACK_LINES);
    let retained = screen.scrollback();
    screen.set_scrollback(restore);
    retained
}

/// One logical line of the visible grid, joined back across the soft wraps the
/// old width imposed on it.
struct LogicalLine {
    /// Styled bytes that redraw the line's cells at whatever width they land on.
    bytes: Vec<u8>,
    /// Printable width in columns, so a replay's height can be predicted.
    width: usize,
}

/// The visible grid as text that outlives a width change.
struct VisibleText {
    lines: Vec<LogicalLine>,
    /// Which logical line the cursor sits on, and how far into it.
    cursor: (usize, usize),
}

/// One cell's drawing attributes, compared to decide when to re-emit an SGR.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct CellStyle {
    fgcolor: vt100::Color,
    bgcolor: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fgcolor: cell.fgcolor(),
            bgcolor: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// Writes the whole style rather than a diff against the previous one. A
    /// resize is rare and the payload is thrown away immediately, so the few
    /// extra bytes buy a function with no state to get wrong.
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for (enabled, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.inverse, "7"),
        ] {
            if enabled {
                out.push(b';');
                out.extend_from_slice(code.as_bytes());
            }
        }
        out.extend_from_slice(color_params(self.fgcolor, 3).as_bytes());
        out.extend_from_slice(color_params(self.bgcolor, 4).as_bytes());
        out.push(b'm');
    }
}

/// SGR parameters for one color, as a foreground (`base` 3) or background (`base` 4).
fn color_params(color: vt100::Color, base: u8) -> String {
    match color {
        vt100::Color::Default => String::new(),
        vt100::Color::Idx(index) if index < 8 => format!(";{}", base * 10 + index),
        vt100::Color::Idx(index) if index < 16 => format!(";{}", base * 10 + 52 + index),
        vt100::Color::Idx(index) => format!(";{base}8;5;{index}"),
        vt100::Color::Rgb(red, green, blue) => format!(";{base}8;2;{red};{green};{blue}"),
    }
}

/// Whether a cell puts anything on screen, and so has to survive a reflow.
///
/// A blank cell still counts when it carries a background or a decoration: that
/// is a painted run, and trimming it would lose the paint.
fn cell_is_painted(cell: &vt100::Cell) -> bool {
    cell.has_contents()
        || cell.bgcolor() != vt100::Color::Default
        || cell.inverse()
        || cell.underline()
}

/// One past the last painted column of `row`, so trailing blanks are not replayed.
fn last_painted_column(screen: &vt100::Screen, row: u16, cols: u16) -> u16 {
    (0..cols)
        .rev()
        .find(|col| screen.cell(row, *col).is_some_and(cell_is_painted))
        .map_or(0, |col| col + 1)
}

/// Reads the visible grid as logical lines, before a resize truncates it.
///
/// `vt100::Screen::set_size` resizes every visible row with `Vec::resize`, which
/// drops the cells past the new width outright — widening the pane afterwards
/// leaves blanks where the text was. Joining the rows the old width wrapped, and
/// replaying them once the grid has its new size, is what tmux, zellij and
/// alacritty all do; this is the same idea expressed through `vt100`'s public API.
fn capture_visible_text(screen: &vt100::Screen) -> VisibleText {
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let mut lines = Vec::new();
    let mut cursor = (0, 0);
    let mut row = 0;
    while row < rows {
        let start = row;
        while row + 1 < rows && screen.row_wrapped(row) {
            row += 1;
        }
        let end = row;
        row += 1;
        if (start..=end).contains(&cursor_row) {
            cursor = (
                lines.len(),
                usize::from(cursor_row - start) * usize::from(cols) + usize::from(cursor_col),
            );
        }
        lines.push(capture_logical_line(screen, start, end, cols));
    }
    // Blank rows under the last line are the grid's own padding, not text. Replaying
    // them would scroll real content off the top the moment the text grows taller
    // than the pane, so they are dropped -- except any the cursor is parked on.
    let used = lines
        .iter()
        .rposition(|line| line.width > 0)
        .map_or(0, |last| last + 1);
    lines.truncate(used.max(cursor.0 + 1));
    VisibleText { lines, cursor }
}

fn capture_logical_line(screen: &vt100::Screen, start: u16, end: u16, cols: u16) -> LogicalLine {
    let mut bytes = Vec::new();
    let mut width = 0;
    let mut style = CellStyle::default();
    let last = last_painted_column(screen, end, cols);
    for row in start..=end {
        let limit = if row == end { last } else { cols };
        let mut col = 0;
        while col < limit {
            let Some(cell) = screen.cell(row, col) else {
                break;
            };
            // The second half of a wide character carries no contents of its own;
            // the character was already emitted with the cell that owns it.
            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }
            let cell_style = CellStyle::of(cell);
            if cell_style != style {
                cell_style.write(&mut bytes);
                style = cell_style;
            }
            match cell.contents() {
                "" => bytes.push(b' '),
                contents => bytes.extend_from_slice(contents.as_bytes()),
            }
            let cell_width = 1 + u16::from(cell.is_wide());
            width += usize::from(cell_width);
            col += cell_width;
        }
    }
    LogicalLine { bytes, width }
}

/// How many rows the replayed text occupies at `cols`, and how many of them
/// scroll off the top of a `rows`-tall grid.
fn replay_extent(text: &VisibleText, rows: u16, cols: u16) -> (usize, usize) {
    let total: usize = text.lines.iter().map(|line| line_height(line, cols)).sum();
    (total, total.saturating_sub(usize::from(rows)))
}

fn line_height(line: &LogicalLine, cols: u16) -> usize {
    line.width.div_ceil(usize::from(cols)).max(1)
}

/// Where the cursor lands once the text has been laid out again at `cols`.
fn replay_cursor(text: &VisibleText, rows: u16, cols: u16) -> (u16, u16) {
    let (_, scrolled) = replay_extent(text, rows, cols);
    let before: usize = text
        .lines
        .iter()
        .take(text.cursor.0)
        .map(|line| line_height(line, cols))
        .sum();
    let row = (before + text.cursor.1 / usize::from(cols)).saturating_sub(scrolled);
    let col = text.cursor.1 % usize::from(cols);
    (
        row.min(usize::from(rows) - 1) as u16,
        col.min(usize::from(cols) - 1) as u16,
    )
}

/// The byte stream that redraws captured text into a freshly resized grid.
///
/// `pen` is the screen's own drawing attributes, restored at the end so the
/// application's next write is styled the way it left off.
fn reflow_payload(text: &VisibleText, rows: u16, cols: u16, pen: &[u8]) -> Vec<u8> {
    // Clear with default attributes: erasing under a coloured pen would paint the
    // whole grid in that background before a single cell is replayed.
    let mut out = b"\x1b[m\x1b[H\x1b[2J".to_vec();
    for (index, line) in text.lines.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(&line.bytes);
    }
    out.extend_from_slice(b"\x1b[m");
    let (row, col) = replay_cursor(text, rows, cols);
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    out.extend_from_slice(pen);
    out
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
    fn hvp_becomes_cup_and_nothing_else_does() {
        let rewrite = |input: &[u8]| super::HvpRewriter::default().feed(input).into_owned();
        assert_eq!(rewrite(b"\x1b[5;10f"), b"\x1b[5;10H");
        assert_eq!(rewrite(b"\x1b[f"), b"\x1b[H");
        assert_eq!(rewrite(b"a\x1b[2;3fb\x1b[9;9fc"), b"a\x1b[2;3Hb\x1b[9;9Hc");

        // An `f` is an ordinary character everywhere else, and rewriting one
        // would corrupt the very thing it appears in.
        assert_eq!(rewrite(b"fluff"), b"fluff");
        assert_eq!(rewrite(b"\x1b]0;my file.rs\x07"), b"\x1b]0;my file.rs\x07");
        assert_eq!(
            rewrite(b"\x1b]2;refactor\x1b\\f"),
            b"\x1b]2;refactor\x1b\\f"
        );
        assert_eq!(rewrite(b"\x1bPtmux;f\x1b\\"), b"\x1bPtmux;f\x1b\\");
        // Not a final byte: `f` after an intermediate is still the final byte,
        // but a parameter `f` cannot occur, and other finals are untouched.
        assert_eq!(
            rewrite(b"\x1b[1;2H\x1b[K\x1b[?25l"),
            b"\x1b[1;2H\x1b[K\x1b[?25l"
        );
    }

    /// A read boundary lands wherever the kernel put it, including the middle of
    /// the escape sequence being repaired.
    #[test]
    fn hvp_is_rewritten_across_a_split_read() {
        let mut rewriter = super::HvpRewriter::default();
        let mut out = rewriter.feed(b"text\x1b[12;").into_owned();
        out.extend_from_slice(&rewriter.feed(b"34f more"));
        assert_eq!(out, b"text\x1b[12;34H more");

        // And the same for a string body, whose `f` must survive the split.
        let mut rewriter = super::HvpRewriter::default();
        let mut out = rewriter.feed(b"\x1b]0;fi").into_owned();
        out.extend_from_slice(&rewriter.feed(b"le.rs\x07\x1b[1;1f"));
        assert_eq!(out, b"\x1b]0;file.rs\x07\x1b[1;1H");
    }

    /// The whole point: `f` used to move the cursor nowhere.
    #[test]
    fn a_pane_positions_the_cursor_with_hvp() {
        let mut screen = HostScreen::new(24, 80).expect("valid dimensions");
        screen.process_pty(b"\x1b[5;10fhere").expect("processed");
        assert_eq!(screen.screen().cursor_position(), (4, 13));
        assert!(screen.screen().contents().contains("here"));
    }

    /// A terminal query is a blocking round trip, so silence is not a degraded
    /// answer — it is a stopped program. `gh secret set` writes `\x1b[6n` and
    /// reads until it is answered; unanswered, it never prints its own prompt.
    #[test]
    fn a_cursor_position_query_is_answered_where_the_cursor_is() {
        let mut screen = HostScreen::new(24, 80).expect("valid dimensions");
        assert!(screen.take_query_replies().is_empty(), "nothing asked yet");

        screen
            .process_pty(b"\x1b[3;10Hasking\x1b[6n")
            .expect("processed");
        // One-based, and past the six characters just written.
        assert_eq!(screen.take_query_replies(), b"\x1b[3;16R");
        assert!(
            screen.take_query_replies().is_empty(),
            "an answer is handed over once"
        );

        screen.process_pty(b"\x1b[?6n").expect("processed");
        assert_eq!(screen.take_query_replies(), b"\x1b[?3;16;1R");
    }

    /// How programs measure a terminal: park the cursor far past the end and ask
    /// where it actually landed. The answer has to be the pane's real grid, which
    /// it is for free — the clamp happens while parsing, before the cursor is read.
    #[test]
    fn the_cursor_parking_size_probe_reports_the_panes_real_grid() {
        let mut screen = HostScreen::new(24, 80).expect("valid dimensions");
        screen
            .process_pty(b"\x1b7\x1b[999;999f\x1b[6n\x1b8")
            .expect("processed");
        assert_eq!(screen.take_query_replies(), b"\x1b[24;80R");

        let mut small = HostScreen::new(9, 40).expect("valid dimensions");
        small
            .process_pty(b"\x1b7\x1b[999;999f\x1b[6n\x1b8")
            .expect("processed");
        assert_eq!(small.take_query_replies(), b"\x1b[9;40R");
    }

    #[test]
    fn the_other_answerable_queries_are_answered_too() {
        let mut screen = HostScreen::new(12, 40).expect("valid dimensions");
        // Are you there? Reaching the parser at all is the proof.
        screen.process_pty(b"\x1b[5n").expect("processed");
        assert_eq!(screen.take_query_replies(), b"\x1b[0n");
        // Text area in characters.
        screen.process_pty(b"\x1b[18t").expect("processed");
        assert_eq!(screen.take_query_replies(), b"\x1b[8;12;40t");
        // Device attributes, primary and secondary.
        screen.process_pty(b"\x1b[c").expect("processed");
        assert_eq!(screen.take_query_replies(), b"\x1b[?1;2c");
        screen.process_pty(b"\x1b[>c").expect("processed");
        assert_eq!(screen.take_query_replies(), b"\x1b[>0;10;0c");
    }

    /// Deliberate silence, not an oversight. This process cannot see the terminal
    /// the human is looking at, and for a pane hosted on another member's machine
    /// there is no single such terminal — so a colour here would be invented, and
    /// an application would pick its palette against the wrong background.
    #[test]
    fn colour_queries_are_left_unanswered_rather_than_guessed() {
        let mut screen = HostScreen::new(24, 80).expect("valid dimensions");
        screen
            .process_pty(b"\x1b]11;?\x1b\\\x1b]10;?\x1b\\")
            .expect("processed");
        assert!(screen.take_query_replies().is_empty());
    }

    /// A program asking faster than its pane is drained must lose its answers,
    /// not the node's memory.
    #[test]
    fn a_flood_of_queries_is_bounded() {
        let mut screen = HostScreen::new(24, 80).expect("valid dimensions");
        screen
            .process_pty(&b"\x1b[6n".repeat(10_000))
            .expect("processed");
        let replies = screen.take_query_replies();
        assert!(!replies.is_empty(), "the early answers still went out");
        assert!(replies.len() <= 4096 + 16, "unbounded: {}", replies.len());
    }

    #[test]
    fn bell_count_ignores_osc_terminators_and_counts_standalone_bells() {
        let mut screen = HostScreen::new(24, 80).expect("valid dimensions");
        assert_eq!(screen.take_bell_count(), 0);

        // Agents set the window title constantly, and OSC strings are BEL-terminated. A raw
        // scan for 0x07 would report each of these as a completion.
        screen
            .process_pty(b"\x1b]0;a title\x07\x1b]2;another\x07")
            .expect("processed");
        assert_eq!(screen.take_bell_count(), 0);

        screen.process_pty(b"done\x07").expect("processed");
        assert_eq!(screen.take_bell_count(), 1);
        // Taking the count clears it, so one bell is reported once.
        assert_eq!(screen.take_bell_count(), 0);

        screen.process_pty(b"\x07\x07").expect("processed");
        assert_eq!(screen.take_bell_count(), 2);
    }

    /// Output shaped like a busy agent pane: colour, cursor moves, redraws, and an
    /// alternate-screen excursion. `lines` past `SCROLLBACK_LINES` pushes retention to its cap.
    fn agent_like_batches(lines: usize) -> Vec<Vec<u8>> {
        let mut batches = Vec::new();
        for i in 0..lines {
            batches.push(
                format!("line {i} \x1b[32mgreen\x1b[0m \x1b[1mbold\x1b[0m padding text\r\n")
                    .into_bytes(),
            );
            if i % 500 == 0 {
                batches.push(b"\x1b[H\x1b[2J\x1b[3;10Hredrawn header\r\n".to_vec());
            }
            if i == 6_000 {
                batches.push(b"\x1b[?1049h\x1b[2Jalternate screen\r\n".to_vec());
                batches.push(b"more alt\r\n".to_vec());
                batches.push(b"\x1b[?1049l".to_vec());
            }
        }
        batches
    }

    #[test]
    /// Locks `retained_scrollback` to the clone-and-clamp reading the wheel handler used
    /// to do, across the two paths that touch the parser: output and resize.
    fn retained_scrollback_matches_a_clone_and_clamp_reading() {
        let clone_and_clamp = |screen: &vt100::Screen| {
            let mut screen = screen.clone();
            screen.set_scrollback(super::SCROLLBACK_LINES);
            screen.scrollback()
        };
        let mut host = HostScreen::new(24, 80).expect("valid dimensions");
        assert_eq!(host.retained_scrollback(), 0);
        for (index, batch) in agent_like_batches(400).into_iter().enumerate() {
            host.process_pty(&batch).expect("processed");
            assert_eq!(
                host.retained_scrollback(),
                clone_and_clamp(host.screen()),
                "retained rows diverged at batch {index}",
            );
        }
        assert!(host.retained_scrollback() > 0, "expected retained history");
        host.resize(30, 100).expect("valid dimensions");
        assert_eq!(host.retained_scrollback(), clone_and_clamp(host.screen()));
    }

    /// The visible grid read back with its soft wraps joined away.
    fn unwrapped(host: &HostScreen) -> String {
        host.screen().contents().replace('\n', "")
    }

    #[test]
    /// A pane that narrows and widens again has to come back with its text.
    ///
    /// `vt100::Screen::set_size` resizes each visible row with `Vec::resize`, so before
    /// the reflow the shrink dropped every cell past the new width and the widening
    /// filled the gap with blanks -- the line came back as its first 40 characters.
    fn a_narrower_pane_rewraps_its_text_instead_of_losing_it() {
        let line = "abcdefghij".repeat(12);
        let mut host = HostScreen::new(24, 100).expect("valid dimensions");
        host.process_pty(line.as_bytes()).expect("processed");
        assert!(unwrapped(&host).contains(&line));

        host.resize(24, 40).expect("valid dimensions");
        assert!(unwrapped(&host).contains(&line), "the shrink lost text");

        host.resize(24, 100).expect("valid dimensions");
        assert!(unwrapped(&host).contains(&line), "the widening lost text");
    }

    #[test]
    /// Text taller than the pane after a shrink belongs in history, not in the bin.
    fn a_shrink_that_outgrows_the_pane_scrolls_the_overflow_into_scrollback() {
        let mut host = HostScreen::new(4, 60).expect("valid dimensions");
        for index in 0..4 {
            host.process_pty(format!("line{index} {}\r\n", "x".repeat(50)).as_bytes())
                .expect("processed");
        }
        let (_, before_end) = host.history_metadata();
        let before_retained = host.retained_scrollback();

        // At 20 columns each of those lines needs three rows, so most of them cannot
        // fit a four-row pane and have to move up into history rather than vanish.
        host.resize(4, 20).expect("valid dimensions");
        let (total_rows, end) = host.history_metadata();
        assert!(end > before_end, "the scrolled rows never reached history");
        assert!(total_rows > 0, "expected retained history after the reflow");
        assert_eq!(
            host.retained_scrollback(),
            before_retained + (end - before_end) as usize,
            "history and its monotonic end disagree about the reflow",
        );
    }

    #[test]
    fn a_reflow_keeps_the_colours_and_puts_the_cursor_back_on_its_character() {
        let mut host = HostScreen::new(10, 20).expect("valid dimensions");
        // 37 printable characters: two rows at 20 columns, one at 40.
        host.process_pty(b"\x1b[31mred\x1b[m plain and then a much longer tail")
            .expect("processed");
        assert_eq!(host.screen().cursor_position(), (1, 17));

        host.resize(10, 40).expect("valid dimensions");
        assert_eq!(host.screen().cursor_position(), (0, 37));
        assert_eq!(
            host.screen().cell(0, 0).map(vt100::Cell::fgcolor),
            Some(vt100::Color::Idx(1)),
            "the reflow dropped the styling it replayed",
        );
        assert!(unwrapped(&host).contains("red plain and then a much longer tail"));
    }

    #[test]
    /// An application in the alternate screen owns the whole frame and repaints it
    /// when it sees the new size, so the columns a shrink takes are its to redraw.
    /// Replaying the old frame instead would only fight that redraw.
    fn the_alternate_screen_is_left_for_its_application_to_repaint() {
        let mut host = HostScreen::new(10, 20).expect("valid dimensions");
        host.process_pty(b"\x1b[?1049h").expect("processed");
        host.process_pty(b"a full width row of text")
            .expect("processed");

        host.resize(10, 10).expect("valid dimensions");
        assert!(
            !unwrapped(&host).contains("a full width row of text"),
            "the alternate screen was reflowed instead of left alone",
        );
    }

    #[test]
    fn a_resize_that_only_changes_rows_leaves_the_text_where_it_was() {
        let mut host = HostScreen::new(10, 20).expect("valid dimensions");
        host.process_pty(b"first\r\nsecond").expect("processed");
        let drawn = host.screen().contents();
        let cursor = host.screen().cursor_position();

        host.resize(20, 20).expect("valid dimensions");
        assert_eq!(host.screen().contents(), drawn);
        assert_eq!(host.screen().cursor_position(), cursor);
    }

    #[test]
    fn scrollback_free_baseline_produces_the_same_deltas_as_a_cloned_screen() {
        let mut host = HostScreen::new(24, 80).expect("valid dimensions");
        // The baseline this replaces: a full clone of the live screen after every batch.
        let mut reference = vt100::Parser::new(24, 80, super::SCROLLBACK_LINES);
        let mut reference_previous = reference.screen().clone();

        // Deliberately short: the reference clones the whole screen per batch, which is the
        // cost this change exists to remove. Scrollback-depth coverage lives in the guest
        // round-trip test below, which needs no clones.
        for (index, batch) in agent_like_batches(600).into_iter().enumerate() {
            let frame = host.process_pty(&batch).expect("processed");
            reference.process(&batch);
            let expected = reference.screen().state_diff(&reference_previous);
            reference_previous = reference.screen().clone();
            assert_eq!(
                frame.delta.as_ref(),
                expected.as_slice(),
                "delta diverged from the cloned baseline at batch {index}",
            );
        }
    }

    #[test]
    fn a_guest_following_deltas_across_a_resize_matches_the_host() {
        let mut host = HostScreen::new(24, 80).expect("valid dimensions");
        let mut guest = super::GuestScreen::new();
        let first = host.process_pty(b"first line\r\n").expect("processed");
        guest
            .apply_snapshot(first.sequence, &first.snapshot)
            .expect("snapshot applies");

        // Past SCROLLBACK_LINES so retention sits at its cap for most of the run.
        let mut resized = false;
        for batch in agent_like_batches(12_100) {
            let frame = host.process_pty(&batch).expect("processed");
            follow(&mut guest, &frame);
            if !resized {
                // A resize reflows the host against its retained rows, which the baseline
                // does not keep; the deltas after it still have to line up.
                let frame = host.resize(40, 100).expect("valid dimensions");
                follow(&mut guest, &frame);
                resized = true;
            }
        }

        assert!(resized);
        assert_eq!(
            guest.screen().expect("guest screen").contents(),
            host.screen().contents(),
        );
    }

    fn follow(guest: &mut super::GuestScreen, frame: &super::ScreenFrame) {
        let applied = if frame.base_sequence == 0 {
            super::ApplyDelta::NeedsSnapshot
        } else {
            guest
                .apply_delta(frame.base_sequence, frame.sequence, &frame.delta)
                .expect("delta applies")
        };
        if applied == super::ApplyDelta::NeedsSnapshot {
            guest
                .apply_snapshot(frame.sequence, &frame.snapshot)
                .expect("snapshot applies");
        }
    }

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
    fn visual_scrollback_is_bounded_and_survives_a_resize() {
        let mut screen = HostScreen::new(1, 3).unwrap();
        screen.process_pty(b"a\r\nb\r\nc").unwrap();
        let (total, rows) = screen.visual_scrollback(1, 1024);
        assert!(total >= 1);
        assert_eq!(rows.len(), 1);

        // A resize used to raise the history floor to the live edge, which left a
        // pane that stopped scrolling -- an agent UI repainting its own frame --
        // with no reachable history at all.
        screen.resize(2, 3).unwrap();
        let (after, rows) = screen.visual_scrollback(10, 1024);
        assert_eq!(after, total);
        assert_eq!(rows.len(), total);

        screen.process_pty(b"\x1b[?1049h").unwrap();
        assert_eq!(screen.visual_scrollback(10, 1024), (0, vec![]));
    }

    /// The rows themselves have to come back, not just a non-zero count: the
    /// window maps absolute positions onto the retained deque, and a resize is
    /// exactly where that mapping used to be abandoned.
    #[test]
    fn history_rows_read_the_same_after_the_pane_is_resized() {
        let mut screen = HostScreen::new(2, 12).unwrap();
        for line in 0..8 {
            screen
                .process_pty(format!("row {line}\r\n").as_bytes())
                .unwrap();
        }
        let (before_total, before) = screen.visual_scrollback(4, 4096);
        assert!(before_total >= 4);

        screen.resize(6, 12).unwrap();
        let (after_total, after) = screen.visual_scrollback(4, 4096);
        assert_eq!(after_total, before_total);
        assert_eq!(after, before);
    }

    /// Retained rows keep the width they were written at, so a narrower pane reads
    /// them through a shorter window than they hold. That has to clip, not panic,
    /// and it has to leave the rows themselves reachable.
    #[test]
    fn history_survives_a_pane_that_gets_narrower() {
        let mut screen = HostScreen::new(2, 24).unwrap();
        for line in 0..8 {
            screen
                .process_pty(format!("{line} wide row of text\r\n").as_bytes())
                .unwrap();
        }
        let (before_total, _) = screen.visual_scrollback(4, 4096);

        screen.resize(2, 10).unwrap();
        let (after_total, after) = screen.visual_scrollback(4, 4096);
        // Not equal: rewrapping the visible rows at ten columns makes them taller than
        // the pane, and the rows pushed past the top join history like any other scroll.
        assert!(after_total >= before_total);
        assert_eq!(after.len(), 4);
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
