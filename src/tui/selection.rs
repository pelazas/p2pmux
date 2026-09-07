//! Turning a screen selection into text, and putting that text on the
//! system clipboard.

use std::{
    borrow::Cow,
    io,
    io::Write,
    process::{Command, Stdio},
};

use crate::tui::{PaneTextSelection, text::copied_line_count};

/// The text of a selection, read across as much scrollback as it spans.
///
/// `view_at` hands back the pane as it looks scrolled back that many rows. A
/// selection made without scrolling only ever asks for offset `0`; one dragged
/// past the top of the viewport asks for every offset the drag scrolled
/// through, which is exactly the set the caller has — a local pane keeps its
/// whole buffer, and an attached client keeps the viewports it fetched on the
/// way past. A row nobody can supply is copied as a blank line rather than
/// silently shortening the selection.
pub(in crate::tui) fn selection_text<'a>(
    selection: PaneTextSelection,
    view_at: impl Fn(usize) -> Option<Cow<'a, vt100::Screen>>,
) -> Option<String> {
    if selection.is_empty() {
        return None;
    }
    let (start, end) = selection.bounds();
    let (rows, cols) = view_at(offset_showing(start.line()))
        .or_else(|| view_at(0))?
        .size();
    if rows == 0 || cols == 0 {
        return None;
    }
    let last_col = cols.saturating_sub(1);
    let mut lines = Vec::new();
    let mut line = start.line();
    while line <= end.line() {
        let offset = offset_showing(line);
        // One screen reaches from `-offset` to `rows - 1 - offset`; past that
        // another has to be asked for.
        let last_line_here = (i64::from(rows) - 1 - offset as i64).min(end.line());
        let screen = view_at(offset);
        while line <= last_line_here {
            let first = if line == start.line() {
                start.cell.col.min(last_col)
            } else {
                0
            };
            let last = if line == end.line() {
                end.cell.col.min(last_col)
            } else {
                last_col
            };
            lines.push(match screen.as_deref() {
                Some(screen) => row_text(
                    screen,
                    u16::try_from(line + offset as i64).unwrap_or(0),
                    first,
                    last,
                ),
                None => String::new(),
            });
            line += 1;
        }
    }
    Some(lines.join("\n"))
}

/// The scrollback offset that brings `line` back on screen — zero for anything
/// at or below the live edge, which is where a selection that never scrolled
/// lives.
fn offset_showing(line: i64) -> usize {
    usize::try_from(-line).unwrap_or(0)
}

fn row_text(screen: &vt100::Screen, row: u16, first_col: u16, last_col: u16) -> String {
    let mut line = String::new();
    for col in first_col..=last_col {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if !cell.is_wide_continuation() {
            let contents = cell.contents();
            line.push_str(if contents.is_empty() { " " } else { contents });
        }
    }
    line.trim_end().to_owned()
}
pub(crate) fn copy_selection_to_clipboard(text: &str) -> io::Result<usize> {
    copy_to_system_clipboard(text)?;
    Ok(copied_line_count(text))
}

/// Hand text to whatever owns the clipboard on this machine.
///
/// macOS has one answer and it is always installed. Linux has three, none of
/// them guaranteed: a Wayland session answers to `wl-copy`, an X11 one to
/// `xclip` or `xsel`, and a bare TTY or an `ssh` session to none of them.
#[cfg(target_os = "macos")]
fn copy_to_system_clipboard(text: &str) -> io::Result<()> {
    pipe_to_clipboard_helper("pbcopy", &[], text)
}

#[cfg(not(target_os = "macos"))]
fn copy_to_system_clipboard(text: &str) -> io::Result<()> {
    const HELPERS: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    for (program, arguments) in HELPERS {
        if pipe_to_clipboard_helper(program, arguments, text).is_ok() {
            return Ok(());
        }
    }
    // Nothing local could take it, which is the normal case over ssh and in a
    // bare TTY. OSC 52 hands the text to the terminal emulator instead, so it
    // lands on the clipboard of whichever machine the human is sitting at.
    write_osc52(text)
}

