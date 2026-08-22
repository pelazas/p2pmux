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

#[derive(Debug, PartialEq)]
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
    FooterSegment::Key("o"),
    FooterSegment::Text("> INBOX   <"),
    FooterSegment::Key("s"),
    FooterSegment::Text("> SHARE   <"),
    FooterSegment::Key("q"),
    // The arrows hang off the `Ctrl` this bar already opens with, which is
    // both what they now do and four words shorter than spelling out the
    // Option+Shift form that still works.
    FooterSegment::Text("> QUIT   <"),
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
    FooterSegment::Key("o"),
    FooterSegment::Text("> INBOX   <"),
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
    FooterSegment::Key("o"),
    FooterSegment::Text("> INBOX   <"),
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
///
/// Below the floor is nothing at all, which is the last tier and a real one.
/// A notice is placed first and the hints are fitted into what it leaves, so a
/// long enough one leaves less than the floor's seventeen columns — and the
/// floor was being drawn into that space anyway and clipped where it ran out,
/// ending the bar `Ctrl+ <p`. Half a hint is worse than none: it names a chord
/// that does not exist, on the one line the mux uses to say what its keys are.
const NORMAL_FOOTER_TIERS: &[&[FooterSegment]] = &[
    NORMAL_FOOTER_FULL,
    NORMAL_FOOTER_NO_FOCUS,
    NORMAL_FOOTER_NO_SHARE,
    NORMAL_FOOTER_CORE,
    NORMAL_FOOTER_KEYS_ONLY,
    &[],
];
const PANE_FOOTER_FULL: &[FooterSegment] = &[
    FooterSegment::Text("  <"),
    FooterSegment::Key("←↓↑→"),
    FooterSegment::Text("> FOCUS   <"),
    FooterSegment::Key("e"),
    FooterSegment::Text("> RENAME   <"),
    FooterSegment::Key("n"),
    FooterSegment::Text("> NEW   <"),
    FooterSegment::Key("r/l/d/u"),
    FooterSegment::Text("> SPLIT   <"),
    FooterSegment::Key("z"),
    FooterSegment::Text("> ZOOM   <"),
    FooterSegment::Key("x"),
    FooterSegment::Text("> CLOSE   <"),
    FooterSegment::Key("k"),
    FooterSegment::Text("> LOCK   <"),
    FooterSegment::Key("L"),
    FooterSegment::Text("> SESSION   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> BACK"),
];
/// The session lock is the first thing to go when the bar will not fit: it is
/// the rarest of these keys and the only one with a modal of its own to explain
/// itself. Everything else here is a per-pane action taken constantly.
const PANE_FOOTER_NO_SESSION_LOCK: &[FooterSegment] = &[
    FooterSegment::Text("  <"),
    FooterSegment::Key("←↓↑→"),
    FooterSegment::Text("> FOCUS   <"),
    FooterSegment::Key("e"),
    FooterSegment::Text("> RENAME   <"),
    FooterSegment::Key("n"),
    FooterSegment::Text("> NEW   <"),
    FooterSegment::Key("r/l/d/u"),
    FooterSegment::Text("> SPLIT   <"),
    FooterSegment::Key("z"),
    FooterSegment::Text("> ZOOM   <"),
    FooterSegment::Key("x"),
    FooterSegment::Text("> CLOSE   <"),
    FooterSegment::Key("k"),
    FooterSegment::Text("> LOCK   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> BACK"),
];
const PANE_FOOTER_TIERS: &[&[FooterSegment]] = &[PANE_FOOTER_FULL, PANE_FOOTER_NO_SESSION_LOCK];
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
/// Enter takes the ticket: it is the only identifier that reaches another machine, so it is
/// Enter takes the code: it is what a person can actually relay over a call, and it resolves
/// to the ticket anywhere. The ticket stays one key away for a peer who cannot reach the
/// rendezvous service, or who would rather not depend on it.
pub(in crate::tui) const SHARE_HELP: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> COPY   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TICKET, WORKS OFFLINE   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
/// The same three keys with the explanation dropped, for a panel too narrow to
/// carry it. What `t` copies is a ticket either way; that it works offline is
/// the part a wider window has room to say.
const SHARE_HELP_TERSE: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> COPY   <"),
    FooterSegment::Key("t"),
    FooterSegment::Text("> TICKET   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];

/// The widest of these that the panel can hold.
///
/// A modal's help line is the only thing on screen that says how to leave it,
/// and this one was being clipped: at sixty columns it ended `<Esc> CLOS`, and
/// at fifty the close hint was gone altogether, leaving a panel over the
/// session with no stated way out. So the tiers drop what can be guessed --
/// what `t` copies, then `t` itself -- and never the way out.
pub(in crate::tui) fn share_help(
    has_ticket: bool,
    has_code: bool,
    width: u16,
) -> &'static [FooterSegment] {
    let tiers: &[&[FooterSegment]] = match (has_ticket, has_code) {
        (true, true) => &[
            SHARE_HELP,
            SHARE_HELP_TERSE,
            SHARE_HELP_NO_CODE,
            SHARE_HELP_GUEST,
        ],
        (true, false) => &[SHARE_HELP_NO_CODE, SHARE_HELP_GUEST],
        (false, _) => &[SHARE_HELP_GUEST],
    };
    tiers
        .iter()
        .copied()
        .find(|tier| footer_segments_width(tier) <= width)
        .unwrap_or(SHARE_HELP_GUEST)
}

