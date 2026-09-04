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
        ChordMode, ModalState, MultiPaneTui, PaneViewState, ScreenCell, ShareView,
        app::{member_color, member_initial},
        geometry::{pane_content_rect, visible_leaf_panes},
        member_label,
        render::{
            footer::render_contextual_footer,
            home::render_home,
            modals::{
                render_add_machine_modal, render_delete_tab_confirmation, render_quit_prompt,
                render_remote_work_prompt, render_rename_prompt, render_share_modal,
                render_update_confirm,
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

fn render_clip_indicators(
    frame: &mut Frame<'_>,
    rect: ratatui::layout::Rect,
    viewport: ratatui::layout::Rect,
    grid: (u16, u16),
    origin: ScreenCell,
    style: Style,
    bottom_chrome: bool,
) {
    if bottom_chrome
        || rect.width < 8
        || rect.height < 3
        || viewport.width == 0
        || viewport.height == 0
    {
        return;
    }
    let mut left = String::new();
    if origin.col > 0 {
        left.push('<');
    }
    if origin.row > 0 {
        left.push('^');
    }
    let mut right = String::new();
    if origin.row.saturating_add(viewport.height) < grid.0 {
        right.push('v');
    }
    if origin.col.saturating_add(viewport.width) < grid.1 {
        right.push('>');
    }
    let buffer = frame.buffer_mut();
    let bottom = rect.bottom().saturating_sub(1);
    if !left.is_empty() {
        buffer.set_string(rect.x.saturating_add(1), bottom, left, style);
    }
    if !right.is_empty() {
        buffer.set_string(rect.right().saturating_sub(3), bottom, right, style);
    }
}

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
/// Whether this pane should be drawn at reduced intensity.
///
/// `dim_unfocused_panes` is off unless a config asked for it, so the first
/// question is settled by the user. The second is which panes it means, and
/// `!focused` is the wrong answer: the pane you are *reading* is very often not
/// the pane you are typing into. So a pane still stands at full strength when
///
/// - the pointer is on it. The wheel is aimed by the pointer and not by focus,
///   which is a ruling this program already made; scrolling an unfocused pane
///   to read it and having it fade is that ruling contradicted.
/// - it is parked in its own scrollback. Nobody scrolls back through a pane
///   they are not reading.
/// - *another* peer is driving it. Watching a pane on somebody else's machine
///   is what p2pmux is for, and a spectated pane is by definition unfocused.
///   Our own hold on a lease says nothing: this client takes one to type, so
///   reading it as "somebody is driving this" would exempt every pane the user
///   has ever typed into.
/// - its agent is working, blocked on a human, or has failed. That is the one
///   thing on the screen that must catch the eye, and it lives in a pane the
///   user is not typing into almost by definition.
///
/// What is left is a pane nobody is reading, which is what the setting is for.
/// An exited pane is among them: it is not where the keystrokes are going
/// either, and its border already says it is finished.
fn pane_recedes(
    tui: &MultiPaneTui,
    pane_id: PaneId,
    view: &PaneViewState,
    focused: bool,
    scrollback: usize,
) -> bool {
    if !tui.dim_unfocused_panes || focused {
        return false;
    }
    let driven_by_a_peer = view
        .controller_peer_id
        .as_deref()
        .is_some_and(|controller| {
            !controller.is_empty() && tui.local_peer_id.as_deref() != Some(controller)
        });
    let being_read = tui.hovered_pane == Some(pane_id) || scrollback != 0 || driven_by_a_peer;
    !being_read && !tui.pane_holds_a_live_agent(pane_id)
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
        let mut drawn_a_tab = false;
        for (index, tab) in tui.snapshot.tabs.iter().enumerate() {
            // A tab with no room is not drawn, and neither is the separator
            // that would have introduced it. Drawing it anyway left a dangling
            // `·` at the end of a full bar, and -- once the strip scrolls --
            // would have opened it with one.
            if geometry.tab_labels[&tab.tab_id].width == 0 {
                continue;
            }
            if drawn_a_tab {
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
            drawn_a_tab = true;
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
            // The active tab's label is wrapped in spaces, which is what gives the
            // highlight its padding -- and the dots below add a separator space of
            // their own. Drawn in that order the selected tab gets both, so its dot
            // sits two cells out with a highlighted gap in front of it, while every
            // other tab's sits one. The label's own trailing pad is therefore held
            // back and drawn *after* the dots, closing the highlight around them:
            // same cells, same click target, one space either way.
            let padded = presence_width > 0 && label.ends_with(' ');
            let name = if padded {
                &label[..label.len() - 1]
            } else {
                &label[..]
            };
            let (mut cursor, _) = buffer.set_stringn(
                label_rect.x,
                label_rect.y,
                name,
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
                if padded {
                    buffer.set_stringn(cursor, label_rect.y, " ", 1, style);
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
        let now_unix_ms = crate::tui::clock::unix_ms_now();
        render_home(frame, tui, now_unix_ms);
        if tui.add_machine_open() {
            render_add_machine_modal(
                frame,
                theme,
                share,
                tui.add_machine_joined().as_deref(),
                crate::tui::render::home::animation_phase(now_unix_ms),
            );
        }
        // Home replaces the session chrome but not the question Ctrl+Q asks:
        // `q` opens the same prompt from here, so it has to be drawn here too.
        if tui.quit_open() {
            render_quit_prompt(frame, theme);
        }
        // Nor the one another machine is waiting on, which can arrive while the
        // inbox is up and expires whether or not it is on screen.
        if let Some(command) = tui.remote_work_command() {
            render_remote_work_prompt(frame, theme, command);
        }
        if let (true, Some(notice)) = (tui.update_confirm_open(), tui.update_notice.as_ref()) {
            render_update_confirm(frame, theme, notice);
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
        let zoomed = tui.zoomed_pane() == Some(pane_id);
        if zoomed {
            block = block.title_bottom(
                Line::styled(" zoom ", Style::default().fg(theme.footer_accent))
                    .alignment(Alignment::Left),
            );
        }
        let chips = pane_presence_chips(
            &tui.pane_watchers(pane_id),
            view.controller_peer_id.as_deref(),
            &tui.snapshot.members,
            theme,
            title_width,
        );
        let bottom_chrome = zoomed || chips.is_some();
        if let Some(chips) = chips {
            block = block.title_bottom(chips);
        }
        let content = pane_content_rect(rect);
        frame.render_widget(block, rect);
        // The fixed VT grid may be smaller than this pane after layout reflow. Clear the full
        // interior before drawing it so letterbox margins cannot retain cells from an older pane.
        frame.render_widget(Clear, content);
        if let Some(screen) = screens.get(&pane_id) {
            // The grid the screen actually has, not the one the layout records:
            // a reflow resizes the PTY here and now, while the descriptor only
            // catches up once the coordinator has accepted the new grid. Trusting
            // the descriptor letterboxes a pane that has already grown -- which is
            // most visible on a zoom, where the pane doubles in size in one frame.
            let (grid_rows, grid_cols) = screen.size();
            let scrollback = tui.scrollback_offset(pane_id);
            let screen = viewed_screen(screen, scrollback);
            let viewport = tui
                .pane_grid_viewport(pane_id, content, (grid_rows, grid_cols))
                .expect("visible pane has a local view");
            let (row, col) = screen.cursor_position();
            let origin = tui
                .nudge_pane_origin(
                    pane_id,
                    (grid_rows, grid_cols),
                    viewport,
                    ScreenCell { row, col },
                    scrollback == 0 && !screen.hide_cursor(),
                )
                .expect("visible pane has a local view");
            frame.render_widget(
                VtScreen::new(screen.as_ref())
                    .at_origin(origin)
                    .with_selection(
                        tui.selection()
                            .filter(|selection| selection.pane_id == pane_id),
                    )
                    .at_scrollback(scrollback)
                    .dimmed(pane_recedes(tui, pane_id, &view, focused, scrollback)),
                viewport,
            );
            render_clip_indicators(
                frame,
                rect,
                viewport,
                (grid_rows, grid_cols),
                origin,
                Style::default().fg(border_color),
                bottom_chrome,
            );
            // `scrollback`, not the screen's own offset: a client keeps no
            // scrollback of its own and renders history by swapping in a
            // viewport the node built, whose retained-row count -- and so whose
            // offset -- is always zero. Asking the screen left the caret parked
            // on a row of history, at a position that belonged to the live edge.
            //
            // A dialog takes the keyboard, so it takes the caret with it: while
            // one is up, keystrokes go to it and not to this pane, and a caret
            // still blinking down in the pane says the opposite. The dialogs
            // that own a text field place it themselves; the rest leave it off.
            if focused
                && view.ready
                && matches!(tui.modal, ModalState::None)
                && scrollback == 0
                && !screen.hide_cursor()
                && !pane.exited
                && row >= origin.row
                && col >= origin.col
                && row.saturating_sub(origin.row) < viewport.height
                && col.saturating_sub(origin.col) < viewport.width
            {
                frame.set_cursor_position((
                    viewport.x.saturating_add(col.saturating_sub(origin.col)),
                    viewport.y.saturating_add(row.saturating_sub(origin.row)),
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
    if let Some(command) = tui.remote_work_command() {
        render_remote_work_prompt(frame, &tui.theme, command);
    }
    if let (true, Some(notice)) = (tui.update_confirm_open(), tui.update_notice.as_ref()) {
        render_update_confirm(frame, &tui.theme, notice);
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
            ChordMode, MultiPaneTui, PaneViewState, ScreenCell, ShareView,
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
            kind: Default::default(),
            machine_proof: Default::default(),
            machine_id: Default::default(),
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
            kind: Default::default(),
            machine_proof: Default::default(),
            machine_id: Default::default(),
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

    /// A zoom is only a zoom if the terminal inside the pane grew with the box.
    ///
    /// The PTY is resized by the node the moment the zoom lands, but the pane
    /// descriptor only carries the new grid once the coordinator has accepted
    /// it. Drawing against the descriptor in the meantime leaves a full-screen
    /// box with a split-sized terminal in the corner of it — the bug this
    /// asserts against.
    #[test]
    fn a_zoomed_pane_draws_the_grid_its_pty_actually_has() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 60, 12);
        tui.toggle_zoom();
        let rect = tui.geometry(area).panes[&tui.focused_pane()];
        let (rows, columns) = crate::tui::geometry::grid_for_pane(rect);
        // The descriptor still says 4x10: the layout has not caught up yet.
        assert_eq!(
            (
                tui.snapshot().panes[&1].grid_rows,
                tui.snapshot().panes[&1].grid_cols
            ),
            (4, 10)
        );

        let mut parser = vt100::Parser::new(rows, columns, 0);
        parser.process(
            "X".repeat(usize::from(rows) * usize::from(columns))
                .as_bytes(),
        );
        let screen = parser.screen().clone();
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("terminal");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::from([(1u64, &screen)])))
            .expect("draw");

        let drawn = rendered(terminal.backend().buffer(), 60, 12);
        let last_content_row = drawn.lines().nth(usize::from(rows)).expect("content row");
        assert!(
            last_content_row.starts_with(&format!("│{}│", "X".repeat(usize::from(columns)))),
            "the zoomed pane should fill its box, got: {last_content_row}"
        );
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
                origin: Default::default(),
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
                origin: Default::default(),
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

    /// Two panes side by side, and only one of them is being typed into.
    ///
    /// The border already carried this, in the two cells of it a user is not
    /// looking at. What they are looking at is three screenfuls of text that
    /// were all exactly as bright as each other.
    #[test]
    fn a_pane_nobody_is_reading_is_drawn_at_reduced_strength() {
        let mut left = vt100::Parser::new(1, 4, 0);
        left.process(b"LEFT");
        let mut right = vt100::Parser::new(1, 4, 0);
        right.process(b"RGHT");
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: None,
            }],
            &[(1, 1, 4), (2, 1, 4)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        for pane_id in [1, 2] {
            tui.set_pane_view(
                pane_id,
                PaneViewState {
                    ready: true,
                    controller_peer_id: None,
                    controller_active: false,
                    scrollback: 0,
                    origin: Default::default(),
                },
            );
        }
        let screens = BTreeMap::from([(1, left.screen()), (2, right.screen())]);

        let dim_of = |tui: &MultiPaneTui| {
            let mut terminal = Terminal::new(TestBackend::new(24, 5)).expect("test terminal");
            terminal
                .draw(|frame| render_multi_pane(frame, tui, &screens))
                .expect("render");
            let buffer = terminal.backend().buffer().clone();
            let find = |needle: char| {
                (0..24)
                    .flat_map(|x| (0..5).map(move |y| (x, y)))
                    .find(|position| buffer[*position].symbol() == needle.to_string())
                    .map(|position| buffer[position].modifier.contains(Modifier::DIM))
                    .expect("both panes drew their text")
            };
            (find('L'), find('R'))
        };

        tui.select_pane(1, 1, "test");
        assert_eq!(
            dim_of(&tui),
            (false, false),
            "off unless the config asked: how faint SGR 2 renders is the terminal's choice"
        );

        tui.set_dim_unfocused_panes(true);
        assert_eq!(
            dim_of(&tui),
            (false, true),
            "asked for, focus is on the left pane, so the right one steps back"
        );

        tui.select_pane(1, 2, "test");
        assert_eq!(
            dim_of(&tui),
            (true, false),
            "and it swaps the moment focus does"
        );

        tui.set_dim_unfocused_panes(false);
        assert_eq!(
            dim_of(&tui),
            (false, false),
            "`dim_unfocused_panes = false` puts every pane back at full strength"
        );
    }

    /// The panes the dimming must leave alone even when it is on.
    ///
    /// Each of these means "somebody is reading this", and each is already
    /// modelled somewhere else in this program: the pointer aims the wheel, a
    /// non-zero scrollback offset is a pane somebody scrolled back through, a
    /// controller is a peer driving it from another machine, and an agent that
    /// is working or blocked is the one thing on the screen that has to catch
    /// the eye. Dimming any of them is the complaint in #119.
    #[test]
    fn the_panes_somebody_is_reading_are_never_dimmed() {
        let mut left = vt100::Parser::new(1, 4, 0);
        left.process(b"LEFT");
        let mut right = vt100::Parser::new(1, 4, 0);
        right.process(b"RGHT");
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first_share_bps: crate::layout::DEFAULT_FIRST_SHARE_BPS,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Leaf { pane_id: 2 }),
                },
                title: None,
            }],
            &[(1, 1, 4), (2, 1, 4)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        tui.set_dim_unfocused_panes(true);
        for pane_id in [1, 2] {
            tui.set_pane_view(
                pane_id,
                PaneViewState {
                    ready: true,
                    controller_peer_id: Some(Vec::new()),
                    controller_active: false,
                    scrollback: 0,
                    origin: Default::default(),
                },
            );
        }
        let screens = BTreeMap::from([(1, left.screen()), (2, right.screen())]);
        tui.select_pane(1, 1, "test");

        let right_is_dim = |tui: &MultiPaneTui| {
            let mut terminal = Terminal::new(TestBackend::new(24, 5)).expect("test terminal");
            terminal
                .draw(|frame| render_multi_pane(frame, tui, &screens))
                .expect("render");
            let buffer = terminal.backend().buffer().clone();
            (0..24)
                .flat_map(|x| (0..5).map(move |y| (x, y)))
                .find(|position| buffer[*position].symbol() == "R")
                .map(|position| buffer[position].modifier.contains(Modifier::DIM))
                .expect("the right pane drew its text")
        };

        assert!(right_is_dim(&tui), "nobody is reading pane 2 yet");

        tui.hovered_pane = Some(2);
        assert!(
            !right_is_dim(&tui),
            "the wheel is aimed by the pointer, so the pane under it is being read"
        );
        tui.hovered_pane = None;

        tui.set_pane_scrollback_offset(2, 3);
        assert!(
            !right_is_dim(&tui),
            "a pane parked in its own history is one somebody scrolled back through"
        );
        tui.set_pane_scrollback_offset(2, 0);

        tui.set_pane_view(
            2,
            PaneViewState {
                ready: true,
                controller_peer_id: Some(b"peer".to_vec()),
                controller_active: true,
                scrollback: 0,
                origin: Default::default(),
            },
        );
        assert!(
            !right_is_dim(&tui),
            "watching a peer drive a pane is what this program is for"
        );

        tui.local_peer_id = Some(b"peer".to_vec());
        assert!(
            right_is_dim(&tui),
            "but our own lease is not somebody else reading it -- typing takes one"
        );
        tui.local_peer_id = None;
        tui.set_pane_view(
            2,
            PaneViewState {
                ready: true,
                controller_peer_id: Some(Vec::new()),
                controller_active: false,
                scrollback: 0,
                origin: Default::default(),
            },
        );
        assert!(right_is_dim(&tui), "and it goes back once they let go");

        let mut agent = crate::tui::AgentOverlayRow {
            pane_id: 2,
            process_pid: 0,
            tab_ordinal: 1,
            pane_ordinal: 2,
            tab_label: String::from("Tab #1"),
            pane_label: String::from("Pane #2"),
            kind: String::from("claude"),
            cwd: String::new(),
            state: crate::protocol::AgentRosterState::Pending,
            working_since_unix_ms: 0,
            host: String::from("host"),
            controller: String::new(),
            message: String::from("permission: write to /etc/hosts"),
            session: String::new(),
            in_another_session: false,
        };
        tui.set_agent_rows(vec![agent.clone()]);
        assert!(
            !right_is_dim(&tui),
            "an agent blocked on a human is the last thing that should fade"
        );

        agent.state = crate::protocol::AgentRosterState::Idle;
        tui.set_agent_rows(vec![agent]);
        assert!(
            right_is_dim(&tui),
            "an idle agent is not a reason to keep a pane nobody is reading at full strength"
        );
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
                origin: Default::default(),
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
    fn focused_pane_crops_to_the_cursor_and_places_its_cursor_locally() {
        let mut parser = vt100::Parser::new(3, 5, 0);
        parser.process(b"abcde\r\nfghij\r\nklmno\x1b[3;5H");
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 3, 5)],
        ))
        .expect("valid layout");
        tui.set_pane_view(1, PaneViewState::from_chrome(true, None, false));
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(5, 5)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");

        assert_eq!(
            tui.pane_view(1).expect("pane view").origin.get(),
            ScreenCell { row: 2, col: 2 },
        );
        assert_eq!(terminal.backend().buffer()[(1, 2)].symbol(), "m");
        terminal.backend_mut().assert_cursor_position((3, 2));
    }

    #[test]
    fn cropped_pane_marks_each_hidden_grid_edge() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"\x1b[?25l");
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 3, 10)],
        ))
        .expect("valid layout");
        tui.pane_view(1)
            .expect("pane view")
            .origin
            .set(ScreenCell { row: 1, col: 1 });
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 3)].symbol(), "<");
        assert_eq!(buffer[(2, 3)].symbol(), "^");
        assert_eq!(buffer[(7, 3)].symbol(), "v");
        assert_eq!(buffer[(8, 3)].symbol(), ">");
    }

    /// Clipped means "outside the pane it is drawn in", not "outside the grid
    /// the layout last recorded" — the screen's own size is what the viewport
    /// follows, so a descriptor lagging behind a reflow no longer hides a cursor
    /// that is plainly on screen. A cursor past the pane's own border still is.
    #[test]
    fn focused_pane_hides_a_hidden_cursor_and_follows_a_visible_one() {
        for sequence in [b"\x1b[?25l".as_slice(), b"abcdefgh".as_slice()] {
            let mut parser = vt100::Parser::new(1, 9, 0);
            parser.process(sequence);
            let mut tui = MultiPaneTui::new(layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                }],
                &[(1, 1, 9)],
            ))
            .expect("valid layout");
            tui.set_pane_view(
                1,
                PaneViewState {
                    ready: true,
                    controller_peer_id: None,
                    controller_active: false,
                    scrollback: 0,
                    origin: Default::default(),
                },
            );
            let screens = BTreeMap::from([(1, parser.screen())]);
            let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &screens))
                .expect("render");

            if parser.screen().hide_cursor() {
                assert!(!terminal.backend().cursor_visible());
            } else {
                assert!(terminal.backend().cursor_visible());
                assert_eq!(tui.pane_view(1).expect("pane view").origin.get().col, 2,);
            }
        }
    }

    /// A pane scrolled back is not showing where its program's cursor is, so the
    /// caret must not blink on a row of history.
    ///
    /// Both ways a pane can be scrolled are covered, because they disagree about
    /// where the offset lives. A local pane scrolls its own screen; a client
    /// keeps no scrollback at all and swaps in a viewport the node built, whose
    /// retained-row count -- and so whose own offset -- is always zero. Reading
    /// the offset off the screen was right for the first and silently wrong for
    /// the second, which is every pane in a `create` session.
    #[test]
    fn a_scrolled_back_pane_keeps_no_caret_whichever_screen_it_is_showing() {
        let mut own_history = vt100::Parser::new(2, 9, 100);
        own_history.process(b"one\r\ntwo\r\nthree\r\nfour");
        // What the node hands a client for the same offset: the history rows,
        // parsed fresh, with no retained rows behind them.
        let mut node_viewport = vt100::Parser::new(2, 9, 0);
        node_viewport.process(b"one\r\ntwo");

        for parser in [&own_history, &node_viewport] {
            let mut tui = MultiPaneTui::new(layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                }],
                &[(1, 2, 9)],
            ))
            .expect("valid layout");
            tui.set_pane_view(
                1,
                PaneViewState {
                    ready: true,
                    controller_peer_id: None,
                    controller_active: false,
                    scrollback: 0,
                    origin: Default::default(),
                },
            );
            assert!(tui.set_pane_scrollback_offset(1, 2));
            let screens = BTreeMap::from([(1, parser.screen())]);
            let mut terminal = Terminal::new(TestBackend::new(11, 8)).expect("test terminal");

            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &screens))
                .expect("render");

            assert!(!terminal.backend().cursor_visible());
        }
    }

    /// Returning to the live edge gives the caret back.
    #[test]
    fn a_pane_back_at_the_live_edge_shows_its_caret_again() {
        let mut parser = vt100::Parser::new(2, 9, 100);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour");
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 9)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
                origin: Default::default(),
            },
        );
        assert!(tui.set_pane_scrollback_offset(1, 2));
        assert!(tui.set_pane_scrollback_offset(1, 0));
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(11, 8)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");

        assert!(terminal.backend().cursor_visible());
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
    /// The dot sits the same distance from its label whichever tab you are on.
    ///
    /// The selected tab's label is wrapped in spaces to give the highlight its
    /// padding, and the dots add a separator space of their own. Drawn in that
    /// order the selected tab got both -- a two-cell gap with a highlighted
    /// blank in it, which reads as the dot having drifted right, or as a second
    /// dot painted in the tab's own background colour.
    #[test]
    fn a_selected_tab_does_not_hold_its_dot_a_cell_further_out() {
        // The watcher is on tab two, which this fixture opens *not* selected --
        // so the same tab can be measured either way with one keypress between.
        let mut tui = two_tab_presence_tui();
        assert!(tui.set_presence(vec![crate::local_ipc::PresenceRow {
            peer_id: b"tis".to_vec(),
            tab_id: 20,
            pane_id: 2,
        }]));

        let gap = |tui: &MultiPaneTui| {
            let mut terminal = Terminal::new(TestBackend::new(52, 4)).expect("test terminal");
            terminal
                .draw(|frame| render_multi_pane(frame, tui, &BTreeMap::new()))
                .expect("render");
            let buffer = terminal.backend().buffer();
            let row = (0..52).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
            let dot = row.find(PRESENCE_WATCHING).expect("a watcher draws a dot");
            let name_end = row[..dot].trim_end().len();
            (dot - name_end, row)
        };

        let (unselected, unselected_row) = gap(&tui);
        tui.set_focus(20, 2)
            .expect("select the tab the watcher is on");
        let (selected, selected_row) = gap(&tui);

        assert_eq!(
            unselected, 1,
            "an unselected tab has one space before its dot: {unselected_row:?}"
        );
        assert_eq!(
            selected, unselected,
            "selecting a tab must not move its dot: {selected_row:?} vs {unselected_row:?}"
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
