//! Turning a screen selection into text, and putting that text on the
//! system clipboard.

use std::{
    io,
    io::Write,
    process::{Command, Stdio},
};

use crate::tui::{PaneTextSelection, ScreenCell, text::copied_line_count};

pub(in crate::tui) fn selection_text(
    screen: &vt100::Screen,
    selection: PaneTextSelection,
) -> Option<String> {
    if selection.is_empty() {
        return None;
    }
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }
    let (start, end) = selection.bounds();
    let start = ScreenCell {
        row: start.row.min(rows.saturating_sub(1)),
        col: start.col.min(cols.saturating_sub(1)),
    };
    let end = ScreenCell {
        row: end.row.min(rows.saturating_sub(1)),
        col: end.col.min(cols.saturating_sub(1)),
    };

    let lines = (start.row..=end.row)
        .map(|row| {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row { end.col } else { cols - 1 };
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
        })
        .collect::<Vec<_>>();
    Some(lines.join("\n"))
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
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let mut stderr = io::stderr().lock();
    write!(stderr, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
    stderr.flush()
}

#[cfg(test)]
mod tests {
    use crate::tui::{PaneTextSelection, ScreenCell, render::vt::viewed_screen};

    use super::selection_text;

    #[test]
    fn selection_text_uses_stream_semantics_in_document_order() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 2, col: 1 },
            cursor: ScreenCell { row: 0, col: 2 },
        };

        assert_eq!(
            selection_text(parser.screen(), selection),
            Some("cd\nefgh\nij".to_owned())
        );
    }

    #[test]
    fn selection_text_uses_the_scrolled_view_cells() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let screen = viewed_screen(parser.screen(), 1);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 0, col: 0 },
            cursor: ScreenCell { row: 0, col: 1 },
        };

        assert_eq!(
            selection_text(screen.as_ref(), selection),
            Some("on".to_owned())
        );
    }

    #[test]
    fn empty_selection_has_no_clipboard_text() {
        let parser = vt100::Parser::new(1, 1, 0);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 0, col: 0 },
            cursor: ScreenCell { row: 0, col: 0 },
        };

        assert_eq!(selection_text(parser.screen(), selection), None);
    }
}
