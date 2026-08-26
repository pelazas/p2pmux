//! Drawing a `vt100` screen — and the two legacy fixed-grid views — into a
//! ratatui buffer.

use std::{borrow::Cow, num::NonZeroU16};

use ratatui::{
    Frame,
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    screen::SCROLLBACK_LINES,
    tui::{PaneTextSelection, ScreenCell},
};

pub(in crate::tui) struct VtScreen<'a> {
    screen: &'a vt100::Screen,
    selection: Option<PaneTextSelection>,
    /// How far back the pane is scrolled, so a selection pinned to its lines
    /// can be drawn on the rows those lines are on now.
    scrollback: usize,
    /// Whether this pane is one the user is not typing into.
    dimmed: bool,
}
impl<'a> VtScreen<'a> {
    pub(in crate::tui) fn new(screen: &'a vt100::Screen) -> Self {
        Self {
            screen,
            selection: None,
            scrollback: 0,
            dimmed: false,
        }
    }

    /// Draw every cell at reduced intensity.
    ///
    /// For a pane that does not have focus. A border colour answers "which pane
    /// am I typing into" in the handful of cells the border occupies; this
    /// answers it across the whole rectangle, which is where the eye actually
    /// goes.
    ///
    /// `Modifier::DIM` rather than a computed colour because a cell's colour is
    /// very often `Color::Reset` -- the terminal's own default, whose value
    /// this process does not know and must not guess. Asking the terminal to
    /// draw its own colours faintly is the only form of this that is right for
    /// a default foreground, a 256-colour index and a true-colour triple alike.
    pub(in crate::tui) fn dimmed(mut self, dimmed: bool) -> Self {
        self.dimmed = dimmed;
        self
    }

    pub(in crate::tui) fn with_selection(mut self, selection: Option<PaneTextSelection>) -> Self {
        self.selection = selection;
        self
    }

