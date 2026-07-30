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
    copy_to_macos_clipboard(text)?;
    Ok(copied_line_count(text))
}
fn copy_to_macos_clipboard(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("pbcopy stdin unavailable"))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    if child.wait()?.success() {
        Ok(())
    } else {
        Err(io::Error::other("pbcopy failed"))
    }
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

        assert_eq!(selection_text(&screen, selection), Some("on".to_owned()));
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
