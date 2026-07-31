//! The bottom help strip: which keys are offered at this width, the chord
//! badge, and the copy/notice flashes that share the line.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::{
    config::UiTheme,
    tui::{ChordMode, text::text_width},
};

pub(in crate::tui) enum FooterSegment {
    Text(&'static str),
    Key(&'static str),
    OrangeKey(&'static str),
}
const NORMAL_FOOTER_FULL: &[FooterSegment] = &[
    FooterSegment::OrangeKey("Ctrl"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("p"),
    FooterSegment::Text("> PANE   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TAB   <"),
    FooterSegment::Key("a"),
    FooterSegment::Text("> AGENTS   <"),
    FooterSegment::Key("s"),
    FooterSegment::Text("> SHARE   <"),
    FooterSegment::Key("q"),
    FooterSegment::Text("> QUIT   "),
    FooterSegment::OrangeKey("Option"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("shift"),
    FooterSegment::Text("> + <"),
    FooterSegment::Key("↑↓←→"),
    FooterSegment::Text("> FOCUS"),
];
const NORMAL_FOOTER_NO_FOCUS: &[FooterSegment] = &[
    FooterSegment::OrangeKey("Ctrl"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("p"),
    FooterSegment::Text("> PANE   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TAB   <"),
    FooterSegment::Key("a"),
    FooterSegment::Text("> AGENTS   <"),
    FooterSegment::Key("s"),
    FooterSegment::Text("> SHARE   <"),
    FooterSegment::Key("q"),
    FooterSegment::Text("> QUIT"),
];
const NORMAL_FOOTER_NO_SHARE: &[FooterSegment] = &[
    FooterSegment::OrangeKey("Ctrl"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("p"),
    FooterSegment::Text("> PANE   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TAB   <"),
    FooterSegment::Key("a"),
    FooterSegment::Text("> AGENTS   <"),
    FooterSegment::Key("q"),
    FooterSegment::Text("> QUIT"),
];
const NORMAL_FOOTER_CORE: &[FooterSegment] = &[
    FooterSegment::OrangeKey("Ctrl"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("p"),
    FooterSegment::Text("> PANE   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TAB   <"),
    FooterSegment::Key("q"),
    FooterSegment::Text("> QUIT"),
];
const NORMAL_FOOTER_KEYS_ONLY: &[FooterSegment] = &[
    FooterSegment::OrangeKey("Ctrl"),
    FooterSegment::Text("+ <"),
    FooterSegment::Key("p"),
    FooterSegment::Text("> <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> <"),
    FooterSegment::Key("q"),
    FooterSegment::Text(">"),
];
/// Normal-mode help, widest tier first.
///
/// The bar sheds whole clauses instead of clipping mid-word, cheapest hint first: the focus
/// arrows are muscle memory, sharing happens once a session, and the agent overlay is opened
/// constantly. `Ctrl+P/T/Q` is the floor — nothing else in the mux is reachable without them,
/// so they outlive every other hint.
const NORMAL_FOOTER_TIERS: &[&[FooterSegment]] = &[
    NORMAL_FOOTER_FULL,
    NORMAL_FOOTER_NO_FOCUS,
    NORMAL_FOOTER_NO_SHARE,
    NORMAL_FOOTER_CORE,
    NORMAL_FOOTER_KEYS_ONLY,
];
const PANE_FOOTER: &[FooterSegment] = &[
    FooterSegment::Text("  <"),
    FooterSegment::Key("←↓↑→"),
    FooterSegment::Text("> FOCUS   <"),
    FooterSegment::Key("e"),
    FooterSegment::Text("> RENAME   <"),
    FooterSegment::Key("n"),
    FooterSegment::Text("> NEW   <"),
    FooterSegment::Key("r/l/d/u"),
    FooterSegment::Text("> SPLIT   <"),
    FooterSegment::Key("x"),
    FooterSegment::Text("> CLOSE   <"),
    FooterSegment::Key("k"),
    FooterSegment::Text("> LOCK   <"),
    FooterSegment::Key("L"),
    FooterSegment::Text("> LOCK SESSION   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> BACK"),
];
const TAB_FOOTER: &[FooterSegment] = &[
    FooterSegment::Text("  <"),
    FooterSegment::Key("←→"),
    FooterSegment::Text("> SWITCH   <"),
    FooterSegment::Key("e"),
    FooterSegment::Text("> RENAME   <"),
    FooterSegment::Key("n"),
    FooterSegment::Text("> NEW   <"),
    FooterSegment::Key("x"),
    FooterSegment::Text("> CLOSE   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> BACK"),
];
pub(in crate::tui) const AGENT_OVERLAY_HELP: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("↑↓"),
    FooterSegment::Text("> MOVE   <"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> FOCUS"),
];
/// Enter takes the ticket: it is the only identifier that reaches another machine, so it is
/// Enter takes the code: it is what a person can actually relay over a call, and it resolves
/// to the ticket anywhere. The ticket stays one key away for a peer who cannot reach the
/// rendezvous service, or who would rather not depend on it.
pub(in crate::tui) const SHARE_HELP: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> COPY CODE   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> COPY TICKET   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
/// When the rendezvous was unreachable there is no code to offer, so Enter falls back.
pub(in crate::tui) const SHARE_HELP_NO_CODE: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> COPY TICKET   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
pub(in crate::tui) const SHARE_HELP_GUEST: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
/// The help segments for a chord mode, narrowed to the widest tier `width` can hold.
///
/// Only normal mode has tiers. The chord footers already fall back atomically through
/// `chord_footer_badge`, which hides the whole bar rather than showing half a chord.
pub(in crate::tui) fn contextual_footer(
    chord_mode: ChordMode,
    width: u16,
) -> &'static [FooterSegment] {
    match chord_mode {
        ChordMode::None => NORMAL_FOOTER_TIERS
            .iter()
            .copied()
            .find(|tier| footer_segments_width(tier) <= width)
            .unwrap_or(NORMAL_FOOTER_KEYS_ONLY),
        ChordMode::Pane => PANE_FOOTER,
        ChordMode::Tab => TAB_FOOTER,
    }
}
pub(in crate::tui) fn render_footer_segments(
    buffer: &mut Buffer,
    theme: &UiTheme,
    mut x: u16,
    y: u16,
    end_x: u16,
    segments: &[FooterSegment],
) -> u16 {
    for segment in segments {
        let (text, style) = match segment {
            FooterSegment::Text(text) => (
                *text,
                Style::default()
                    .fg(theme.footer_muted)
                    .bg(theme.footer_background),
            ),
            FooterSegment::Key(text) => (
                *text,
                Style::default()
                    .fg(theme.footer_accent)
                    .bg(theme.footer_background)
                    .add_modifier(Modifier::BOLD),
            ),
            FooterSegment::OrangeKey(text) => (
                *text,
                Style::default()
                    .fg(theme.footer_orange)
                    .bg(theme.footer_background)
                    .add_modifier(Modifier::BOLD),
            ),
        };
        x = buffer
            .set_stringn(x, y, text, usize::from(end_x.saturating_sub(x)), style)
            .0;
    }
    x
}
fn footer_segments_width(segments: &[FooterSegment]) -> u16 {
    segments
        .iter()
        .map(|segment| match segment {
            FooterSegment::Text(text)
            | FooterSegment::Key(text)
            | FooterSegment::OrangeKey(text) => text_width(text),
        })
        .sum()
}
pub(in crate::tui) fn chord_footer_badge(
    chord_mode: ChordMode,
    width: u16,
) -> Option<&'static str> {
    let (full, compact) = match chord_mode {
        ChordMode::Pane => ("PANE MODE", "PANE"),
        ChordMode::Tab => ("TAB MODE", "TAB"),
        ChordMode::None => return None,
    };

    if width >= text_width(full) {
        Some(full)
    } else if width >= text_width(compact) {
        Some(compact)
    } else {
        None
    }
}
fn render_copy_feedback(
    buffer: &mut Buffer,
    theme: &UiTheme,
    mut x: u16,
    y: u16,
    end_x: u16,
    lines: usize,
) {
    x = buffer
        .set_stringn(
            x,
            y,
            "  copied ",
            usize::from(end_x.saturating_sub(x)),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background),
        )
        .0;
    buffer.set_stringn(
        x,
        y,
        format!("{lines} line{}", if lines == 1 { "" } else { "s" }),
        usize::from(end_x.saturating_sub(x)),
        Style::default()
            .fg(theme.copy_feedback_accent)
            .bg(theme.footer_background),
    );
}
fn mid_footer_flash_width(copied_lines: Option<usize>, footer_notice: Option<&str>) -> u16 {
    if let Some(notice) = footer_notice {
        return u16::try_from(notice.chars().count().saturating_add(2)).unwrap_or(u16::MAX);
    }
    if let Some(lines) = copied_lines {
        let count = format!("{lines} line{}", if lines == 1 { "" } else { "s" });
        return u16::try_from(
            count
                .chars()
                .count()
                .saturating_add("  copied ".chars().count()),
        )
        .unwrap_or(u16::MAX);
    }
    0
}
struct FooterText {
    text: String,
    rect: Rect,
}
enum FooterFlash {
    Notice(FooterText),
    CopyFeedback { lines: usize, rect: Rect },
}
/// The shared placement authority for footer content.
struct FooterLayout {
    badge: Option<&'static str>,
    /// The help tier that fits, chosen once so measuring and drawing cannot disagree.
    help: &'static [FooterSegment],
    help_start: u16,
    help_end: u16,
    status: Option<FooterText>,
    flash: Option<FooterFlash>,
}
fn footer_layout(
    area: Rect,
    chord_mode: ChordMode,
    status: &str,
    footer_notice: Option<&str>,
    copied_lines: Option<usize>,
) -> FooterLayout {
    let end_x = area.right();

    if chord_mode != ChordMode::None {
        let badge = chord_footer_badge(chord_mode, area.width);
        let mut x = area.x.saturating_add(badge.map_or(0, text_width));
        let segments = contextual_footer(chord_mode, end_x.saturating_sub(x));
        if badge.is_none() || end_x.saturating_sub(x) < footer_segments_width(segments) {
            return FooterLayout {
                badge,
                help: segments,
                help_start: x,
                help_end: end_x,
                status: None,
                flash: None,
            };
        }

        x = x.saturating_add(footer_segments_width(segments));
        let help_end = x;
        let status = (!status.is_empty()).then(|| {
            let text = format!("{status}  ");
            let width = text_width(&text);
            (text, width)
        });
        let status = status.and_then(|(text, width)| {
            (end_x.saturating_sub(x) >= width).then(|| {
                let rect = Rect::new(x, area.y, width, 1);
                x = x.saturating_add(width);
                FooterText { text, rect }
            })
        });
        let flash = footer_notice.and_then(|notice| {
            let text = format!("  {notice}");
            let width = text_width(&text);
            (end_x.saturating_sub(x) >= width).then(|| {
                let rect = Rect::new(x, area.y, width, 1);
                x = x.saturating_add(width);
                FooterFlash::Notice(FooterText { text, rect })
            })
        });
        return FooterLayout {
            badge,
            help: segments,
            help_start: area.x.saturating_add(badge.map_or(0, text_width)),
            help_end,
            status,
            flash,
        };
    }

    let mut x = area.x;
    let status = (!status.is_empty()).then(|| {
        let text = format!("{status}  ");
        let width = text_width(&text).min(end_x.saturating_sub(x));
        let rect = Rect::new(x, area.y, width, 1);
        x = x.saturating_add(width);
        FooterText { text, rect }
    });
    let flash_width = if footer_notice.is_some() {
        mid_footer_flash_width(None, footer_notice)
    } else if status.is_none() {
        mid_footer_flash_width(copied_lines, None)
    } else {
        0
    };
    let help_end = end_x.saturating_sub(flash_width);
    // The help tier is chosen against the span left once status and any flash are placed, so a
    // long notice narrows the hints rather than overwriting them.
    let segments = contextual_footer(chord_mode, help_end.saturating_sub(x));
    let flash_start = x.max(help_end);
    let flash_rect = Rect::new(flash_start, area.y, end_x.saturating_sub(flash_start), 1);
    let flash = footer_notice
        .map(|notice| {
            FooterFlash::Notice(FooterText {
                text: format!("  {notice}"),
                rect: flash_rect,
            })
        })
        .or_else(|| {
            status
                .is_none()
                .then_some(copied_lines)
                .flatten()
                .map(|lines| FooterFlash::CopyFeedback {
                    lines,
                    rect: flash_rect,
                })
        });

    FooterLayout {
        badge: None,
        help: segments,
        help_start: x,
        help_end,
        status,
        flash,
    }
}
fn render_chord_footer(buffer: &mut Buffer, theme: &UiTheme, area: Rect, layout: &FooterLayout) {
    let Some(badge) = layout.badge else {
        return;
    };
    buffer.set_stringn(
        area.x,
        area.y,
        badge,
        usize::from(area.right().saturating_sub(area.x)),
        Style::default()
            .fg(theme.footer_orange)
            .bg(theme.footer_background)
            .add_modifier(Modifier::BOLD),
    );
    render_footer_segments(
        buffer,
        theme,
        layout.help_start,
        area.y,
        layout.help_end,
        layout.help,
    );

    if let Some(status) = &layout.status {
        buffer.set_stringn(
            status.rect.x,
            area.y,
            &status.text,
            usize::from(status.rect.width),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background),
        );
    }
    if let Some(FooterFlash::Notice(notice)) = &layout.flash {
        buffer.set_stringn(
            notice.rect.x,
            area.y,
            &notice.text,
            usize::from(notice.rect.width),
            Style::default()
                .fg(theme.footer_orange)
                .bg(theme.footer_background)
                .add_modifier(Modifier::BOLD),
        );
    }
}
pub(in crate::tui) fn render_contextual_footer(
    buffer: &mut Buffer,
    theme: &UiTheme,
    area: Rect,
    status: &str,
    copied_lines: Option<usize>,
    footer_notice: Option<&str>,
    chord_mode: ChordMode,
) {
    let layout = footer_layout(area, chord_mode, status, footer_notice, copied_lines);
    buffer.set_stringn(
        area.x,
        area.y,
        " ".repeat(usize::from(area.width)),
        usize::from(area.width),
        Style::default().bg(theme.footer_background),
    );

    if chord_mode != ChordMode::None {
        render_chord_footer(buffer, theme, area, &layout);
        return;
    }

    if let Some(status) = &layout.status {
        buffer.set_stringn(
            status.rect.x,
            area.y,
            &status.text,
            usize::from(status.rect.width),
            Style::default()
                .fg(theme.footer_muted)
                .bg(theme.footer_background),
        );
    }
    render_footer_segments(
        buffer,
        theme,
        layout.help_start,
        area.y,
        layout.help_end,
        layout.help,
    );
    // Mid-bar flash messages sit after the help, filling the bar's right end.
    match &layout.flash {
        Some(FooterFlash::Notice(notice)) => {
            buffer.set_stringn(
                notice.rect.x,
                area.y,
                &notice.text,
                usize::from(notice.rect.width),
                Style::default()
                    .fg(theme.footer_orange)
                    .bg(theme.footer_background)
                    .add_modifier(Modifier::BOLD),
            );
        }
        Some(FooterFlash::CopyFeedback { lines, rect }) => {
            render_copy_feedback(buffer, theme, rect.x, area.y, rect.right(), *lines);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };

    use crate::{
        config::UiTheme,
        layout::{Node, Tab},
        tui::{ChordMode, MultiPaneTui, render_multi_pane, test_support::layout},
    };

    use super::{FooterSegment, chord_footer_badge, contextual_footer};

    #[test]
    fn shared_footer_uses_help_for_each_chord_mode() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        for (mode, expected) in [
            (
                ChordMode::None,
                "Ctrl+ <p> PANE   <t> TAB   <a> AGENTS   <s> SHARE   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS",
            ),
            (
                ChordMode::Pane,
                "PANE MODE  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <k> LOCK   <L> LOCK SESSION   <Esc> BACK",
            ),
            (
                ChordMode::Tab,
                "TAB MODE  <←→> SWITCH   <e> RENAME   <n> NEW   <x> CLOSE   <Esc> BACK",
            ),
        ] {
            tui.chord_mode = mode;
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = (0..120)
                .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
                .collect::<String>();
            assert!(
                footer.starts_with(expected),
                "mode: {mode:?}, footer: {footer}"
            );
        }
    }

    #[test]
    fn shared_footer_accents_key_glyphs_on_a_dark_background() {
        let theme = UiTheme::default();
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        for mode in [ChordMode::None, ChordMode::Pane, ChordMode::Tab] {
            tui.chord_mode = mode;
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = terminal.backend().buffer();
            assert_eq!(footer[(0, 3)].bg, Color::Rgb(30, 30, 30));
            let segments = contextual_footer(mode, 120);
            let mut x = 0;
            if let Some(badge) = chord_footer_badge(mode, 120) {
                for character in badge.chars() {
                    assert_eq!(footer[(x, 3)].fg, theme.footer_orange);
                    assert_eq!(footer[(x, 3)].bg, theme.footer_background);
                    assert!(footer[(x, 3)].modifier.contains(Modifier::BOLD));
                    assert_eq!(footer[(x, 3)].symbol(), character.to_string());
                    x += 1;
                }
            }
            for segment in segments {
                let (text, fg, bg, bold) = match segment {
                    FooterSegment::Text(text) => {
                        (*text, theme.footer_muted, theme.footer_background, false)
                    }
                    FooterSegment::Key(text) => {
                        (*text, theme.footer_accent, theme.footer_background, true)
                    }
                    FooterSegment::OrangeKey(text) => {
                        (*text, theme.footer_orange, theme.footer_background, true)
                    }
                };
                for key in text.chars() {
                    assert_eq!(footer[(x, 3)].fg, fg, "mode: {mode:?}, key: {key}");
                    assert_eq!(footer[(x, 3)].bg, bg, "mode: {mode:?}, key: {key}");
                    assert_eq!(
                        footer[(x, 3)].modifier.contains(Modifier::BOLD),
                        bold,
                        "mode: {mode:?}, key: {key}"
                    );
                    x += 1;
                }
            }
        }
    }

    #[test]
    fn chord_footer_badges_fall_back_atomically_on_narrow_terminals() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");

        for (mode, width, expected) in [
            (ChordMode::Pane, 9, "PANE MODE"),
            (ChordMode::Pane, 8, "PANE"),
            (ChordMode::Pane, 4, "PANE"),
            (ChordMode::Pane, 3, ""),
            (ChordMode::Tab, 8, "TAB MODE"),
            (ChordMode::Tab, 7, "TAB"),
        ] {
            let mut tui = tui.clone();
            tui.chord_mode = mode;
            let mut terminal = Terminal::new(TestBackend::new(width, 4)).expect("test terminal");
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = (0..width)
                .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
                .collect::<String>();

            assert!(
                footer.starts_with(expected),
                "mode: {mode:?}, footer: {footer}"
            );
            assert!(
                !footer.starts_with("PANE MO") || footer.starts_with("PANE MODE"),
                "footer: {footer}"
            );
            assert!(
                !footer.starts_with("TAB MOD") || footer.starts_with("TAB MODE"),
                "footer: {footer}"
            );
        }
    }
}
