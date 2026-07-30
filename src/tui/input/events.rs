//! Terminal event intake and the frame pacing the run loops share.

use std::{
    io,
    io::Write,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, MouseEventKind};

/// Upper bound on terminal events applied per loop cycle so one burst cannot
/// starve PTY draining forever.
pub(in crate::tui) const MAX_EVENTS_PER_CYCLE: usize = 256;
/// Frames are presented at most once per interval (~60 fps) so parse and input
/// work is never crowded out by redraws faster than anyone can perceive.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
pub(in crate::tui) fn frame_due(last_draw: Option<Instant>) -> bool {
    last_draw.is_none_or(|drawn| drawn.elapsed() >= FRAME_INTERVAL)
}
/// Asks the outer terminal to present the writes up to the matching end marker
/// atomically (DEC 2026). Terminals without support ignore both markers.
pub(in crate::tui) fn begin_synchronized_output() -> io::Result<()> {
    io::stdout().write_all(b"\x1b[?2026h")
}
pub(in crate::tui) fn end_synchronized_output() -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1b[?2026l")?;
    stdout.flush()
}
/// Poll timeout for the run loops: until the next frame deadline while a dirty
/// frame is waiting, one full interval when idle.
pub(in crate::tui) fn event_poll_timeout(dirty: bool, last_draw: Option<Instant>) -> Duration {
    if !dirty {
        return FRAME_INTERVAL;
    }
    last_draw
        .map(|drawn| FRAME_INTERVAL.saturating_sub(drawn.elapsed()))
        .unwrap_or(Duration::ZERO)
        .max(Duration::from_millis(1))
}
fn is_mouse_moved(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Moved)
    )
}
/// Reads every queued terminal event without blocking. Hover only depends on the
/// final pointer position, so all mouse-moves but the last are dropped; a burst
/// of moves then costs one update instead of one redraw cycle each.
pub(in crate::tui) fn collect_pending_events(max: usize) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    while events.len() < max && event::poll(Duration::ZERO)? {
        events.push(event::read()?);
    }
    if let Some(last_moved) = events.iter().rposition(is_mouse_moved) {
        let mut index = 0;
        events.retain(|event| {
            let keep = index == last_moved || !is_mouse_moved(event);
            index += 1;
            keep
        });
    }
    Ok(events)
}
