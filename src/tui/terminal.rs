//! Raw mode, the alternate screen, and the keyboard-enhancement handshake —
//! entered on start and undone on drop.

use std::{fs::OpenOptions, io, io::Write};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use ratatui::{Terminal, backend::Backend, layout::Rect};

pub(in crate::tui) struct TerminalGuard {
    pub(in crate::tui) raw_mode: bool,
    pub(in crate::tui) keyboard_enhancement: bool,
    pub(in crate::tui) alternate_screen: bool,
    pub(in crate::tui) bracketed_paste: bool,
    pub(in crate::tui) mouse_capture: bool,
}
impl TerminalGuard {
    pub(in crate::tui) fn new() -> Self {
        Self {
            raw_mode: false,
            keyboard_enhancement: false,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_capture: false,
        }
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhancement {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
            if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
                let _ = execute!(tty, PopKeyboardEnhancementFlags);
                let _ = tty.flush();
            }
        }
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.mouse_capture {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        if self.alternate_screen {
            let _ = execute!(stdout, crossterm::cursor::Show, LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}
/// Blank the viewport, and ratatui's diff baseline with it, before the first frame.
///
/// Entering the alternate screen does not mean landing on a blank one: a terminal
/// already showing it treats the switch as a no-op and keeps every cell that was
/// there. Ratatui only ever writes the difference against a back buffer it starts
/// blank, so each surviving cell is one it believes it has already drawn — the
/// stale text then outlives every redraw, and new output lands between the letters
/// of whatever the previous occupant left behind.
///
/// `resize` rather than `clear`, though both blank the region and reset the back
/// buffer: `Terminal::clear` reads the cursor position first, and that DSR round
/// trip never returns on a terminal that does not answer it, taking startup down
/// with it.
pub(crate) fn clear_before_first_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    area: Rect,
) -> Result<(), B::Error> {
    terminal.resize(area)
}

pub(in crate::tui) fn enable_keyboard_enhancement() -> io::Result<bool> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal, TerminalOptions, Viewport,
        backend::{Backend, TestBackend},
        buffer::Cell,
        layout::Rect,
        widgets::Paragraph,
    };

    use super::clear_before_first_frame;

    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(Cell::symbol)
            .collect()
    }

    /// A terminal already on the alternate screen keeps its cells through the switch,
    /// and ratatui only ever writes the difference against a back buffer it starts
    /// blank — so without an opening clear the leftovers are cells it never revisits,
    /// and new output arrives interleaved with text from whoever was there before.
    #[test]
    fn a_dirty_screen_does_not_survive_the_first_frame() {
        let area = Rect::new(0, 0, 12, 3);
        let mut terminal = Terminal::with_options(
            TestBackend::new(area.width, area.height),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("test terminal");
        let mut stale = Cell::default();
        stale.set_symbol("Z");
        let leftovers: Vec<(u16, u16, Cell)> = (0..area.height)
            .flat_map(|row| {
                let stale = stale.clone();
                (0..area.width).map(move |col| (col, row, stale.clone()))
            })
            .collect();
        terminal
            .backend_mut()
            .draw(leftovers.iter().map(|(col, row, cell)| (*col, *row, cell)))
            .expect("prior occupant's screen");
        assert!(rendered(&terminal).contains('Z'));

        clear_before_first_frame(&mut terminal, area).expect("opening clear");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("hi"), frame.area()))
            .expect("first frame");

        let screen = rendered(&terminal);
        assert!(!screen.contains('Z'), "stale cells survived: {screen:?}");
        assert!(screen.starts_with("hi"));
    }
}
