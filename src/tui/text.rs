//! Width-aware string helpers shared by every piece of TUI chrome.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn text_width(text: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
}
pub(super) fn truncate_trailing(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut result = String::new();
    for character in value.chars() {
        if UnicodeWidthStr::width(result.as_str()).saturating_add(character.width().unwrap_or(0))
            > width - 1
        {
            break;
        }
        result.push(character);
    }
    result.push('…');
    result
}
pub(super) fn copied_line_count(text: &str) -> usize {
    text.split('\n').count().max(1)
}
/// Break a ticket across lines by character count.
///
/// A ticket is one unbroken base32-ish run, so ratatui's word wrapping would leave most of
/// every line empty and make the thing look longer than it is.
pub(super) fn wrap_fixed(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    text.chars()
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}
pub(super) fn truncate_leading(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut result = String::new();
    for character in value.chars().rev() {
        if UnicodeWidthStr::width(result.as_str()).saturating_add(character.width().unwrap_or(0))
            > width - 1
        {
            break;
        }
        result.insert(0, character);
    }
    format!("…{result}")
}
pub(super) fn sanitize_single_line(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}
/// Cut `text` to at most `limit` bytes on a character boundary.
pub(super) fn truncate_bytes(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let end = (0..=limit)
            .rev()
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(0);
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::copied_line_count;

    #[test]
    fn copied_line_count_reports_newline_separated_lines() {
        assert_eq!(copied_line_count("one line"), 1);
        assert_eq!(copied_line_count("first\nsecond\nthird"), 3);
        assert_eq!(copied_line_count(""), 1);
    }
}