fn pipe_to_clipboard_helper(program: &str, arguments: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other(format!("{program} stdin unavailable")))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    if child.wait()?.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} failed")))
    }
}

/// Ask the terminal emulator to put `text` on its own clipboard.
///
/// Unacknowledged by design: the sequence goes out and the terminal either
/// honours it or drops it silently, so a success here means "asked", not
/// "copied". Terminals that ignore unknown OSC codes — which is all of the ones
/// that do not implement this — discard it without printing anything.
///
/// Written to stderr, not stdout: the TUI renders through stdout, and a byte
/// that arrives from anywhere else is one ratatui does not know it drew.
#[cfg(not(target_os = "macos"))]
fn write_osc52(text: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(osc52_sequence(text).as_bytes())?;
    stderr.flush()
}

/// The OSC 52 clipboard-set sequence for `text`, base64 in the standard
/// alphabet with padding, which is what the terminals implementing this expect.
#[cfg(not(target_os = "macos"))]
fn osc52_sequence(text: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

#[cfg(test)]
mod tests {
    use crate::{
        screen::HostScreen,
        tui::{PaneTextSelection, ScreenCell, SelectionPoint, render::vt::viewed_screen},
    };

    use super::selection_text;

    fn point(scrollback: usize, row: u16, col: u16) -> SelectionPoint {
        SelectionPoint {
            scrollback,
            cell: ScreenCell { row, col },
        }
    }

    #[test]
    fn selection_text_uses_stream_semantics_in_document_order() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 2, 1),
            cursor: point(0, 0, 2),
        };

        assert_eq!(
            selection_text(selection, |offset| Some(viewed_screen(
                parser.screen(),
                offset
            ))),
            Some("cd\nefgh\nij".to_owned())
        );
    }

    #[test]
    fn selection_text_uses_the_scrolled_view_cells() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(1, 0, 0),
            cursor: point(1, 0, 1),
        };

        assert_eq!(
            selection_text(selection, |offset| Some(viewed_screen(
                parser.screen(),
                offset
            ))),
            Some("on".to_owned())
        );
    }

    /// The whole point of #79: a drag that pulls the pane past its own top has
    /// one end in the scrollback and one on screen, and copying it must give
    /// back every line between them — not the page that happens to be visible
    /// when the button comes up.
    #[test]
    fn node_owned_host_buffer_copies_a_scrolled_selection_without_viewport_holes() {
        let mut host = HostScreen::new(2, 6).expect("host screen");
        host.process_pty(b"LINE-1\r\nLINE-2\r\nLINE-3\r\nLINE-4\r\nLINE-5\r\nLINE-6")
            .expect("screen contents");

        // Anchored on `LINE-6` at the live edge, then dragged up until `LINE-1` —
        // four rows back — is the top row on screen. Three screens' worth, and
        // only two of those rows were ever visible at once.
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 1, 5),
            cursor: point(4, 0, 0),
        };

        assert_eq!(
            selection_text(selection, |offset| Some(viewed_screen(
                host.screen(),
                offset
            ))),
            Some("LINE-1\nLINE-2\nLINE-3\nLINE-4\nLINE-5\nLINE-6".to_owned())
        );
    }

    /// A client holds only the viewports it fetched. One it never got is a
    /// blank line rather than a selection that quietly comes back short — the
    /// lines either side of it are still in the right places.
    #[test]
    fn a_view_the_caller_cannot_supply_leaves_its_line_blank() {
        let mut parser = vt100::Parser::new(1, 6, 20);
        parser.process(b"one\r\ntwo\r\nthree");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 4),
            cursor: point(2, 0, 0),
        };

        assert_eq!(
            selection_text(selection, |offset| (offset != 1)
                .then(|| viewed_screen(parser.screen(), offset))),
            Some("one\n\nthree".to_owned())
        );
    }

    /// The one part of the clipboard path with no visible failure: a terminal
    /// that receives a malformed OSC 52 discards it in silence, exactly as one
    /// that does not implement OSC 52 discards a correct one. Nothing at run
    /// time can tell those apart, so the bytes are pinned here instead.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn osc52_carries_the_selection_as_padded_base64() {
        use super::osc52_sequence;

        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
        // A multi-line selection travels as one payload; the newline is data,
        // not a terminator, and must not end the sequence early.
        assert_eq!(osc52_sequence("a\nb"), "\x1b]52;c;YQpi\x07");
    }

    #[test]
    fn empty_selection_has_no_clipboard_text() {
        let parser = vt100::Parser::new(1, 1, 0);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 0),
            cursor: point(0, 0, 0),
        };

        assert_eq!(
            selection_text(selection, |offset| Some(viewed_screen(
                parser.screen(),
                offset
            ))),
            None
        );
    }

    fn copied(parser: &vt100::Parser, selection: PaneTextSelection) -> Option<String> {
        selection_text(selection, |offset| {
            Some(viewed_screen(parser.screen(), offset))
        })
    }

    /// Issue #139: a sentence the pane wrapped to fit is one line in the
    /// document and has to come back as one line on the clipboard. Splitting
    /// on the wrap made a drag over three paragraphs copy as four lines, with
    /// the last one starting mid-word.
    #[test]
    fn a_soft_wrapped_row_copies_as_one_line() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"abcdefghij");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 0),
            cursor: point(0, 1, 1),
        };

        assert_eq!(copied(&parser, selection), Some("abcdefghij".to_owned()));
    }

    /// A real line break is still a line break. Wrap-joining must not swallow
    /// `\r\n` just because the previous row happened to fill the width.
    #[test]
    fn a_hard_break_still_splits_even_when_the_row_is_full() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"abcdefgh\r\nijklmnop");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 0),
            cursor: point(0, 1, 7),
        };

        assert_eq!(
            copied(&parser, selection),
            Some("abcdefgh\nijklmnop".to_owned())
        );
    }

    /// Selecting only the first visual row of a wrap does not invent the rest.
    #[test]
    fn a_partial_wrap_copies_only_the_selected_rows() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"abcdefghij");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 0),
            cursor: point(0, 0, 7),
        };

        assert_eq!(copied(&parser, selection), Some("abcdefgh".to_owned()));
    }

    /// The wrap can sit across two viewports: a two-row pane that then
    /// scrolled, so the first visual row is one offset and the continuation
    /// is another. Copying still has to join them.
    #[test]
    fn a_wrap_that_scrolled_off_still_joins() {
        let mut parser = vt100::Parser::new(2, 8, 10);
        parser.process(b"abcdefghij\r\nXXXXXXX\r\nYYYYYYY");
        // Live view is the last two hard-broken rows. The wrap starts at the
        // top of offset 2 (`abcdefgh`) and continues as the top of offset 1
        // (`ij`), so a copy has to join across viewports.
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(2, 0, 0),
            cursor: point(1, 0, 1),
        };

        assert_eq!(copied(&parser, selection), Some("abcdefghij".to_owned()));
    }

    /// A wrap followed by a real newline in the same drag is two clipboard
    /// lines, not three and not one.
    #[test]
    fn a_wrap_then_a_hard_break_copies_as_two_lines() {
        let mut parser = vt100::Parser::new(3, 8, 0);
        parser.process(b"abcdefghij\r\nnext");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 0),
            cursor: point(0, 2, 3),
        };

        assert_eq!(
            copied(&parser, selection),
            Some("abcdefghij\nnext".to_owned())
        );
    }

    /// Starting mid-row still joins onto the continuation.
    #[test]
    fn a_wrap_joined_from_mid_row_keeps_the_selected_suffix() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"abcdefghij");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: point(0, 0, 2),
            cursor: point(0, 1, 1),
        };

        assert_eq!(copied(&parser, selection), Some("cdefghij".to_owned()));
    }
}
