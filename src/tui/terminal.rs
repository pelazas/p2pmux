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

/// Stop `NO_COLOR` from taking every text attribute down with the colours.
///
/// Crossterm reads `NO_COLOR` once, process-wide, and then makes every `Colored`
/// render as the empty string. `SetColors` — the one command ratatui's backend
/// uses, once per cell whose colour changed — still writes both halves and the
/// separator around them, so what reaches the terminal is `ESC [ ; m`: an SGR
/// with no parameters, which every terminal reads as SGR 0. A full reset.
///
/// Ratatui emits the modifier diff *before* the colours, so that reset lands
/// between "this cell is faint" and the cell's own glyph. The stream carries the
/// `ESC [ 2 m` and the cell is drawn at full intensity anyway — and so is
/// everything else the frame asked for, because SGR 0 clears bold, reverse and
/// underline with it. Unfocused panes stop looking unfocused, a text selection
/// stops looking selected, and `vt100`'s replay of whatever is running in a pane
/// loses the attributes that program chose.
///
/// So p2pmux opts out of crossterm's handling and keeps its own. A multiplexer
/// is the wrong layer for `NO_COLOR` in any case: most of what it puts on screen
/// is another program's output, and that program read `NO_COLOR` out of the
/// environment p2pmux passed it and already decided for itself. Repainting its
/// frame in the terminal's default colours would be p2pmux overriding an answer
/// that was not its to give.
pub(crate) fn keep_attributes_through_no_color() {
    crossterm::style::Colored::set_ansi_color_disabled(false);
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
    use crossterm::style::Colored;
    use ratatui::{
        Terminal, TerminalOptions, Viewport,
        backend::{Backend, CrosstermBackend, TestBackend},
        buffer::Cell,
        layout::Rect,
        style::{Color, Modifier, Style},
        widgets::Paragraph,
    };

    use super::{clear_before_first_frame, keep_attributes_through_no_color};

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

    /// A pane border followed by the first faint cell of the body beside it,
    /// drawn through the same backend the real client uses, as the escape
    /// sequence a terminal would receive.
    ///
    /// The border carries a colour and the body does not, which is what makes
    /// ratatui emit `SetColors` between the two — a run of same-coloured cells
    /// never asks for its colours twice, and the bug rides on that command.
    fn faint_body_after_border() -> String {
        let mut border = Cell::default();
        border.set_symbol("│");
        border.set_style(Style::default().fg(Color::Yellow));
        let mut body = Cell::default();
        body.set_symbol("A");
        body.set_style(Style::default().add_modifier(Modifier::DIM));
        let cells = [(0_u16, 0_u16, &border), (1_u16, 0_u16, &body)];
        let mut stream: Vec<u8> = Vec::new();
        CrosstermBackend::new(&mut stream)
            .draw(cells.into_iter())
            .expect("draw two cells");
        String::from_utf8(stream).expect("ansi is utf-8")
    }

    /// Whether the terminal would be in faint intensity by the time it draws
    /// `A` — the same replay `scripts/e2e/check_dim_panes.py` does, and the only
    /// question the user is actually asking of the frame.
    fn faint_at_the_glyph(stream: &str) -> bool {
        let glyph = stream.rfind('A').expect("the cell was drawn");
        let mut faint = false;
        let mut rest = &stream[..glyph];
        while let Some(start) = rest.find("\u{1b}[") {
            let Some(end) = rest[start..].find('m') else {
                break;
            };
            let parameters = &rest[start + 2..start + end];
            if parameters
                .chars()
                .all(|character| character.is_ascii_digit() || character == ';')
            {
                for value in parameters.split(';') {
                    match if value.is_empty() { "0" } else { value } {
                        "2" => faint = true,
                        "0" | "22" => faint = false,
                        _ => {}
                    }
                }
            }
            rest = &rest[start + end + 1..];
        }
        faint
    }

    /// Issue #113: the dim survived ratatui and died in crossterm.
    ///
    /// `NO_COLOR` makes crossterm render both halves of `SetColors` as nothing
    /// while still writing the separator between them, so a cell's colours reach
    /// the terminal as `ESC [ ; m` — a parameterless SGR, which is a full reset.
    /// Ratatui writes the modifiers first, so that reset lands after the
    /// `ESC [ 2 m` and before the glyph: the stream carries the dim and the
    /// terminal never applies it.
    ///
    /// Both halves run in one test because the switch is process-wide state.
    #[test]
    fn no_color_does_not_reset_the_dim_before_the_glyph() {
        // Stand in for the environment variable: crossterm memoises its
        // `NO_COLOR` read at first use, so setting it here would be too late.
        Colored::set_ansi_color_disabled(true);
        let broken = faint_body_after_border();
        assert!(
            broken.contains("\u{1b}[;m"),
            "expected crossterm's empty SGR to reproduce the bug: {broken:?}"
        );
        assert!(
            !faint_at_the_glyph(&broken),
            "the bug should leave the glyph at full intensity: {broken:?}"
        );

        keep_attributes_through_no_color();
        let fixed = faint_body_after_border();
        assert!(
            !fixed.contains("\u{1b}[;m"),
            "no parameterless SGR should reach the terminal: {fixed:?}"
        );
        assert!(
            fixed.contains("\u{1b}[2m"),
            "the frame still asks for faint intensity: {fixed:?}"
        );
        assert!(
            faint_at_the_glyph(&fixed),
            "the glyph is drawn faint: {fixed:?}"
        );
    }
}