/// The same rule for the add-machine panel, whose bar is short enough that only
/// a very small window reaches for this -- but the way out is the way out.
pub(in crate::tui) fn add_machine_help(has_invite: bool, width: u16) -> &'static [FooterSegment] {
    let tiers: &[&[FooterSegment]] = if has_invite {
        &[ADD_MACHINE_HELP, SHARE_HELP_GUEST]
    } else {
        &[SHARE_HELP_GUEST]
    };
    tiers
        .iter()
        .copied()
        .find(|tier| footer_segments_width(tier) <= width)
        .unwrap_or(SHARE_HELP_GUEST)
}

/// When the rendezvous was unreachable there is no code to offer, so Enter falls back to the
/// ticket and `t` would be the same key twice. Say COPY rather than naming either one.
pub(in crate::tui) const SHARE_HELP_NO_CODE: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Enter"),
    FooterSegment::Text("> COPY   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
pub(in crate::tui) const SHARE_HELP_GUEST: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
/// The add-machine panel offers one invite and one thing to do with it, so
/// there is no second key to name.
pub(in crate::tui) const ADD_MACHINE_HELP: &[FooterSegment] = &[
    FooterSegment::Text("<"),
    FooterSegment::Key("c"),
    FooterSegment::Text("> COPY   <"),
    FooterSegment::Key("Esc"),
    FooterSegment::Text("> CLOSE"),
];
/// The help segments for a chord mode, narrowed to the widest tier `width` can hold.
///
/// Pane mode has tiers because its bar is the longest one here and adding a key
/// to it pushed `Esc BACK` off the end of a 120-column terminal — clipped
/// mid-word, which teaches a binding that does not exist. Tab mode is short
/// enough not to need them.
pub(in crate::tui) fn contextual_footer(
    chord_mode: ChordMode,
    width: u16,
) -> &'static [FooterSegment] {
    match chord_mode {
        // The last tier is empty and therefore always fits, so the `unwrap_or`
        // the other arms need is unreachable here by construction.
        ChordMode::None => NORMAL_FOOTER_TIERS
            .iter()
            .copied()
            .find(|tier| footer_segments_width(tier) <= width)
            .unwrap_or(&[]),
        ChordMode::Pane => PANE_FOOTER_TIERS
            .iter()
            .copied()
            .find(|tier| footer_segments_width(tier) <= width)
            .unwrap_or(PANE_FOOTER_NO_SESSION_LOCK),
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
pub(in crate::tui) fn footer_segments_width(segments: &[FooterSegment]) -> u16 {
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

    use super::{
        FooterSegment, add_machine_help, chord_footer_badge, contextual_footer, footer_layout,
        footer_segments_width, share_help,
    };

    /// The help bar is a whole tier or it is nothing.
    ///
    /// A notice is laid out first and the hints are fitted into the columns it
    /// leaves. `contextual_footer` used to answer a width it could not honour —
    /// falling back to the narrowest tier whether or not that tier fit — and the
    /// renderer then clipped it wherever the room ran out. The reported case was
    /// a 90-character scrollback notice on a 99-column terminal, which left the
    /// bar reading `Ctrl+ <p`: a chord that does not exist, on the one line that
    /// exists to say what the chords are.
    ///
    /// Swept across every width a terminal can be rather than checked at the one
    /// that was reported, because which tier is on the edge of fitting changes
    /// with the notice, and the next notice will be a different length.
    #[test]
    fn the_help_bar_never_renders_a_tier_too_wide_for_its_span() {
        let notices = [
            "",
            "that scrollback expired — scroll again",
            "a full-screen program owns this pane",
            "that pane's history is on another machine",
            "coordinator unreachable; panes still live, structure frozen",
            "local scrollback is unavailable for this pane (remote, alternate screen, or stale history)",
        ];
        for width in 1u16..=200 {
            for notice in notices {
                for status in ["", "relayed"] {
                    let area = ratatui::layout::Rect::new(0, 0, width, 1);
                    let notice = (!notice.is_empty()).then_some(notice);
                    let layout = footer_layout(area, ChordMode::None, status, notice, None);
                    let span = layout.help_end.saturating_sub(layout.help_start);
                    assert!(
                        footer_segments_width(layout.help) <= span,
                        "width {width} notice {notice:?} status {status:?}: tier needs {} \
                         columns and has {span}",
                        footer_segments_width(layout.help),
                    );
                }
            }
        }
    }

    /// The reported geometry, drawn: a 99-column terminal carrying a notice too
    /// long to leave room for even the narrowest tier.
    ///
    /// What the daily run of 17 Aug saw was `Ctrl+ <p  local scrollback is …`.
    /// The keybinding half is either whole or absent; there is no width at which
    /// it is a prefix of itself.
    #[test]
    fn a_notice_too_long_to_share_the_line_takes_it_rather_than_cutting_a_chord() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(99, 4)).expect("test terminal");
        terminal
            .draw(|frame| {
                crate::tui::render::panes::render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "",
                    None,
                    Some(
                        "local scrollback is unavailable for this pane \
                         (remote, alternate screen, or stale history)",
                    ),
                    crate::tui::ShareView::default(),
                    None,
                );
            })
            .expect("render");
        let footer = (0..99)
            .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
            .collect::<String>();
        assert!(
            footer.contains("local scrollback is unavailable"),
            "the notice is what the line is for: {footer:?}"
        );
        assert!(
            !footer.contains("Ctrl"),
            "a chord that does not fit is dropped whole, not cut: {footer:?}"
        );
    }

    /// A modal always says how to leave it.
    ///
    /// The share panel's help line was clipped by the panel border: at sixty
    /// columns it ended `<Esc> CLOS`, and at fifty the close hint was gone
    /// entirely, leaving a panel over the session with no stated way out.
    #[test]
    fn the_share_panel_never_drops_the_way_out() {
        for width in [12u16, 20, 26, 30, 40, 44, 54, 80] {
            let chosen = share_help(true, true, width);
            assert!(
                chosen
                    .iter()
                    .any(|segment| matches!(segment, FooterSegment::Key("Esc"))),
                "at {width} columns the panel stopped saying how to close it: {chosen:?}"
            );
            assert!(
                footer_segments_width(chosen) <= width || width < 11,
                "at {width} columns it drew {}: {chosen:?}",
                footer_segments_width(chosen)
            );
        }

        // The add-machine panel follows the same rule.
        for width in [12u16, 20, 22, 40] {
            let chosen = add_machine_help(true, width);
            assert!(
                chosen
                    .iter()
                    .any(|segment| matches!(segment, FooterSegment::Key("Esc"))),
                "at {width} columns add-machine stopped saying how to close: {chosen:?}"
            );
        }

        // Room for everything means everything is said.
        let roomy = share_help(true, true, 80);
        assert!(
            roomy.iter().any(|segment| matches!(
                segment,
                FooterSegment::Text("> TICKET, WORKS OFFLINE   <")
            )),
            "a wide panel keeps the explanation: {roomy:?}"
        );
    }

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
                "Ctrl+ <p> PANE   <t> TAB   <o> INBOX   <s> SHARE   <q> QUIT   <↑↓←→> FOCUS",
            ),
            (
                // 120 columns cannot hold the session lock as well, so the tier
                // below it is what renders here. See `contextual_footer`.
                ChordMode::Pane,
                "PANE MODE  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <z> ZOOM   <x> CLOSE   <k> LOCK   <Esc> BACK",
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
            // The tier is chosen from what is left after the badge, so asking
            // for 120 here would compare against a bar that was never drawn.
            let badge_width = chord_footer_badge(mode, 120).map_or(0, crate::tui::text::text_width);
            let segments = contextual_footer(mode, 120 - badge_width);
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
