//! The pane grid and the chrome around it: the brand and tab strip, each
//! pane's title and border, and the shared multi-pane frame.

use std::collections::BTreeMap;

use unicode_width::UnicodeWidthStr;

use ratatui::{
    Frame,
    layout::Alignment,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

use crate::{
    config::UiTheme,
    layout::PaneId,
    tui::{
        ChordMode, ModalState, MultiPaneTui, ShareView,
        app::{member_color, member_initial},
        geometry::{fixed_grid_viewport, pane_content_rect, visible_leaf_panes},
        member_label,
        render::{
            footer::render_contextual_footer,
            home::render_home,
            modals::{
                render_delete_tab_confirmation, render_quit_prompt, render_rename_prompt,
                render_share_modal,
            },
            vt::{VtScreen, viewed_screen},
        },
        text::{text_width, truncate_trailing},
    },
};

pub(in crate::tui) const TOP_BAR_BRAND: &str = "p2pmux";
pub(in crate::tui) const TOP_BAR_BRAND_SEPARATOR: &str = " │ ";
pub(in crate::tui) const TAB_BAR_SEPARATOR: &str = " · ";
/// The widest the session label is allowed to be before the bar starts cutting
/// it.
///
/// The truncation order across the whole bar, decided once here rather than at
/// tab seven: the session label gives way first, then tab names, and the inbox
/// badge never gives way at all. You already know which session you are in; the
/// badge is the one thing on the bar that is telling you something you do not.
pub(in crate::tui) const TOP_BAR_TITLE_MAX_WIDTH: usize = 22;
/// The inbox segment, between the session label and the tabs.
///
/// Fixed width so the separator after it does not shuffle sideways every time
/// an agent gets blocked or unblocked — a bar that twitches is a bar the eye
/// stops trusting.
const INBOX_SEGMENT_MIN_WIDTH: usize = 7;

/// `inbox` with its count, or `inbox` alone.
///
/// Never `inbox 0`. Absence is quieter than a zero and means exactly the same
/// thing, and a zero on screen is a number the eye has to read before it can
/// discard it.
///
/// Centred in the fixed cell rather than left-aligned: the width is there to
/// stop the separator twitching, not to pin the word to the left divider, and
/// a label hugging one wall of its own segment reads as a mistake.
pub(in crate::tui) fn inbox_segment(needs_you: usize) -> String {
    let text = if needs_you == 0 {
        String::from("inbox")
    } else {
        format!("inbox {needs_you}")
    };
    format!("{text:^INBOX_SEGMENT_MIN_WIDTH$}")
}

/// The whole width the inbox segment and its trailing separator occupy.
pub(in crate::tui) fn inbox_segment_width(needs_you: usize) -> u16 {
    text_width(&inbox_segment(needs_you)).saturating_add(text_width(TOP_BAR_BRAND_SEPARATOR))
}
pub(in crate::tui) fn tab_label(
    title: Option<&str>,
    index: usize,
    active: bool,
    unread: bool,
) -> String {
    let label = title.map_or_else(|| format!("Tab #{index}"), str::to_owned);
    let label = if unread { format!("* {label}") } else { label };
    if active { format!(" {label} ") } else { label }
}
/// A dot per other member on the tab: a separator space, then one cell each.
///
/// Measured by `tab_label_rects` and drawn by the renderer from this one function, so a
/// tab's click target can never drift from what is on screen.
pub(in crate::tui) fn tab_presence_width(watchers: usize) -> u16 {
    match u16::try_from(watchers) {
        Ok(0) => 0,
        Ok(watchers) => watchers.saturating_add(1),
        Err(_) => 0,
    }
}

pub(in crate::tui) const PRESENCE_WATCHING: &str = "●";

/// Right-aligned chips for the members watching a pane, one initial each.
///
/// This lives on the bottom border because the top one is already carrying
/// `host: … control: …` and a right-aligned `(locked by …)` badge. Watchers never reach
/// the border color: that is reserved for the one member holding the pane (see
/// [`pane_border_color`]), and a pane can have several watchers at once, which a single
/// color could not express anyway.
///
/// The member holding the control lease is drawn reversed rather than in a second glyph,
/// so "someone is watching" and "someone can type here" differ without costing a column.
pub(in crate::tui) fn pane_presence_chips(
    watchers: &[&crate::local_ipc::PresenceRow],
    controller_peer_id: Option<&[u8]>,
    members: &[crate::layout::Member],
    theme: &UiTheme,
    available_width: usize,
) -> Option<Line<'static>> {
    if watchers.is_empty() || available_width < 3 {
        return None;
    }
    let mut spans = Vec::new();
    let mut width = 0;
    for watcher in watchers {
        if width + 2 > available_width.saturating_sub(1) {
            break;
        }
        let color = member_color(&watcher.peer_id, members, theme).unwrap_or(theme.footer_muted);
        let controlling =
            controller_peer_id.is_some_and(|controller| controller == watcher.peer_id);
        let style = if controlling {
            Style::default().fg(theme.footer_background).bg(color)
        } else {
            Style::default().fg(color)
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            member_initial(&watcher.peer_id, members).to_string(),
            style,
        ));
        width += 2;
    }
    if spans.is_empty() {
        return None;
    }
    spans.push(Span::raw(" "));
    Some(Line::from(spans).alignment(Alignment::Right))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::tui) fn pane_title(
    custom_title: Option<&str>,
    index: usize,
    host_peer_id: &[u8],
    controller_peer_id: Option<&[u8]>,
    locked: bool,
    exited: bool,
    members: &[crate::layout::Member],
    available_width: usize,
) -> Line<'static> {
    let control = match controller_peer_id {
        Some([]) => "free".to_owned(),
        Some(peer_id) => member_label(peer_id, members),
        None => "…".to_owned(),
    };
    let label = truncate_trailing(
        &custom_title.map_or_else(|| format!("Pane #{index}"), str::to_owned),
        available_width,
    );
    let control = if exited {
        "exited"
    } else if locked {
        "host-only"
    } else {
        &control
    };
    let metadata = format!(
        " host: {} control: {control}",
        member_label(host_peer_id, members)
    );
    let metadata_width = available_width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
    Line::from(vec![
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(truncate_trailing(&metadata, metadata_width)),
    ])
}
/// The border color for one pane, from this client's point of view.
///
/// A held pane is drawn in its controller's member color, the same color that member
/// gets on every tab dot and presence marker. Control is authoritative shared state, so
/// every client colors that pane identically and the border answers *who is driving
/// this* rather than only *somebody is*. Focus is local and stays neutral: white when
/// the pane is free, since your own focus means nothing to anyone else's screen.
///
/// [`UiTheme::pane_border_remote_control`] is the fallback for a controller who is no
/// longer in the member list -- a peer that left mid-hold has no slot and so no color,
/// and the pane still has to read as held.
pub(in crate::tui) fn pane_border_color(
    theme: &UiTheme,
    members: &[crate::layout::Member],
    exited: bool,
    controller_peer_id: Option<&[u8]>,
    focused: bool,
    hovered: bool,
    chord_mode: ChordMode,
) -> Color {
    if exited {
        return theme.pane_border_idle;
    }
    if focused && chord_mode == ChordMode::Pane {
        return theme.pane_border_chord_focused;
    }

    match controller_peer_id {
        Some([]) if focused => theme.pane_border_free_focused,
        None if focused => theme.pane_border_unknown_focused,
        Some([]) | None if hovered => theme.pane_border_hovered,
        Some([]) | None => theme.pane_border_idle,
        Some(peer_id) => {
            member_color(peer_id, members, theme).unwrap_or(theme.pane_border_remote_control)
        }
    }
}
/// Renders layout chrome plus any currently available fixed-size VT screens.
pub fn render_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
) {
    render_shared_multi_pane(
        frame,
        tui,
        screens,
        "",
        None,
        None,
        ShareView::default(),
        None,
    );
}
/// Renders the local attachment footer with its own copy feedback.
#[allow(clippy::too_many_arguments)]
pub fn render_multi_pane_with_copy_feedback(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
    copied_lines: Option<usize>,
    footer_notice: Option<&str>,
    share: ShareView<'_>,
    local_peer_id: Option<&[u8]>,
    link: Option<&str>,
) {
    let exited_notice = tui
        .snapshot()
        .panes
        .get(&tui.focused_pane())
        .filter(|pane| pane.exited)
        .map(|pane| {
            if local_peer_id == Some(pane.host_peer_id.as_slice()) {
                "exited — close with Ctrl+P, X"
            } else {
                "exited — input disabled; pane host can close with Ctrl+P, X"
            }
        });
    render_shared_multi_pane(
        frame,
        tui,
        screens,
        "",
        copied_lines,
        footer_notice.or(exited_notice),
        share,
        link,
    );
}
#[allow(clippy::too_many_arguments)]
pub(in crate::tui) fn render_shared_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
    status: &str,
    copied_lines: Option<usize>,
    footer_notice: Option<&str>,
    share: ShareView<'_>,
    link: Option<&str>,
) {
    let theme = &tui.theme;
    let geometry = tui.geometry(frame.area());
    if geometry.tab_bar.width > 0 && geometry.tab_bar.height > 0 {
        let buffer = frame.buffer_mut();
        buffer.set_stringn(
            geometry.tab_bar.x,
            geometry.tab_bar.y,
            " ".repeat(usize::from(geometry.tab_bar.width)),
            usize::from(geometry.tab_bar.width),
            Style::default().bg(theme.footer_background),
        );
        let mut x = buffer
            .set_stringn(
                geometry.tab_bar.x,
                geometry.tab_bar.y,
                truncate_trailing(tui.title(), TOP_BAR_TITLE_MAX_WIDTH),
                usize::from(geometry.tab_bar.width),
                Style::default()
                    .fg(theme.tab_foreground)
                    .bg(theme.footer_background),
            )
            .0;
        x = buffer
            .set_stringn(
                x,
                geometry.tab_bar.y,
                TOP_BAR_BRAND_SEPARATOR,
                usize::from(geometry.tab_bar.right().saturating_sub(x)),
                Style::default()
                    .fg(theme.tab_separator)
                    .bg(theme.footer_background),
            )
            .0;
        // The inbox badge. It lives in the tab bar for one reason: the count
        // stays visible while you are deep inside a terminal. An ambient alert,
        // with no notification system behind it yet.
        //
        // Two colors doing two different jobs. Amber says "this wants you"; the
        // active-tab red says "you are looking at this". One color for both
        // would make being *on* Home indistinguishable from being *called* to
        // it, and the count would keep shouting at someone already reading it —
        // so once Home is focused the number drops amber and takes the tab
        // label's own treatment.
        let needs_you = tui.home_needs_you_count();
        let inbox_style = if tui.home_open() {
            Style::default()
                .fg(theme.tab_foreground)
                .bg(theme.tab_active_background)
                .add_modifier(Modifier::BOLD)
        } else if needs_you > 0 {
            Style::default()
                .fg(theme.agent_overlay_attention)
                .bg(theme.footer_background)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.tab_foreground)
                .bg(theme.footer_background)
        };
        x = buffer
            .set_stringn(
                x,
                geometry.tab_bar.y,
                inbox_segment(needs_you),
                usize::from(geometry.tab_bar.right().saturating_sub(x)),
                inbox_style,
            )
            .0;
        x = buffer
            .set_stringn(
                x,
                geometry.tab_bar.y,
                TOP_BAR_BRAND_SEPARATOR,
                usize::from(geometry.tab_bar.right().saturating_sub(x)),
                Style::default()
                    .fg(theme.tab_separator)
                    .bg(theme.footer_background),
            )
            .0;
        for (index, tab) in tui.snapshot.tabs.iter().enumerate() {
            if index > 0 {
                x = buffer
                    .set_stringn(
                        x,
                        geometry.tab_bar.y,
                        TAB_BAR_SEPARATOR,
                        usize::from(geometry.tab_bar.right().saturating_sub(x)),
                        Style::default()
                            .fg(theme.tab_separator)
                            .bg(theme.footer_background),
                    )
                    .0;
            }
            let active = tui.is_active_tab(tab.tab_id);
            let style = if active {
                Style::default()
                    .fg(theme.tab_foreground)
                    .bg(theme.tab_active_background)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(if tui.chord_mode == ChordMode::Tab {
                        theme.tab_separator
                    } else {
                        theme.tab_foreground
                    })
                    .bg(theme.footer_background)
            };
            let label_rect = geometry.tab_labels[&tab.tab_id];
            if label_rect.width == 0 {
                continue;
            }
            // Dots first: they are the whole point of glancing at a tab you are not on,
            // so when the bar runs out of room the tab name gives way, not the presence.
            let watchers = tui.tab_watchers(tab.tab_id);
            let presence_width = tab_presence_width(watchers.len()).min(label_rect.width);
            let name_width = label_rect.width.saturating_sub(presence_width);
            let label = truncate_trailing(
                &tab_label(
                    tab.title.as_deref(),
                    index + 1,
                    active,
                    tui.tab_has_unread_agent_pane(tab),
                ),
                usize::from(name_width),
            );
            let (mut cursor, _) = buffer.set_stringn(
                label_rect.x,
                label_rect.y,
                &label,
                usize::from(name_width),
                style,
            );
            if presence_width > 0 {
                cursor = buffer.set_stringn(cursor, label_rect.y, " ", 1, style).0;
                for watcher in watchers
                    .iter()
                    .take(usize::from(presence_width.saturating_sub(1)))
                {
                    let color = member_color(&watcher.peer_id, &tui.snapshot.members, theme)
                        .unwrap_or(theme.tab_foreground);
                    cursor = buffer
                        .set_stringn(cursor, label_rect.y, PRESENCE_WATCHING, 1, style.fg(color))
                        .0;
                }
            }
            x = x
                .saturating_add(text_width(&label))
                .saturating_add(presence_width);
        }
        // `direct 35ms` / `relayed 120ms ×3`, right-aligned. Lives in the tab bar rather
        // than the footer because the footer is transient -- chords, copy feedback and
        // status notices all take it over -- and connectivity is exactly the fact you
        // want on screen at the moment something else has gone wrong.
        let badge = match (tui.session_locked, link.filter(|link| !link.is_empty())) {
            (true, Some(link)) => Some(format!("locked · {link}")),
            (true, None) => Some(String::from("locked")),
            (false, link) => link.map(str::to_owned),
        };
        if let Some(link) = badge.as_deref() {
            let width = text_width(link);
            let start = geometry.tab_bar.right().saturating_sub(width);
            // Never overwrite a tab label: if the tabs already reach that far, the tab
            // names are the more important thing and the badge simply stands down.
            if width > 0 && start > x {
                buffer.set_stringn(
                    start,
                    geometry.tab_bar.y,
                    link,
                    usize::from(width),
                    Style::default()
                        .fg(theme.tab_separator)
                        .bg(theme.footer_background),
                );
            }
        }
    }
    // Home replaces the content area and the footer wholesale. It is not an
    // overlay: there is no session chrome underneath it to see around, and
    // drawing panes behind a screen that covers them is wasted work on every
    // frame.
    if tui.home_open() {
        render_home(frame, tui, crate::tui::clock::unix_ms_now());
        // Home replaces the session chrome but not the question Ctrl+Q asks:
        // `q` opens the same prompt from here, so it has to be drawn here too.
        if tui.quit_open() {
            render_quit_prompt(frame, theme);
        }
        return;
    }

    if geometry.footer.width > 0 && geometry.footer.height > 0 {
        render_contextual_footer(
            frame.buffer_mut(),
            theme,
            geometry.footer,
            status,
            copied_lines,
            footer_notice,
            tui.chord_mode,
        );
    }

    let pane_ids = tui
        .current_tab_layout()
        .map(|tab| visible_leaf_panes(&tab.root))
        .unwrap_or_default();
    for (index, pane_id) in pane_ids.into_iter().enumerate() {
        let Some(rect) = geometry.panes.get(&pane_id).copied() else {
            continue;
        };
        let pane = &tui.snapshot.panes[&pane_id];
        let view = tui.pane_views.get(&pane_id).cloned().unwrap_or_default();
        let focused = pane_id == tui.focused_pane;
        let title_width = usize::from(rect.width.saturating_sub(2));
        let lock_badge = (!pane.exited && pane.locked).then(|| {
            format!(
                "(locked by {})",
                member_label(&pane.host_peer_id, &tui.snapshot.members)
            )
        });
        let badge_width = lock_badge
            .as_deref()
            .map_or(0, |badge| UnicodeWidthStr::width(badge).min(title_width));
        let mut title = pane_title(
            pane.title.as_deref(),
            index + 1,
            &pane.host_peer_id,
            view.controller_peer_id.as_deref(),
            pane.locked,
            pane.exited,
            &tui.snapshot.members,
            title_width.saturating_sub(badge_width).saturating_sub(2),
        );
        title.spans.insert(
            0,
            Span::raw(if tui.unread_agent_panes.contains(&pane_id) {
                " * "
            } else {
                " "
            }),
        );
        title.spans.push(Span::raw(" "));
        let border_color = pane_border_color(
            theme,
            &tui.snapshot.members,
            pane.exited,
            view.controller_peer_id.as_deref(),
            focused,
            tui.hovered_pane == Some(pane_id),
            tui.chord_mode,
        );
        let mut block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(border_color));
        if let Some(badge) = lock_badge {
            block = block.title(
                Line::from(truncate_trailing(&badge, badge_width)).alignment(Alignment::Right),
            );
        }
        // A zoomed pane looks exactly like a tab with one pane in it, so
        // without a mark there is no way to tell whether the siblings are
        // hidden or were never there. Bottom-left, because the top border is
        // already carrying the metadata and a right-aligned lock badge, and a
        // pane can be locked and zoomed at once.
        if tui.zoomed_pane() == Some(pane_id) {
            block = block.title_bottom(
                Line::styled(" zoom ", Style::default().fg(theme.footer_accent))
                    .alignment(Alignment::Left),
            );
        }
        if let Some(chips) = pane_presence_chips(
            &tui.pane_watchers(pane_id),
            view.controller_peer_id.as_deref(),
            &tui.snapshot.members,
            theme,
            title_width,
        ) {
            block = block.title_bottom(chips);
        }
        let content = pane_content_rect(rect);
        frame.render_widget(block, rect);
        // The fixed VT grid may be smaller than this pane after layout reflow. Clear the full
        // interior before drawing it so letterbox margins cannot retain cells from an older pane.
        frame.render_widget(Clear, content);
        if let Some(screen) = screens.get(&pane_id) {
            let viewport = fixed_grid_viewport(content, pane.grid_rows, pane.grid_cols);
            let screen = viewed_screen(screen, tui.scrollback_offset(pane_id));
            frame.render_widget(
                VtScreen::new(screen.as_ref()).with_selection(
                    tui.selection()
                        .filter(|selection| selection.pane_id == pane_id),
                ),
                viewport,
            );
            let (row, col) = screen.cursor_position();
            if focused
                && view.ready
                && screen.scrollback() == 0
                && !screen.hide_cursor()
                && !pane.exited
                && row < viewport.height
                && col < viewport.width
            {
                frame.set_cursor_position((
                    viewport.x.saturating_add(col),
                    viewport.y.saturating_add(row),
                ));
            }
        } else if !view.ready {
            frame.render_widget(Paragraph::new("waiting for pane snapshot/lease"), content);
        }
    }
    if tui.share_open() {
        render_share_modal(frame, &tui.theme, share);
    }
    if let ModalState::Rename(prompt) = &tui.modal {
        render_rename_prompt(frame, prompt, &tui.theme);
    }
    if let ModalState::ConfirmDeleteTab { pane_count, .. } = &tui.modal {
        render_delete_tab_confirmation(frame, *pane_count, &tui.theme);
    }
    if tui.quit_open() {
        render_quit_prompt(frame, &tui.theme);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Color, Modifier},
    };

    use crate::{
        config::UiTheme,
        layout::{Axis, Node, Tab},
        tui::{
            ChordMode, MultiPaneTui, PaneViewState, ShareView,
            geometry::visible_leaf_panes,
            test_support::{
                home_tui, layout, named_members, split_layout, two_tab_presence_tui, watcher,
            },
            text::text_width,
        },
    };

    use super::{
        PRESENCE_WATCHING, inbox_segment, member_color, pane_border_color, pane_presence_chips,
        pane_title, render_multi_pane, render_shared_multi_pane,
    };

    #[test]
    fn shared_footer_places_rejection_notice_after_help() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "",
                    None,
                    Some("layout request 5 rejected"),
                    ShareView::default(),
                    None,
                );
            })
            .expect("draw");
        let footer = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        let help = footer.find("QUIT").expect("help rendered");
        let notice = footer
            .find("layout request 5 rejected")
            .expect("rejection notice rendered");
        assert!(help < notice, "notice sits after helper text");
        assert_eq!(
            terminal.backend().buffer()[(notice as u16, 4)].fg,
            UiTheme::default().footer_orange
        );
        assert!(
            !footer.trim_start().starts_with("layout request"),
            "rejection no longer occupies the left status slot: {footer}"
        );
    }

    #[test]
    fn shared_renderer_keeps_join_commands_out_of_the_footer() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "waiting for current pane reservation",
                    None,
                    None,
                    ShareView {
                        code: Some("TESTCODE"),
                        ticket: Some("p2pmux-v1:TICKET"),
                        notice: None,
                    },
                    None,
                );
            })
            .expect("draw");
        let footer = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        assert!(footer.contains("waiting for current pane reservation"));
        // Invite material lives behind Ctrl+S; a same-Mac-only command in the corner read as
        // something a peer could run, and it never was.
        assert!(!footer.contains("p2pmux join"));
        assert!(!footer.contains("TESTCODE"));
        assert!(footer.contains("SHARE"));
    }

    #[test]
    fn shared_footer_places_copy_feedback_after_quit_with_a_red_line_count() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "",
                    Some(3),
                    None,
                    ShareView::default(),
                    None,
                );
            })
            .expect("draw");
        let footer = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        let quit = footer.find("> QUIT").expect("quit help rendered");
        let copied = footer
            .find("copied 3 lines")
            .expect("copy feedback rendered");
        let count = footer.find("3 lines").expect("line count rendered");

        assert!(copied > quit + "> QUIT".len());
        assert_eq!(
            terminal.backend().buffer()[(text_width(&footer[..copied]), 4)].fg,
            Color::White
        );
        assert_eq!(
            terminal.backend().buffer()[(text_width(&footer[..count]), 4)].fg,
            Color::Rgb(255, 69, 0)
        );
    }

    #[test]
    fn pane_title_uses_stable_leaf_order_and_control_state() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 8 }),
                    second: Box::new(Node::Leaf { pane_id: 3 }),
                },

                title: None,
            }],
            &[(3, 1, 1), (8, 1, 1)],
        );
        let mut members = snapshot.members.clone();
        members[0].display_name = String::from("Host");
        members.push(crate::layout::Member {
            peer_id: b"guest".to_vec(),
            endpoint_addr: b"guest-endpoint".to_vec(),
            display_name: String::from("Guest"),
        });

        assert_eq!(visible_leaf_panes(&snapshot.tabs[0].root), vec![8, 3]);
        assert_eq!(
            pane_title(None, 1, b"host", Some(b""), false, false, &members, 80).to_string(),
            "Pane #1 host: Host control: free"
        );
        assert_eq!(
            pane_title(None, 2, b"host", Some(b"guest"), false, false, &members, 80).to_string(),
            "Pane #2 host: Host control: Guest"
        );
        assert_eq!(
            pane_title(None, 2, b"host", None, false, false, &members, 80).to_string(),
            "Pane #2 host: Host control: …"
        );
        assert_eq!(
            pane_title(None, 2, b"host", Some(b"guest"), true, false, &members, 80).to_string(),
            "Pane #2 host: Host control: host-only"
        );
        assert_eq!(
            pane_title(None, 2, b"host", Some(b"guest"), true, true, &members, 80).to_string(),
            "Pane #2 host: Host control: exited"
        );
    }

    #[test]
    fn title_chrome_uses_custom_labels_and_cell_width_ellipsis() {
        assert_eq!(
            super::tab_label(Some("build logs"), 1, false, false),
            "build logs"
        );
        assert_eq!(super::tab_label(None, 2, false, false), "Tab #2");
        assert_eq!(
            super::tab_label(Some("build logs"), 1, false, true),
            "* build logs"
        );
        assert_eq!(super::truncate_trailing("界界", 3), "界…");
        assert_eq!(super::truncate_trailing("界", 1), "…");
        assert_eq!(super::truncate_trailing("title", 0), "");

        let members = vec![crate::layout::Member {
            peer_id: b"host".to_vec(),
            endpoint_addr: b"endpoint".to_vec(),
            display_name: String::from("Host"),
        }];
        assert_eq!(
            pane_title(
                Some("build logs"),
                1,
                b"host",
                Some(b""),
                false,
                false,
                &members,
                12
            )
            .to_string(),
            "build logs …"
        );
    }

    #[test]
    fn unread_agent_badges_use_the_same_starred_tab_label_for_hit_testing() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: Some(String::from("build")),
            }],
            &[(1, 2, 8), (2, 2, 8)],
        ))
        .expect("valid layout");
        tui.unread_agent_panes.insert(2);
        let area = Rect::new(0, 0, 80, 8);
        let geometry = tui.geometry(area);
        assert_eq!(
            geometry.tab_labels[&1].width,
            text_width(&super::tab_label(Some("build"), 1, true, true))
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test terminal");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let tab_bar = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        let pane_titles = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(tab_bar.contains("* build"));
        assert!(pane_titles.contains("* Pane #2"));
    }

    #[test]
    fn locked_badge_keeps_right_title_space_on_narrow_chrome() {
        let mut snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 2)],
        );
        snapshot.panes.get_mut(&1).expect("pane").locked = true;
        snapshot.members[0].display_name = String::from("Host");
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(18, 4)).expect("terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("draw");
        let chrome = (0..18)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(chrome.contains("locked by Host"), "{chrome}");
        assert!(!chrome.contains("Pane #1"));
    }

    /// A zoomed pane is indistinguishable from a tab with one pane in it, so
    /// without a mark there is no telling whether siblings are hidden or absent.
    #[test]
    fn a_zoomed_pane_says_so_on_its_bottom_border() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("draw");
        let plain = rendered(terminal.backend().buffer(), 60, 12);
        assert!(!plain.contains(" zoom "), "{plain}");

        tui.toggle_zoom();
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("draw");
        let zoomed = rendered(terminal.backend().buffer(), 60, 12);
        assert!(zoomed.contains(" zoom "), "{zoomed}");
    }

    fn rendered(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn free_panes_use_a_mid_gray_border_when_hovered_unfocused() {
        let theme = UiTheme::default();
        let nobody: &[crate::layout::Member] = &[];
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b""),
                true,
                false,
                ChordMode::None
            ),
            Color::White
        );
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b""),
                false,
                true,
                ChordMode::None
            ),
            Color::Gray
        );
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b""),
                false,
                false,
                ChordMode::None
            ),
            Color::DarkGray
        );
        assert_eq!(
            pane_border_color(&theme, nobody, false, None, true, false, ChordMode::None),
            Color::Yellow
        );
        assert_eq!(
            pane_border_color(&theme, nobody, false, None, false, true, ChordMode::None),
            Color::Gray
        );
        assert_eq!(
            pane_border_color(&theme, nobody, false, None, false, false, ChordMode::None),
            Color::DarkGray
        );
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b"guest"),
                true,
                true,
                ChordMode::None
            ),
            Color::Rgb(255, 69, 0)
        );
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b"guest"),
                false,
                true,
                ChordMode::None
            ),
            Color::Rgb(255, 69, 0)
        );

        let themed = UiTheme {
            pane_border_remote_control: Color::Rgb(1, 2, 3),
            ..Default::default()
        };
        assert_eq!(
            pane_border_color(
                &themed,
                nobody,
                false,
                Some(b"guest"),
                false,
                false,
                ChordMode::None
            ),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b"guest"),
                true,
                true,
                ChordMode::Pane
            ),
            theme.pane_border_chord_focused
        );
        assert_eq!(
            pane_border_color(
                &theme,
                nobody,
                false,
                Some(b"guest"),
                true,
                true,
                ChordMode::Tab
            ),
            theme.pane_border_remote_control
        );
    }

    #[test]
    fn a_held_pane_borrows_its_controllers_member_color() {
        let theme = UiTheme::default();
        let members = crate::tui::test_support::presence_members(3);

        // Every member gets their own border, and it is the same color that member is
        // drawn with everywhere else, so the border names the holder without a legend.
        for member in &members {
            let expected =
                member_color(&member.peer_id, &members, &theme).expect("member has a slot");
            assert_ne!(expected, theme.pane_border_remote_control);
            for focused in [false, true] {
                assert_eq!(
                    pane_border_color(
                        &theme,
                        &members,
                        false,
                        Some(&member.peer_id),
                        focused,
                        false,
                        ChordMode::None
                    ),
                    expected,
                );
            }
        }

        // A controller who has left the member list has no slot and so no color; the
        // pane still has to read as held rather than as free.
        assert_eq!(
            pane_border_color(
                &theme,
                &members,
                false,
                Some(b"departed"),
                false,
                false,
                ChordMode::None
            ),
            theme.pane_border_remote_control,
        );
    }

    #[test]
    fn chrome_reports_the_pane_host_and_controller() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: Some(b"peer".to_vec()),
                controller_active: true,
                scrollback: 0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(36, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), "┌");
        assert_eq!(buffer[(1, 1)].symbol(), " ");
        assert_eq!(buffer[(2, 1)].symbol(), "P");
        assert!((2..9).all(|x| buffer[(x, 1)].modifier.contains(Modifier::BOLD)));
        assert!(!buffer[(9, 1)].modifier.contains(Modifier::BOLD));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "h"));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "t"));
    }

    #[test]
    fn pane_badge_uses_the_snapshot_host_not_mutable_view_state() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let title = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();

        assert!(title.contains("host: 686f7374"));
        assert!(!title.contains("host: 66616b65"));
    }

    #[test]
    fn focused_ready_pane_maps_its_visible_vt_cursor_into_the_letterboxed_viewport() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"ab");
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 3)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
            },
        );
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");

        terminal.backend_mut().assert_cursor_position((3, 2));
    }

    #[test]
    fn focused_pane_never_draws_a_hidden_or_clipped_vt_cursor() {
        for sequence in [b"\x1b[?25l".as_slice(), b"abcd".as_slice()] {
            let mut parser = vt100::Parser::new(1, 5, 0);
            parser.process(sequence);
            let mut tui = MultiPaneTui::new(layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                }],
                &[(1, 1, 3)],
            ))
            .expect("valid layout");
            tui.set_pane_view(
                1,
                PaneViewState {
                    ready: true,
                    controller_peer_id: None,
                    controller_active: false,
                    scrollback: 0,
                },
            );
            let screens = BTreeMap::from([(1, parser.screen())]);
            let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &screens))
                .expect("render");

            assert!(!terminal.backend().cursor_visible());
        }
    }

    #[test]
    fn fixed_grid_view_is_top_left_and_clears_letterbox_margins() {
        let mut previous_parser = vt100::Parser::new(3, 6, 0);
        previous_parser.process(b"XXXXXX\r\nYYYYYY\r\nZZZZZZ");
        let previous_tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 3, 6)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(8, 7)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_multi_pane(
                    frame,
                    &previous_tui,
                    &BTreeMap::from([(1, previous_parser.screen())]),
                )
            })
            .expect("initial render");

        let mut parser = vt100::Parser::new(1, 5, 0);
        parser.process(b"abcde");
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 5)],
        ))
        .expect("valid layout");
        let screens = BTreeMap::from([(1, parser.screen())]);

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(1, 2)].symbol(), "a");
        assert_eq!(buffer[(5, 2)].symbol(), "e");
        assert_eq!(buffer[(7, 3)].symbol(), "│");
        for (x, y) in [(6, 2), (1, 3), (6, 3), (1, 4), (6, 4)] {
            assert_eq!(buffer[(x, y)].symbol(), " ", "letterbox cell ({x}, {y})");
        }
    }

    #[test]
    fn chord_mode_chrome_switches_and_restores_remote_focused_pane_colors() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState::from_chrome(true, Some(b"guest".to_vec()), true),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test terminal");

        tui.chord_mode = ChordMode::Pane;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, tui.theme.pane_border_chord_focused);
        assert_eq!(
            (0..9).map(|x| buffer[(x, 7)].symbol()).collect::<String>(),
            "PANE MODE"
        );

        tui.chord_mode = ChordMode::Tab;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, tui.theme.pane_border_remote_control);
        assert_eq!(
            (0..8).map(|x| buffer[(x, 7)].symbol()).collect::<String>(),
            "TAB MODE"
        );

        tui.chord_mode = ChordMode::None;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, tui.theme.pane_border_remote_control);
        assert!(
            (0..4)
                .map(|x| buffer[(x, 7)].symbol())
                .collect::<String>()
                .starts_with("Ctrl")
        );
    }

    #[test]
    fn themed_tui_overrides_footer_chrome() {
        let theme = UiTheme {
            footer_background: Color::Rgb(1, 2, 3),
            footer_orange: Color::Rgb(4, 5, 6),
            ..Default::default()
        };
        let tui = MultiPaneTui::with_theme(
            layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                    title: None,
                }],
                &[(1, 2, 2)],
            ),
            theme,
        )
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let footer = terminal.backend().buffer();
        assert_eq!(footer[(0, 3)].bg, theme.footer_background);
        assert_eq!(footer[(0, 3)].fg, theme.footer_orange);
    }

    #[test]
    fn tab_bar_uses_a_branded_footer_like_strip_and_highlights_the_active_tab() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 10,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 20,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(44, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let tab_bar = (0..44).map(|x| buffer[(x, 0)].symbol()).collect::<String>();

        assert!(
            tab_bar.starts_with("p2pmux │  inbox  │  Tab #1  · Tab #2"),
            "{tab_bar:?}"
        );
        assert!(tab_bar.contains("Tab #1"));
        assert!(tab_bar.contains("Tab #2"));
        assert_eq!(buffer[(0, 0)].fg, Color::White);
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(30, 30, 30));
        assert_eq!(buffer[(19, 0)].fg, Color::White);
        assert_eq!(buffer[(19, 0)].bg, Color::Rgb(220, 50, 47));
        assert_eq!(buffer[(30, 0)].fg, Color::White);
        assert_eq!(buffer[(30, 0)].bg, Color::Rgb(30, 30, 30));

        tui.chord_mode = ChordMode::Tab;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(19, 0)].fg, Color::White);
        assert_eq!(buffer[(19, 0)].bg, Color::Rgb(220, 50, 47));
        assert_eq!(buffer[(30, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(30, 0)].bg, Color::Rgb(30, 30, 30));

        tui.chord_mode = ChordMode::None;
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        assert_eq!(terminal.backend().buffer()[(30, 0)].fg, Color::White);
    }

    /// The badge's two colours do two different jobs, and one colour for both
    /// would make being *on* Home indistinguishable from being *called* to it.
    #[test]
    fn the_inbox_badge_carries_an_amber_count_and_turns_red_only_when_focused() {
        let theme = UiTheme::default();
        let mut tui = home_tui(&[
            (
                "laptop",
                "claude",
                crate::protocol::AgentRosterState::Working,
            ),
            (
                "desktop",
                "codex",
                crate::protocol::AgentRosterState::Pending,
            ),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(44, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let tab_bar = (0..44).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        assert!(tab_bar.starts_with("p2pmux │ inbox 1 │"), "{tab_bar:?}");
        assert_eq!(buffer[(9, 0)].fg, theme.agent_overlay_attention);
        assert_eq!(buffer[(9, 0)].bg, theme.footer_background);

        // Focused: the ambient alert has done its job once you are reading the
        // list, so the number drops amber and takes the tab label's treatment.
        tui.set_home_open(true, "test");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(9, 0)].bg, theme.tab_active_background);
        assert_ne!(buffer[(9, 0)].fg, theme.agent_overlay_attention);
        assert_eq!(
            buffer[(19, 0)].bg,
            theme.footer_background,
            "no tab is the one being looked at while Home is"
        );
    }

    #[test]
    fn the_badge_never_renders_a_zero() {
        // Absence is quieter than a zero and means exactly the same thing.
        assert_eq!(inbox_segment(0), " inbox ");
        assert_eq!(inbox_segment(1), "inbox 1");
        assert_eq!(
            text_width(&inbox_segment(0)),
            text_width(&inbox_segment(9)),
            "a fixed-width segment keeps the separator after it from twitching"
        );
    }

    /// The fixed width is there to stop the separator twitching, not to pin the
    /// word to the left divider.
    #[test]
    fn the_badge_sits_in_the_middle_of_its_segment() {
        for needs_you in [0, 1, 9] {
            let segment = inbox_segment(needs_you);
            let leading = segment.len() - segment.trim_start().len();
            let trailing = segment.len() - segment.trim_end().len();
            assert!(
                leading.abs_diff(trailing) <= 1,
                "{segment:?} is not centred in its cell"
            );
        }
    }

    #[test]
    fn a_tab_shows_a_colored_dot_per_other_member_on_it() {
        let mut tui = two_tab_presence_tui();
        assert!(tui.set_presence(vec![crate::local_ipc::PresenceRow {
            peer_id: b"tis".to_vec(),
            tab_id: 20,
            pane_id: 2,
        }]));
        let mut terminal = Terminal::new(TestBackend::new(52, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let tab_bar = (0..52).map(|x| buffer[(x, 0)].symbol()).collect::<String>();

        assert!(
            tab_bar.starts_with("p2pmux │  inbox  │  Tab #1  · Tab #2 ●"),
            "the dot belongs to the tab that member is on: {tab_bar:?}"
        );
        let dots = (0..52)
            .filter(|x| buffer[(*x, 0)].symbol() == PRESENCE_WATCHING)
            .collect::<Vec<u16>>();
        assert_eq!(dots.len(), 1, "one member on one tab draws one dot");
        assert_eq!(
            buffer[(dots[0], 0)].fg,
            UiTheme::default().member_colors[1],
            "a member's dot uses their own slot color"
        );
    }
    #[test]
    fn presence_dots_stay_inside_the_tabs_click_target() {
        // The tab bar measures click targets from tab_label_rects and draws from the
        // render path. If the dots were added to only one of them, every tab click after
        // a member moved would land on the wrong tab.
        let mut tui = two_tab_presence_tui();
        let area = Rect::new(0, 0, 52, 4);
        let before = tui.geometry(area).tab_labels[&20];

        assert!(tui.set_presence(vec![crate::local_ipc::PresenceRow {
            peer_id: b"tis".to_vec(),
            tab_id: 10,
            pane_id: 1,
        }]));
        let after = tui.geometry(area).tab_labels[&20];

        assert_eq!(
            after.x,
            before.x.saturating_add(2),
            "a dot on tab one shifts tab two's click target by the space plus the dot"
        );

        let mut terminal = Terminal::new(TestBackend::new(52, 4)).expect("test terminal");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let drawn = (after.x..after.right())
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert_eq!(
            drawn, "Tab #2",
            "the click target has to cover exactly the label that was drawn"
        );
    }
    #[test]
    fn a_tab_too_narrow_for_both_keeps_the_dot_and_drops_the_name() {
        let mut tui = two_tab_presence_tui();
        assert!(tui.set_presence(vec![crate::local_ipc::PresenceRow {
            peer_id: b"tis".to_vec(),
            tab_id: 20,
            pane_id: 2,
        }]));
        // Wide enough for the chrome and the active tab, and then almost nothing.
        let mut terminal = Terminal::new(TestBackend::new(36, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let tab_bar = (0..36).map(|x| buffer[(x, 0)].symbol()).collect::<String>();

        assert!(
            tab_bar.contains('●'),
            "presence outranks the tab name when the bar runs out of room: {tab_bar:?}"
        );
    }
    #[test]
    fn pane_chips_mark_the_controller_apart_from_plain_watchers() {
        let theme = UiTheme::default();
        let members = named_members();
        let rows = [watcher(b"tis", 1, 1), watcher(b"ana", 1, 1)];
        let watchers = rows.iter().collect::<Vec<_>>();

        let chips = pane_presence_chips(&watchers, Some(b"ana"), &members, &theme, 40)
            .expect("two watchers draw chips");
        let initials = chips
            .spans
            .iter()
            .filter(|span| span.content.trim() != "")
            .collect::<Vec<_>>();

        assert_eq!(initials.len(), 2);
        assert_eq!(initials[0].content, "T");
        assert_eq!(initials[1].content, "A");
        // Watching is foreground-only; holding the lease reverses the chip. Nothing here
        // may borrow the alert color that means "this pane is under remote control".
        assert_eq!(initials[0].style.fg, Some(theme.member_colors[1]));
        assert_eq!(initials[0].style.bg, None);
        assert_eq!(initials[1].style.bg, Some(theme.member_colors[2]));
        assert_ne!(initials[1].style.bg, Some(theme.pane_border_remote_control));
    }
    #[test]
    fn pane_chips_stand_down_when_the_pane_is_too_narrow() {
        let theme = UiTheme::default();
        let members = named_members();
        let rows = [watcher(b"tis", 1, 1), watcher(b"ana", 1, 1)];
        let watchers = rows.iter().collect::<Vec<_>>();

        assert!(pane_presence_chips(&watchers, None, &members, &theme, 2).is_none());
        let cramped = pane_presence_chips(&watchers, None, &members, &theme, 4)
            .expect("a narrow pane still shows who it can");
        assert_eq!(
            cramped
                .spans
                .iter()
                .filter(|span| span.content.trim() != "")
                .count(),
            1,
            "chips are dropped from the end rather than overflowing the border"
        );
    }
    #[test]
    fn watcher_chips_reach_the_pane_bottom_border() {
        let mut snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 4, 20)],
        );
        snapshot.members = named_members();
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        assert!(tui.set_presence(vec![watcher(b"tis", 1, 1)]));
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let bottom = buffer.area.bottom().saturating_sub(2);
        let border = (0..24)
            .map(|x| buffer[(x, bottom)].symbol())
            .collect::<String>();

        assert!(
            border.contains('T'),
            "the watcher's initial belongs on the bottom border: {border:?}"
        );
        let chip_x = (0..24)
            .find(|x| buffer[(*x, bottom)].symbol() == "T")
            .expect("chip column");
        assert_eq!(
            buffer[(chip_x, bottom)].fg,
            UiTheme::default().member_colors[1]
        );
    }
    #[test]
    fn a_pane_border_keeps_its_meaning_when_somebody_watches_it() {
        // Presence must not repaint the border: it encodes focus and control, and one
        // pane can have several watchers that a single color could not express. A member
        // color on the border means that member *holds* the pane, not that they are
        // looking at it.
        let theme = UiTheme::default();
        let members = crate::tui::test_support::presence_members(2);
        let watched = pane_border_color(
            &theme,
            &members,
            false,
            Some(&[]),
            true,
            false,
            ChordMode::None,
        );

        assert_eq!(watched, theme.pane_border_free_focused);
        assert_ne!(watched, theme.pane_border_remote_control);
    }
}
