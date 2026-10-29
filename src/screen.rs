//! Fixed-grid vt100 state codec used only by the host and guest renderers.

use std::{
    collections::VecDeque,
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
/// How many rows the parser itself is allowed to keep.
///
/// A vt100 row is a cell for every column, about 32 bytes each, whether or not
/// the line had anything on it. Ten thousand of those is tens of megabytes per
/// pane, and the allocator does not hand that back when the pane closes.
/// The parser only needs a short run of rows to notice what just scrolled off;
/// everything older is kept below as the text of the line.
const PARSER_SCROLLBACK: usize = 64;
const SNAPSHOT_HEADER_BYTES: usize = 5;

#[derive(Clone, Debug)]
pub struct ScreenFrame {
    pub sequence: u64,
    pub base_sequence: u64,
    pub snapshot: Arc<[u8]>,
    pub delta: Arc<[u8]>,
    pub kitty_keyboard_active: bool,
    /// The terminal outside ratatui was cleared or an alternate screen ended.
    pub reset_outer: bool,
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

/// Notices the terminal operations that invalidate ratatui's cached outer grid.
///
/// This shares the VT state boundaries with [`HvpRewriter`] instead of looking
/// for bytes: `2J` in a title is text, not an erase, and a CSI can cross reads.
#[derive(Debug, Default)]
pub(crate) struct OuterResetRecognizer {
    state: VtState,
    params: Vec<u8>,
}

impl OuterResetRecognizer {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut reset = false;
        for &byte in bytes {
            self.state = match self.state {
                VtState::Ground if byte == 0x1b => VtState::Escape,
                VtState::Ground => VtState::Ground,
                VtState::Escape => {
                    let state = HvpRewriter::after_escape(byte);
                    if state == VtState::Csi {
                        self.params.clear();
                    }
                    state
                }
                VtState::Csi if (0x40..=0x7e).contains(&byte) => {
                    if byte == b'J' && csi_erases_whole_screen(&self.params) {
                        reset = true;
                    }
                    self.params.clear();
                    VtState::Ground
                }
                VtState::Csi => {
                    self.params.push(byte);
                    VtState::Csi
                }
                VtState::StringBody => match byte {
                    0x07 => VtState::Ground,
                    0x1b => VtState::StringEscape,
                    _ => VtState::StringBody,
                },
                VtState::StringEscape if byte == b'\\' => VtState::Ground,
                VtState::StringEscape => {
                    let state = HvpRewriter::after_escape(byte);
                    if state == VtState::Csi {
                        self.params.clear();
                    }
                    state
                }
            };
        }
        reset
    }
}

fn csi_erases_whole_screen(params: &[u8]) -> bool {
    params
        .split(|byte| *byte == b';')
        .filter_map(|param| std::str::from_utf8(param).ok()?.parse::<u8>().ok())
        .any(|param| matches!(param, 2 | 3))
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

/// One row that has scrolled off the live screen, stored as the bytes that
/// redraw it rather than as a cell for every column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryLine {
    bytes: Vec<u8>,
    wrapped: bool,
}

pub struct HostScreen {
    parser: vt100::Parser<PaneCallbacks>,
    /// Lines that have left the parser's own short scrollback, oldest first.
    history: VecDeque<HistoryLine>,
    /// The parser's current scrollback, oldest first. A line moves into
    /// `history` at the moment the parser is about to drop it.
    staged: Vec<HistoryLine>,
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
    outer_reset: OuterResetRecognizer,
}

impl HostScreen {
    pub fn new(rows: u16, cols: u16) -> Result<Self, ScreenError> {
        validate_dimensions(rows, cols)?;
        let parser = vt100::Parser::new_with_callbacks(
            rows,
            cols,
            PARSER_SCROLLBACK,
            PaneCallbacks::default(),
        );
        let snapshot = snapshot_payload(parser.screen())?;
        Ok(Self {
            parser,
            history: VecDeque::new(),
            staged: Vec::new(),
            previous: vt100::Parser::new(rows, cols, 0),
            current: ScreenFrame {
                sequence: 1,
                base_sequence: 0,
                snapshot,
                delta: Arc::from([]),
                kitty_keyboard_active: false,
                reset_outer: false,
            },
            kitty_keyboard: KittyKeyboardTracker::default(),
            history_end: 0,
            retained_scrollback: 0,
            hvp: HvpRewriter::default(),
            outer_reset: OuterResetRecognizer::default(),
        })
    }

    pub fn process_pty(&mut self, bytes: &[u8]) -> Result<ScreenFrame, ScreenError> {
        let before_history = self.retained_scrollback;
        let was_alternate_screen = self.parser.screen().alternate_screen();
        let erased_outer = self.outer_reset.feed(bytes);
        // Both parsers see the same rewritten bytes, or the delta baseline would
        // drift from the live screen by exactly the cursor moves being repaired.
        let bytes = &*self.hvp.feed(bytes);
        self.kitty_keyboard.observe(bytes);
        self.ingest(bytes);
        let after_history = self.retained_scrollback;
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
            reset_outer: erased_outer
                || (was_alternate_screen && !self.parser.screen().alternate_screen()),
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
            self.ingest(&reflow_payload(&text, rows, cols, &pen));
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
        // rebuild what was thrown away. `ingest` already counted whatever the
        // reflow pushed off the top.
        let frame = ScreenFrame {
            sequence,
            base_sequence: 0,
            snapshot: snapshot_payload(self.parser.screen())?,
            delta: Arc::from([]),
            kitty_keyboard_active: self.current.kitty_keyboard_active,
            reset_outer: false,
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
        let (total_rows, _) = self.history_metadata();
        if total_rows == 0 {
            return (0, Vec::new());
        }
        let offset = offset.min(total_rows);
        let last = total_rows.saturating_sub(offset);
        let first = last.saturating_sub(max_rows as u64);
        let mut rows = Vec::new();
        let mut bytes = 0_usize;
        for line in self
            .history
            .iter()
            .chain(self.staged.iter())
            .skip(first as usize)
            .take((last - first) as usize)
        {
            if bytes.saturating_add(line.bytes.len()) > max_bytes {
                break;
            }
            bytes = bytes.saturating_add(line.bytes.len());
            rows.push(line.bytes.clone());
        }
        (total_rows, rows)
    }

    /// The screen as it looks `offset` rows back, including lines the parser
    /// itself has already forgotten.
    ///
    /// Offsets the parser still holds are a `set_scrollback` on a clone, which
    /// is exactly the view selection and the wheel used to take. Anything
    /// older is redrawn from the stored lines: the parser no longer has them.
    pub(crate) fn view_at(&self, offset: usize) -> vt100::Screen {
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(offset);
        if offset == 0 || screen.scrollback() == offset {
            return screen;
        }
        let (rows, cols) = self.parser.screen().size();
        let scrollback = self.history.len() + self.staged.len();
        let mut lines: Vec<&HistoryLine> = Vec::with_capacity(scrollback + usize::from(rows));
        lines.extend(self.history.iter());
        lines.extend(self.staged.iter());
        let visible = self.visible_lines();
        lines.extend(visible.iter());
        let start = scrollback.saturating_sub(offset);
        let end = (start + usize::from(rows)).min(lines.len());
        let mut parser = vt100::Parser::new(rows, cols, 0);
        for (index, line) in lines[start..end].iter().enumerate() {
            let cup = format!("\x1b[{};1H", index + 1);
            parser.process(cup.as_bytes());
            parser.process(&line.bytes);
        }
        parser.screen().clone()
    }

    fn visible_lines(&self) -> Vec<HistoryLine> {
        let screen = self.parser.screen();
        let cols = screen.size().1;
        screen
            .rows_formatted(0, cols)
            .enumerate()
            .map(|(index, bytes)| HistoryLine {
                bytes,
                wrapped: screen.row_wrapped(index as u16),
            })
            .collect()
    }

    /// Scrollback plus the live rows, oldest first. What a frozen browse
    /// session scrolls through once the parser has let the older lines go.
    pub(crate) fn frozen_lines(&self) -> Vec<HistoryLine> {
        let mut lines = Vec::with_capacity(self.history.len() + self.staged.len());
        lines.extend(self.history.iter().cloned());
        lines.extend(self.staged.iter().cloned());
        lines.extend(self.visible_lines());
        lines
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

/// Feed `bytes` to the parser and remember every line it scrolls off.
///
/// Split into short pieces first. The parser only keeps [`PARSER_SCROLLBACK`]
/// rows, so a single batch longer than that would scroll lines off that this
/// had never stored. An alternate screen has no history of its own; its bytes
/// are parsed and forgotten, and the primary history is left where it was.
fn ingest(host: &mut HostScreen, bytes: &[u8]) {
    if host.parser.screen().alternate_screen() {
        host.parser.screen_mut().set_scrollback(0);
        host.parser.process(bytes);
        return;
    }
    let cols = usize::from(host.parser.screen().size().1).max(1);
    // One line at a time once a newline shows up. A longer piece can scroll
    // more rows off the parser's short buffer than this has stored, and one
    // newline can scroll at most one row.
    let max_newlines = 1;
    let max_bytes = (PARSER_SCROLLBACK / 2).max(1) * cols;
    let mut rest = bytes;
    while !rest.is_empty() {
        let mut end = rest.len();
        let mut newlines = 0_usize;
        for (index, byte) in rest.iter().enumerate() {
            if *byte == b'\n' {
                newlines += 1;
                if newlines >= max_newlines {
                    end = index + 1;
                    break;
                }
            }
            if index + 1 >= max_bytes {
                end = index + 1;
                break;
            }
        }
        let chunk = &rest[..end];
        rest = &rest[end..];
        host.parser.screen_mut().set_scrollback(0);
        host.parser.process(chunk);
        if host.parser.screen().alternate_screen() {
            // The offset now refers to the other grid. Syncing here would
            // read that grid and throw away the primary history.
            if !rest.is_empty() {
                host.parser.process(rest);
            }
            return;
        }
        sync_history(host, newlines);
    }
}

impl HostScreen {
    fn ingest(&mut self, bytes: &[u8]) {
        ingest(self, bytes);
    }
}

fn sync_history(host: &mut HostScreen, newlines: usize) {
    let depth = scrollback_depth(host);
    let before = host.staged.len();
    if depth < PARSER_SCROLLBACK {
        if depth < before {
            host.staged = read_newest(host, depth);
        } else if depth > before {
            let added = read_newest(host, depth - before);
            host.staged.extend(added);
        }
    } else if before < PARSER_SCROLLBACK {
        // This chunk is the one that filled the parser's buffer. Re-read it
        // and keep whatever prefix of the old mirror is no longer there.
        let now = read_newest(host, depth);
        remember_dropped(host, &now);
        host.staged = now;
    } else if newlines > 0 {
        let newest = read_newest(host, newlines.min(depth));
        let unchanged = host.staged.len() >= newest.len()
            && host.staged[host.staged.len() - newest.len()..]
                .iter()
                .zip(&newest)
                .all(|(left, right)| left == right);
        if !unchanged {
            let dropped = newlines.min(host.staged.len());
            for line in host.staged.drain(..dropped) {
                host.history.push_back(line);
            }
            host.staged.extend(newest);
        }
    } else {
        let now = read_newest(host, depth);
        if lines_dropped(&host.staged, &now) > 0 {
            remember_dropped(host, &now);
            host.staged = now;
        }
    }
    if host.staged.len() > depth {
        let extra = host.staged.len() - depth;
        for line in host.staged.drain(..extra) {
            host.history.push_back(line);
        }
    }
    while host.history.len() + host.staged.len() > SCROLLBACK_LINES {
        host.history.pop_front();
    }
    host.retained_scrollback = host.history.len() + host.staged.len();
}

fn remember_dropped(host: &mut HostScreen, now: &[HistoryLine]) {
    let dropped = lines_dropped(&host.staged, now).min(host.staged.len());
    for line in host.staged.drain(..dropped) {
        host.history.push_back(line);
    }
}

/// How many leading `old` lines are gone from `new`.
///
/// The parser drops from the front and appends at the back, so the longest
/// suffix of `old` that is a prefix of `new` is the part still retained.
fn lines_dropped(old: &[HistoryLine], new: &[HistoryLine]) -> usize {
    if old.is_empty() {
        return 0;
    }
    let max = old.len().min(new.len());
    for overlap in (0..=max).rev() {
        if old[old.len() - overlap..]
            .iter()
            .zip(&new[..overlap])
            .all(|(left, right)| left == right)
        {
            return old.len() - overlap;
        }
    }
    old.len()
}

fn scrollback_depth(host: &mut HostScreen) -> usize {
    let screen = host.parser.screen_mut();
    let restore = screen.scrollback();
    screen.set_scrollback(PARSER_SCROLLBACK);
    let depth = screen.scrollback();
    screen.set_scrollback(restore);
    depth
}

/// The newest `count` scrollback rows, oldest first.
fn read_newest(host: &mut HostScreen, count: usize) -> Vec<HistoryLine> {
    if count == 0 {
        return Vec::new();
    }
    let screen = host.parser.screen_mut();
    let cols = screen.size().1;
    let mut lines = Vec::with_capacity(count);
    for offset in (1..=count).rev() {
        screen.set_scrollback(offset);
        let bytes = screen.rows_formatted(0, cols).next().unwrap_or_default();
        let wrapped = screen.row_wrapped(0);
        lines.push(HistoryLine { bytes, wrapped });
    }
    screen.set_scrollback(0);
    lines
}

/// A viewport of stored lines, for a browse offset the live parser no longer reaches.
///
/// `lines` is scrollback and then the live rows, oldest first. `offset` skips
/// that many rows back from the live edge, the same count `set_scrollback` takes.
pub(crate) fn snapshot_history_viewport(
    lines: &[HistoryLine],
    scrollback_rows: u64,
    rows: u16,
    cols: u16,
    offset: u64,
) -> Vec<u8> {
    let rows = rows.max(1);
    let cols = cols.max(1);
    let scrollback = scrollback_rows.min(lines.len() as u64) as usize;
    let start = scrollback.saturating_sub(offset as usize);
    let end = (start + usize::from(rows)).min(lines.len());
    let mut parser = vt100::Parser::new(rows, cols, 0);
    for (index, line) in lines[start..end].iter().enumerate() {
        let cup = format!("\x1b[{};1H", index + 1);
        parser.process(cup.as_bytes());
        parser.process(&line.bytes);
    }
    snapshot_payload(parser.screen())
        .map(|snapshot| snapshot.as_ref().to_vec())
        .unwrap_or_default()
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
    underline_color: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: vt100::Underline,
    inverse: bool,
    strikethrough: bool,
    blink: bool,
    conceal: bool,
    overline: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fgcolor: cell.fgcolor(),
            bgcolor: cell.bgcolor(),
            underline_color: cell.underline_color(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline_style(),
            inverse: cell.inverse(),
            strikethrough: cell.strikethrough(),
            blink: cell.blink(),
            conceal: cell.conceal(),
            overline: cell.overline(),
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
            (self.inverse, "7"),
            (self.blink, "5"),
            (self.conceal, "8"),
            (self.strikethrough, "9"),
            (self.overline, "53"),
        ] {
            if enabled {
                out.push(b';');
                out.extend_from_slice(code.as_bytes());
            }
        }
        let underline = match self.underline {
            vt100::Underline::None => None,
            vt100::Underline::Single => Some("4"),
            vt100::Underline::Double => Some("21"),
            vt100::Underline::Curly => Some("4:3"),
            vt100::Underline::Dotted => Some("4:4"),
            vt100::Underline::Dashed => Some("4:5"),
        };
        if let Some(code) = underline {
            out.push(b';');
            out.extend_from_slice(code.as_bytes());
        }
        out.extend_from_slice(color_params(self.fgcolor, 3).as_bytes());
        out.extend_from_slice(color_params(self.bgcolor, 4).as_bytes());
        out.extend_from_slice(underline_color_params(self.underline_color).as_bytes());
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

/// SGR 58 parameters. Empty when the underline keeps the terminal's colour.
fn underline_color_params(color: vt100::Color) -> String {
    match color {
        vt100::Color::Default => String::new(),
        vt100::Color::Idx(index) => format!(";58;5;{index}"),
        vt100::Color::Rgb(red, green, blue) => format!(";58;2;{red};{green};{blue}"),
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
        || cell.strikethrough()
        || cell.overline()
        || cell.blink()
        || cell.conceal()
        || cell.underline_color() != vt100::Color::Default
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
    /// The count a scroll can reach is the count `retained_scrollback` reports,
    /// including lines the parser itself has already dropped.
    fn retained_scrollback_matches_what_a_scroll_can_reach() {
        let mut host = HostScreen::new(24, 80).expect("valid dimensions");
        assert_eq!(host.retained_scrollback(), 0);
        for (index, batch) in agent_like_batches(400).into_iter().enumerate() {
            host.process_pty(&batch).expect("processed");
            let (total, _) = host.visual_scrollback(1, 64);
            assert_eq!(
                host.retained_scrollback(),
                total,
                "retained rows diverged at batch {index}",
            );
        }
        assert!(
            host.retained_scrollback() > super::PARSER_SCROLLBACK,
            "expected history past the parser's own buffer, got {}",
            host.retained_scrollback()
        );
        let before = host.retained_scrollback();
        host.resize(30, 100).expect("valid dimensions");
        let (total, _) = host.visual_scrollback(1, 64);
        assert_eq!(host.retained_scrollback(), total);
        assert!(host.retained_scrollback() >= before);
    }

    #[test]
    fn lines_older_than_the_parser_buffer_stay_readable() {
        let mut host = HostScreen::new(2, 40).expect("valid dimensions");
        for index in 0..300 {
            host.process_pty(format!("line-{index}\r\n").as_bytes())
                .expect("processed");
        }
        let (total, _) = host.history_metadata();
        assert!(total > super::PARSER_SCROLLBACK as u64);
        let (_, oldest) = host.visual_scrollback_window(total.saturating_sub(1), 1, 4096);
        let text = String::from_utf8_lossy(oldest.first().expect("oldest line"));
        assert!(
            text.contains("line-0"),
            "the first line was dropped with the parser buffer: {text}"
        );
        let deep = host.view_at(total as usize);
        assert!(
            deep.contents().contains("line-0"),
            "scrolling back to it lost the text: {}",
            deep.contents()
        );
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
    fn leaving_the_alternate_screen_resets_the_outer_terminal() {
        let mut host = HostScreen::new(2, 20).expect("valid dimensions");
        host.process_pty(b"\x1b[?1049hClaude")
            .expect("entered alternate");

        assert!(
            host.process_pty(b"\x1b[?1049l")
                .expect("left alternate")
                .reset_outer
        );
    }

    #[test]
    fn full_screen_erases_reset_the_outer_terminal() {
        let mut host = HostScreen::new(2, 20).expect("valid dimensions");

        assert!(host.process_pty(b"\x1b[2J").unwrap().reset_outer);
        assert!(host.process_pty(b"\x1b[3J").unwrap().reset_outer);
        assert!(!host.process_pty(b"\x1b[0J").unwrap().reset_outer);
        assert!(!host.process_pty(b"\x1b[J").unwrap().reset_outer);
    }

    #[test]
    fn outer_reset_recognizer_ignores_plain_and_string_text() {
        let mut host = HostScreen::new(2, 20).expect("valid dimensions");

        assert!(!host.process_pty(b"hello\r\n").unwrap().reset_outer);
        assert!(!host.process_pty(b"\x1b]0;foo2J\x07").unwrap().reset_outer);
    }

    #[test]
    fn split_full_screen_erase_resets_the_outer_terminal() {
        let mut host = HostScreen::new(2, 20).expect("valid dimensions");
        assert!(!host.process_pty(b"\x1b[2").unwrap().reset_outer);
        assert!(host.process_pty(b"J").unwrap().reset_outer);
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

    #[test]
    fn extended_sgr_round_trips_through_a_formatted_screen() {
        let bytes = b"\x1b[9mS\x1b[0m \x1b[4:3mC\x1b[0m \x1b[21mD\x1b[0m \x1b[58;2;255;0;0;4mU\x1b[0m \x1b[5mB\x1b[0m \x1b[8mH\x1b[0m \x1b[53mO";
        let mut parser = vt100::Parser::new(2, 40, 0);
        parser.process(bytes);
        let mut replay = vt100::Parser::new(2, 40, 0);
        replay.process(&parser.screen().contents_formatted());
        let cell = |column: u16| replay.screen().cell(0, column).expect("cell");
        assert!(cell(0).strikethrough());
        assert_eq!(cell(2).underline_style(), vt100::Underline::Curly);
        assert_eq!(cell(4).underline_style(), vt100::Underline::Double);
        assert_eq!(cell(6).underline_color(), vt100::Color::Rgb(255, 0, 0));
        assert!(cell(6).underline());
        assert!(cell(8).blink());
        assert!(cell(10).conceal());
        assert!(cell(12).overline());
        assert_eq!(cell(0).contents(), "S");
        assert_eq!(cell(10).contents(), "H");
    }

    #[test]
    fn focus_event_mode_is_part_of_the_snapshot() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        assert!(!parser.screen().focus_event_mode());
        parser.process(b"\x1b[?1004h");
        assert!(parser.screen().focus_event_mode());
        let state = parser.screen().state_formatted();
        assert!(state.windows(8).any(|window| window == b"\x1b[?1004h"));
        let mut replay = vt100::Parser::new(2, 8, 0);
        replay.process(&state);
        assert!(replay.screen().focus_event_mode());
        parser.process(b"\x1b[?1004l");
        assert!(!parser.screen().focus_event_mode());
    }
}