    pub(in crate::tui) fn at_scrollback(mut self, scrollback: usize) -> Self {
        self.scrollback = scrollback;
        self
    }
}
/// The screen as seen from `scrollback` rows back, copying only when it has to.
///
/// Panes render every frame and almost all of them sit at the live edge, where the
/// screen already shows what the caller asked for. Cloning to say so costs a deep copy
/// of every retained row — 2.8ms per pane at 30x100 with a full buffer, which four
/// panes turn into most of the 16ms frame budget. Borrow when the offsets already
/// agree; a scrolled-back pane still pays for its own copy.
pub(in crate::tui) fn viewed_screen(
    screen: &vt100::Screen,
    scrollback: usize,
) -> Cow<'_, vt100::Screen> {
    if screen.scrollback() == scrollback {
        return Cow::Borrowed(screen);
    }
    let mut owned = screen.clone();
    owned.set_scrollback(scrollback);
    Cow::Owned(owned)
}
pub(in crate::tui) fn available_scrollback(screen: &vt100::Screen) -> usize {
    let mut screen = screen.clone();
    screen.set_scrollback(SCROLLBACK_LINES);
    screen.scrollback()
}
impl Widget for VtScreen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        for row in 0..rows.min(area.height) {
            for col in 0..cols.min(area.width) {
                let Some(source) = self.screen.cell(row, col) else {
                    continue;
                };
                if source.is_wide_continuation() {
                    continue;
                }
                let target = &mut buf[(area.x + col, area.y + row)];
                let contents = source.contents();
                let reserved = if source.is_wide() { 2 } else { 1 };
                target.set_symbol(&fitted_symbol(contents, source.is_wide()));
                // vt100's grid is the wire format the whole session shares, so
                // it is the authority on how many columns this cell occupies.
                // The symbol above is chosen to measure exactly that many, and
                // this says so outright rather than leaving ratatui to measure
                // it again with a table that does not always agree: a cell whose
                // width ratatui gets wrong is not one wrong cell, it is every
                // cell after it on the row, for as long as the back buffer keeps
                // believing it.
                target.set_diff_option(CellDiffOption::ForcedWidth(
                    NonZeroU16::new(reserved).expect("1 and 2 are both nonzero"),
                ));
                let style = if self.selection.is_some_and(|selection| {
                    selection.contains(ScreenCell { row, col }, self.scrollback)
                }) {
                    // Never dimmed. A selection is something the user made a
                    // moment ago and is about to copy, and it can perfectly
                    // well sit in a pane they have since focused away from --
                    // selecting in one pane and working in another is the
                    // ordinary way to use two of them.
                    vt_style(source)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::REVERSED)
                } else if self.dimmed {
                    vt_style(source).add_modifier(Modifier::DIM)
                } else {
                    vt_style(source)
                };
                target.set_style(style);
            }
        }
    }
}
/// The cell's text, trimmed to measure exactly as many columns as the pane's
/// grid reserved for it.
///
/// These two widths are allowed to disagree, and when they do the frame is
/// corrupted well beyond the one cell. `✳\u{fe0f}` is the case that bites: the
/// variation selector asks for emoji presentation, `vt100` still counts the
/// cell as one column wide, and `unicode-width` reads the pair as two. Ratatui
/// believes the symbol and skips the next cell in its diff, so a real character
/// beside the emoji is never drawn; the terminal believes it too and advances
/// the cursor twice, so the rest of that row lands a column early. Neither cell
/// is ever repaired, because the buffer ratatui diffs against records what it
/// *meant* to draw. The result is a row of stale glyphs with new output
/// scattered between them — the same failure `clear_before_first_frame` guards
/// against at startup, arriving mid-session instead.
///
/// Dropping the presentation selectors keeps the character and gives back the
/// width the grid reserved. Anything still too wide falls back to its base
/// character, and then to a space, because a cell that cannot be drawn at the
/// right width is better blank than shifted.
///
/// The measurement is not the whole test. `unicode-width` scores a cluster from
/// its characters; a terminal renders one and advances by what it drew. The two
/// part company over the sequences that ask for emoji presentation by
/// *composition* rather than by codepoint -- a keycap `1\u{fe0f}\u{20e3}`, a
/// ZWJ family -- where the table says one column and every terminal draws two.
/// Those are rejected here even when the arithmetic checks out. See
/// [`composes_an_emoji`].
fn fitted_symbol(contents: &str, wide: bool) -> Cow<'_, str> {
    let reserved = if wide { 2 } else { 1 };
    if contents.is_empty() {
        return Cow::Borrowed(" ");
    }
    if fits(contents, reserved) {
        return Cow::Borrowed(contents);
    }
    let trimmed: String = contents
        .chars()
        .filter(|character| !matches!(character, '\u{fe0e}' | '\u{fe0f}'))
        .collect();
    if fits(&trimmed, reserved) {
        return Cow::Owned(trimmed);
    }
    match trimmed.chars().next() {
        Some(base) if UnicodeWidthChar::width(base) == Some(reserved) => {
            Cow::Owned(base.to_string())
        }
        _ => Cow::Borrowed(" "),
    }
}

/// Whether this cluster measures `reserved` columns *and* will be drawn in
/// `reserved` columns.
fn fits(cluster: &str, reserved: usize) -> bool {
    UnicodeWidthStr::width(cluster) == reserved && !composes_an_emoji(cluster)
}

/// Whether the terminal will read this cluster as an emoji however wide the
/// width table thinks it is.
///
/// The variation selector is the famous half of this and the easy half: it is a
/// codepoint, and `unicode-width` knows what it does. The other half composes:
/// a digit followed by a combining enclosing keycap is a keycap emoji, and
/// anything joined by a zero-width joiner is one glyph made of several. The
/// table scores both by their parts -- one column for `1\u{20e3}`, two for the
/// leading person of a family -- and the terminal draws one emoji, two columns
/// wide, having consumed a cluster the grid gave one or six columns to.
///
/// So they are refused, and the base character is drawn instead. A keycap loses
/// its box and a family loses everybody after the first joiner, which is the
/// same bargain the presentation selectors already strike here: a pane is a
/// fixed grid, the grid is shared with peers over the wire, and a glyph drawn at
/// the wrong width does not cost one cell, it costs every cell after it on that
/// row until something forces a full repaint.
fn composes_an_emoji(cluster: &str) -> bool {
    cluster
        .chars()
        .any(|character| matches!(character, '\u{20e3}' | '\u{200d}'))
}

fn vt_style(cell: &vt100::Cell) -> Style {
    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers.insert(Modifier::BOLD);
    }
    if cell.dim() {
        modifiers.insert(Modifier::DIM);
    }
    if cell.italic() {
        modifiers.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        modifiers.insert(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        modifiers.insert(Modifier::REVERSED);
    }
    Style::default()
        .fg(vt_color(cell.fgcolor()))
        .bg(vt_color(cell.bgcolor()))
        .add_modifier(modifiers)
}
fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}
pub(in crate::tui) fn render_guest_screen(
    frame: &mut Frame<'_>,
    screen: &vt100::Screen,
    footer: &str,
) {
    let area = frame.area();
    let screen_height = screen.size().0.min(area.height.saturating_sub(1));
    let screen_area = Rect::new(area.x, area.y, area.width, screen_height);
    frame.render_widget(VtScreen::new(screen), screen_area);
    let (row, col) = screen.cursor_position();
    if !screen.hide_cursor() && row < screen_height && col < area.width {
        frame.set_cursor_position((area.x + col, area.y + row));
    }
    if area.height > 0 {
        let footer_y = area.y + screen_area.height;
        frame
            .buffer_mut()
            .set_string(area.x, footer_y, footer, Style::default());
    }
}
pub(in crate::tui) fn render_host_screen(
    frame: &mut Frame<'_>,
    screen: &vt100::Screen,
    footer: &str,
) {
    render_guest_screen(frame, screen, footer);
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use ratatui::{
        Terminal,
        backend::{Backend, CrosstermBackend, TestBackend},
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };
    use unicode_width::UnicodeWidthStr;

    use crate::{
        screen::{GuestScreen, HostScreen},
        tui::{PaneTextSelection, ScreenCell, SelectionPoint},
    };

    use super::{VtScreen, fitted_symbol, render_guest_screen, viewed_screen};

    #[test]
    fn remote_renderer_keeps_host_grid_fixed_and_draws_a_footer() {
        let mut host = HostScreen::new(1, 3).expect("host screen");
        let frame = host.process_pty(b"abc").expect("frame");
        let mut guest = GuestScreen::new();
        guest
            .apply_snapshot(frame.sequence, &frame.snapshot)
            .expect("snapshot");
        let mut terminal = Terminal::new(TestBackend::new(5, 3)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_guest_screen(
                    frame,
                    guest.screen().expect("guest screen"),
                    "controller: abcdef idle",
                )
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 0)].symbol(), "c");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].symbol(), "c");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
    }

    #[test]
    fn remote_renderer_places_the_host_cursor() {
        let mut host = HostScreen::new(1, 3).expect("host screen");
        let frame = host.process_pty(b"ab").expect("frame");
        let mut guest = GuestScreen::new();
        guest
            .apply_snapshot(frame.sequence, &frame.snapshot)
            .expect("snapshot");
        let mut terminal = Terminal::new(TestBackend::new(5, 3)).expect("test terminal");

        terminal
            .draw(|frame| {
                render_guest_screen(
                    frame,
                    guest.screen().expect("guest screen"),
                    "controller: peer typing",
                );
            })
            .expect("render");

        terminal.backend_mut().assert_cursor_position((2, 0));
    }

    #[test]
    fn renders_vt100_cell_styles() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"\x1b[31;44;1mX");
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render should work");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "X");
        assert_eq!(buffer[(0, 0)].fg, Color::Indexed(1));
        assert_eq!(buffer[(0, 0)].bg, Color::Indexed(4));
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn renderer_highlights_stream_selected_cells() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: SelectionPoint {
                scrollback: 0,
                cell: ScreenCell { row: 2, col: 1 },
            },
            cursor: SelectionPoint {
                scrollback: 0,
                cell: ScreenCell { row: 0, col: 2 },
            },
        };
        let mut terminal = Terminal::new(TestBackend::new(4, 3)).expect("test terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(
                    VtScreen::new(parser.screen()).with_selection(Some(selection)),
                    frame.area(),
                )
            })
            .expect("render should work");

        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(1, 0)].bg, Color::DarkGray);
        assert_eq!(buffer[(2, 0)].bg, Color::DarkGray);
        assert_eq!(buffer[(0, 1)].bg, Color::DarkGray);
        assert_eq!(buffer[(1, 2)].bg, Color::DarkGray);
        assert_ne!(buffer[(2, 2)].bg, Color::DarkGray);
        assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn renderer_keeps_the_parser_grid_fixed() {
        let mut parser = vt100::Parser::new(2, 3, 0);
        parser.process(b"abc\r\ndef");
        let mut terminal = Terminal::new(TestBackend::new(5, 4)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render should work");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 1)].symbol(), "f");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
    }

    /// The live edge is the common case and it renders every frame, so it must not
    /// copy the row buffer. Only a genuine move off the current offset may.
    #[test]
    fn the_live_view_is_borrowed_and_only_a_scrolled_view_is_copied() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo\r\nsix");

        assert!(matches!(
            viewed_screen(parser.screen(), 0),
            Cow::Borrowed(_)
        ));
        assert!(matches!(viewed_screen(parser.screen(), 2), Cow::Owned(_)));

        // Borrowing must not leave the caller looking at the wrong rows, and the
        // copy must not disturb the screen it came from.
        parser.screen_mut().set_scrollback(2);
        assert!(matches!(
            viewed_screen(parser.screen(), 2),
            Cow::Borrowed(_)
        ));
        let live = viewed_screen(parser.screen(), 0);
        assert_eq!(live.scrollback(), 0);
        assert_eq!(parser.screen().scrollback(), 2);
    }

    #[test]
    fn renderer_uses_the_requested_scrollback_view() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let live = parser.screen().clone();
        let history = viewed_screen(parser.screen(), 1);
        assert_eq!(history.scrollback(), 1);
        assert_ne!(
            history.cell(0, 0).expect("history cell").contents(),
            live.cell(0, 0).expect("live cell").contents()
        );
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(history.as_ref()), frame.area()))
            .expect("render should work");

        assert_eq!(
            terminal.backend().buffer()[(0, 0)].symbol(),
            history.cell(0, 0).expect("history cell").contents()
        );
    }

    /// An emoji-presentation sequence measures two columns while the pane's
    /// grid gives it one. Handing that symbol to ratatui costs the cell beside
    /// it — dropped from the diff, and overwritten on the terminal by the
    /// glyph's second half — and every cell after it on the row is drawn a
    /// column early.
    #[test]
    fn a_narrow_cell_never_carries_a_symbol_two_columns_wide() {
        let mut parser = vt100::Parser::new(1, 6, 0);
        parser.process("\u{2733}\u{fe0f}abcd".as_bytes());
        assert!(
            !parser.screen().cell(0, 0).expect("cell").is_wide(),
            "the grid gives this cell one column"
        );
        let mut terminal = Terminal::new(TestBackend::new(6, 1)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(
            UnicodeWidthStr::width(buffer[(0, 0)].symbol()),
            1,
            "symbol {:?} does not fit the column it was given",
            buffer[(0, 0)].symbol()
        );
        // The character next door survives, which it does not when ratatui is
        // told the emoji owns two columns.
        assert_eq!(buffer[(1, 0)].symbol(), "a");
    }

    #[test]
    fn a_wide_cell_keeps_the_two_columns_the_grid_gave_it() {
        let mut parser = vt100::Parser::new(1, 6, 0);
        parser.process("\u{4f60}\u{597d}".as_bytes());
        let mut terminal = Terminal::new(TestBackend::new(6, 1)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "\u{4f60}");
        assert_eq!(buffer[(2, 0)].symbol(), "\u{597d}");
    }

    /// A row of what an agent pane actually prints, with the four ways a cluster
    /// can claim more columns than the grid gave it: a presentation selector, a
    /// keycap, a zero-width joiner, and an honest wide character.
    const A_BUSY_ROW: &str = "\u{2733}\u{fe0f} \u{26a0}\u{fe0f}ok 1\u{fe0f}\u{20e3}          \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} \u{2705} \u{2570} e\u{0301} \u{4f60}";

    #[test]
    fn every_symbol_matches_the_columns_its_cell_reserved() {
        let mut parser = vt100::Parser::new(1, 40, 0);
        parser.process(A_BUSY_ROW.as_bytes());
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render");

        let buffer = terminal.backend().buffer();
        for col in 0..40_u16 {
            let cell = parser.screen().cell(0, col).expect("cell");
            if cell.is_wide_continuation() {
                continue;
            }
            let reserved = if cell.is_wide() { 2 } else { 1 };
            let symbol = buffer[(col, 0)].symbol();
            assert_eq!(
                UnicodeWidthStr::width(symbol),
                reserved,
                "column {col} symbol {symbol:?} does not fit the columns its cell reserved"
            );
        }
    }

    /// The same claim one layer down: the bytes that reach the terminal.
    ///
    /// Issue #120. A cell whose width the frame gets wrong is not one wrong
    /// cell. `CrosstermBackend` writes a run of adjacent cells with no
    /// re-anchoring cursor move, so the first cluster the terminal draws two
    /// columns wide pushes the rest of that row one column right of where the
    /// back buffer records it -- and the back buffer is what the next frame
    /// diffs against, so every cell it believes is already correct is never
    /// written again. That is a pane that stops repainting until something
    /// forces a full redraw, which is why resizing the window by one column was
    /// the accidental cure.
    ///
    /// Replaying the bytes through `vt100` is a fair model of the terminal here
    /// only because of the second assertion: nothing this renderer emits still
    /// carries a selector, a keycap or a joiner, so the width table `vt100`
    /// applies and the one a terminal applies cannot disagree about it. Without
    /// that, a regression would put `\u{fe0f}` back on the wire and this replay
    /// would keep agreeing with itself while real terminals drifted.
    #[test]
    fn the_bytes_a_pane_writes_leave_the_row_where_the_grid_put_it() {
        let mut parser = vt100::Parser::new(1, 40, 0);
        parser.process(A_BUSY_ROW.as_bytes());
        let area = Rect::new(0, 0, 40, 1);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render");

        // What `Terminal::flush` does with that frame: the difference against
        // the screen ratatui believes is up, handed to the backend the client
        // really uses. Going through the diff is the point -- it is the diff
        // that decides which cells are written and which are assumed correct.
        let drawn = terminal.backend().buffer().clone();
        let mut written: Vec<u8> = Vec::new();
        CrosstermBackend::new(&mut written)
            .draw(Buffer::empty(area).diff(&drawn).into_iter())
            .expect("write the frame");

        let mut terminal_side = vt100::Parser::new(1, 40, 0);
        terminal_side.process(&written);
        for col in 0..40_u16 {
            let sent = parser.screen().cell(0, col).expect("source cell");
            if sent.is_wide_continuation() {
                continue;
            }
            let drawn = terminal_side.screen().cell(0, col).expect("drawn cell");
            // A cell nobody wrote reads as empty rather than as a space: the
            // frame and the screen it is drawn on already agreed about it.
            let drawn = if drawn.contents().is_empty() {
                String::from(" ")
            } else {
                drawn.contents().to_string()
            };
            assert_eq!(
                drawn,
                fitted_symbol(sent.contents(), sent.is_wide()),
                "column {col} landed somewhere else on the terminal's screen"
            );
        }

        let text = String::from_utf8(written).expect("ansi is utf-8");
        for trigger in ['\u{fe0f}', '\u{fe0e}', '\u{20e3}', '\u{200d}'] {
            assert!(
                !text.contains(trigger),
                "{trigger:?} reached the terminal, which draws it wider than the grid"
            );
        }
    }

    #[test]
    fn renderer_erases_cells_cleared_by_the_pty() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");
        parser.process(b"abc");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("initial render");

        parser.process(b"\x1b[2J\x1b[H");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("clear render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), " ");
    }
}
