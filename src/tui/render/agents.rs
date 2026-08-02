//! The Ctrl+A agents overlay: panel geometry, one line per agent, and the help
//! line under them.
//!
//! The panel is sized to what it has to say. It used to be a fixed 28-row box
//! that a single agent — or none — left almost entirely blank, with each agent
//! spread over two lines and a third for separation. Density is the whole point
//! of a list you open to answer "which of these needs me": the answer should be
//! readable in one glance without scrolling past whitespace.
//!
//! What each line spends its width on changed with it. The plumbing — tab, pane,
//! host, controller — moved to a detail line under the selected row, because it
//! is what you need *after* choosing a row, not to choose one. The width it
//! freed goes to what the agent is actually doing.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

use crate::{
    config::UiTheme,
    protocol::AgentRosterState,
    tui::{
        AGENT_OVERLAY_ANIMATION_INTERVAL, AgentOverlayRow, MultiPaneTui,
        app::member_color,
        member_label,
        render::{
            footer::{AGENT_OVERLAY_HELP, render_footer_segments},
            panes::PRESENCE_WATCHING,
        },
        text::{sanitize_single_line, text_width, truncate_leading, truncate_trailing},
    },
};

/// Widest the panel is allowed to get, and narrowest it is worth drawing.
const PANEL_MAX_WIDTH: u16 = 96;
const PANEL_MIN_WIDTH: u16 = 24;
/// Room for at least this many agent rows before the list starts scrolling,
/// when the terminal is tall enough to give it.
const MAX_VISIBLE_ROWS: u16 = 12;

/// The nudge shown when agents are running but nothing has reported on them.
///
/// This is the state a user lands in by default — the hooks are opt-in — and
/// without a line saying so the overlay looks broken rather than unconfigured.
pub(in crate::tui) const OVERLAY_UNWIRED_HINT: &str = "no agent hooks wired — run: p2pmux setup";

/// Where each part of the overlay is drawn.
///
/// Computed once from the terminal area and what there is to show, so the
/// renderer, the scroll clamp, and the click-to-row lookup all read the same
/// numbers. They used to derive their own from `area` alone, which is how the
/// click map came to ignore the members header and land a row too high for
/// every member in the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) struct AgentOverlayLayout {
    pub(in crate::tui) panel: Rect,
    /// The "Members" header block. Zero-height in a solo session.
    pub(in crate::tui) members: Rect,
    /// The scrollable agent list. One line per agent.
    pub(in crate::tui) rows: Rect,
    /// The selected row's plumbing, or the unwired nudge. Zero-height when neither applies.
    pub(in crate::tui) detail: Rect,
    pub(in crate::tui) help: Rect,
}

/// How many lines the members block wants for this session.
fn member_lines(tui: &MultiPaneTui) -> u16 {
    if tui.presence.is_empty() {
        0
    } else {
        // A "Members" heading, one line each, and a blank before the agents.
        u16::try_from(tui.presence.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
    }
}

pub(in crate::tui) fn agents_overlay_layout(area: Rect, tui: &MultiPaneTui) -> AgentOverlayLayout {
    let members_height = member_lines(tui);
    let rows_wanted = u16::try_from(tui.agent_rows.len().max(1)).unwrap_or(u16::MAX);
    let detail_height = u16::from(overlay_detail_line(tui, &UiTheme::default()).is_some());
    let width = area
        .width
        .saturating_sub(2)
        .clamp(PANEL_MIN_WIDTH.min(area.width), PANEL_MAX_WIDTH);
    // Borders, the members block, the rows, the detail line, and the help line.
    let wanted = 2u16
        .saturating_add(members_height)
        .saturating_add(rows_wanted.min(MAX_VISIBLE_ROWS))
        .saturating_add(detail_height)
        .saturating_add(1);
    let height = wanted.min(area.height);
    let panel = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.min(area.width),
        height,
    );
    let inner = Block::bordered().inner(panel);
    // Help first, then the detail line above it: both are anchored to the
    // bottom, so a panel squeezed by a short terminal loses list rows rather
    // than the line telling you what the keys do.
    let help = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        1.min(inner.height),
    );
    let detail_height = detail_height.min(inner.height.saturating_sub(help.height));
    let detail = Rect::new(
        inner.x,
        help.y.saturating_sub(detail_height),
        inner.width,
        detail_height,
    );
    let members_height = members_height.min(
        inner
            .height
            .saturating_sub(help.height)
            .saturating_sub(detail.height),
    );
    let members = Rect::new(inner.x, inner.y, inner.width, members_height);
    let rows = Rect::new(
        inner.x,
        members.bottom(),
        inner.width,
        detail.y.saturating_sub(members.bottom()).min(inner.height),
    );
    AgentOverlayLayout {
        panel,
        members,
        rows,
        detail,
        help,
    }
}

pub(in crate::tui) fn render_agents_overlay(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    now_unix_ms: u64,
) {
    let area = frame.area();
    let layout = agents_overlay_layout(area, tui);
    frame.render_widget(Clear, layout.panel);
    let needs_you = tui.agent_rows.iter().any(|row| row.state.needs_you());
    let title_color = if needs_you {
        tui.theme.agent_overlay_attention
    } else {
        tui.theme.agent_overlay_chrome
    };
    let block = Block::bordered()
        .title(Line::styled(
            agents_overlay_title(&tui.agent_rows),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(tui.theme.agent_overlay_chrome));
    frame.render_widget(block, layout.panel);

    // The overlay is already the place you look to answer "what is happening in this
    // session". Who is here and where belongs in the same glance, above the agents.
    if layout.members.height > 0 {
        frame.render_widget(Paragraph::new(presence_overlay_lines(tui)), layout.members);
    }

    if tui.agent_rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " No agents running",
                Style::default().fg(tui.theme.agent_overlay_muted),
            )),
            layout.rows,
        );
    } else {
        let animation_phase = agent_overlay_animation_phase(now_unix_ms);
        let lines = tui
            .agent_rows
            .iter()
            .skip(tui.agent_overlay_scroll_line)
            .map(|row| {
                format_agent_overlay_card(
                    row,
                    tui.agent_selected_pane == Some(row.pane_id),
                    layout.rows.width,
                    now_unix_ms,
                    animation_phase,
                    &tui.theme,
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), layout.rows);
    }

    if layout.detail.height > 0
        && let Some(detail) = overlay_detail_line(tui, &tui.theme)
    {
        frame.render_widget(Paragraph::new(detail), layout.detail);
    }
    render_agents_overlay_help(frame.buffer_mut(), &tui.theme, layout.help);
}

/// The line under the list: the selected agent's plumbing, or the unwired nudge.
///
/// The nudge wins. A session where nothing is reporting has a configuration
/// problem, and telling the user which tab an unreported agent sits in does not
/// help them fix it.
fn overlay_detail_line(tui: &MultiPaneTui, theme: &UiTheme) -> Option<Line<'static>> {
    if !tui.agent_rows.is_empty()
        && tui
            .agent_rows
            .iter()
            .all(|row| row.state == AgentRosterState::Unknown)
    {
        return Some(Line::styled(
            format!(" {OVERLAY_UNWIRED_HINT}"),
            Style::default().fg(theme.agent_overlay_attention),
        ));
    }
    let selected = tui
        .agent_selected_pane
        .and_then(|pane| tui.agent_rows.iter().find(|row| row.pane_id == pane))?;
    let mut spans = vec![Span::styled(
        format!(
            " {} · {} · host: {}",
            selected.tab_label, selected.pane_label, selected.host
        ),
        Style::default().fg(theme.agent_overlay_muted),
    )];
    if !selected.controller.is_empty() {
        spans.push(Span::styled(
            format!(" · control: {}", selected.controller),
            Style::default().fg(theme.agent_overlay_secondary),
        ));
    }
    Some(Line::from(spans))
}

/// One line per other member: their dot, their name, and where they are.
///
/// Empty in a solo session, so a single-player overlay gains no header for nobody. The
/// tab and pane ordinals come from the same lookup the agent rows use, so a member on a
/// pane this client cannot resolve is reported honestly rather than placed at a guess.
pub(in crate::tui) fn presence_overlay_lines(tui: &MultiPaneTui) -> Vec<Line<'static>> {
    let watchers = tui.presence.iter().collect::<Vec<_>>();
    if watchers.is_empty() {
        return Vec::new();
    }
    let mut watchers = watchers;
    watchers.sort_by_key(|row| tui.member_slot(&row.peer_id));
    let mut lines = vec![Line::styled(
        " Members",
        Style::default()
            .fg(tui.theme.agent_overlay_muted)
            .add_modifier(Modifier::BOLD),
    )];
    for row in watchers {
        let color = member_color(&row.peer_id, &tui.snapshot.members, &tui.theme)
            .unwrap_or(tui.theme.agent_overlay_muted);
        let location = match tui.pane_location(row.pane_id) {
            Some((tab_ordinal, pane_ordinal)) => {
                format!("Tab {tab_ordinal} · Pane {pane_ordinal}")
            }
            None => String::from("elsewhere"),
        };
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(PRESENCE_WATCHING, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(
                sanitize_single_line(&member_label(&row.peer_id, &tui.snapshot.members)),
                Style::default().fg(tui.theme.agent_overlay_foreground),
            ),
            Span::styled(
                format!(" · {location}"),
                Style::default().fg(tui.theme.agent_overlay_muted),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines
}

fn render_agents_overlay_help(buffer: &mut Buffer, theme: &UiTheme, help: Rect) {
    if help.height == 0 {
        return;
    }
    buffer.set_stringn(
        help.x,
        help.y,
        " ".repeat(usize::from(help.width)),
        usize::from(help.width),
        Style::default().bg(theme.footer_background),
    );
    render_footer_segments(
        buffer,
        theme,
        help.x.saturating_add(1),
        help.y,
        help.right(),
        AGENT_OVERLAY_HELP,
    );
}

/// Colour for a state's glyph and label. `Working` keeps the chrome accent it
/// has always had; the two states that want a human get their own roles so a
/// blocked or failed agent never reads as ordinary muted text.
fn agent_overlay_state_color(state: AgentRosterState, theme: &UiTheme) -> Color {
    match state {
        AgentRosterState::Working => theme.agent_overlay_chrome,
        AgentRosterState::Pending => theme.agent_overlay_attention,
        AgentRosterState::Error => theme.agent_overlay_error,
        AgentRosterState::Idle | AgentRosterState::Done | AgentRosterState::Unknown => {
            theme.agent_overlay_muted
        }
    }
}

/// Overlay title, carrying a count of agents blocked on a human when there are
/// any. The whole point of opening the overlay is finding those, so the count
/// belongs where it is readable before the eye reaches the rows.
pub(in crate::tui) fn agents_overlay_title(rows: &[AgentOverlayRow]) -> String {
    let needs_you = rows.iter().filter(|row| row.state.needs_you()).count();
    match needs_you {
        0 => String::from(" Agents "),
        1 => String::from(" Agents · 1 needs you "),
        count => format!(" Agents · {count} need you "),
    }
}

/// The glyph and the word for a state.
///
/// The word stays even though the glyph carries the same information: this list
/// is opened rarely, by someone who has not memorised a glyph legend, and the
/// column it costs is cheaper than the ambiguity it removes.
fn state_label(state: AgentRosterState, animation_phase: usize) -> (&'static str, &'static str) {
    match state {
        AgentRosterState::Working => (agent_overlay_working_glyph(animation_phase), "working"),
        AgentRosterState::Pending => ("◆", "needs you"),
        AgentRosterState::Error => ("✗", "error"),
        AgentRosterState::Done => ("●", "done"),
        AgentRosterState::Idle => ("○", "idle"),
        // Not "idle": nothing here knows that it is idle. It is running and
        // silent, and the row says exactly that.
        AgentRosterState::Unknown => ("·", "not reporting"),
    }
}

/// One agent, one line: `‹marker›‹glyph› ‹kind› ‹cwd›  ‹state›  ‹message›`.
///
/// The right-hand columns are the ones that get cut first as the panel narrows,
/// and they are ordered so that what survives is what a human needs: the state
/// outlives the message, and the glyph outlives both.
pub(in crate::tui) fn format_agent_overlay_card(
    row: &AgentOverlayRow,
    selected: bool,
    width: u16,
    now_unix_ms: u64,
    animation_phase: usize,
    theme: &UiTheme,
) -> Line<'static> {
    let (glyph, state_word) = state_label(row.state, animation_phase);
    let state_color = agent_overlay_state_color(row.state, theme);
    let marker = if selected { "›" } else { " " };
    let kind = overlay_kind_label(&row.kind);

    // Elapsed on `working` because that is the number a human reads as progress,
    // and on `needs you` because that is the one that reads as cost: an agent
    // that has been blocked on you for nine minutes is nine minutes wasted.
    let elapsed = matches!(
        row.state,
        AgentRosterState::Working | AgentRosterState::Pending
    )
    .then(|| format_agent_elapsed(row.working_since_unix_ms, now_unix_ms))
    .filter(|_| row.working_since_unix_ms > 0);

    let mut state_style = Style::default().fg(state_color);
    if row.state.needs_you() {
        // A blocked agent is the one row in the overlay that is costing someone
        // time right now, so it gets the only weight in the state column.
        state_style = state_style.add_modifier(Modifier::BOLD);
    }

    let kind_width = 11usize;
    let mut spans = vec![
        Span::styled(marker, Style::default().fg(theme.agent_overlay_chrome)),
        Span::styled(glyph, Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(
            format!("{kind:<kind_width$}"),
            Style::default()
                .fg(theme.agent_overlay_foreground)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    let state_text = match elapsed {
        Some(elapsed) => format!("{state_word} {elapsed}"),
        None => state_word.to_owned(),
    };
    // Budget from the right: the state column is fixed-width so rows line up,
    // and whatever is left over is the cwd's, then the message's.
    let fixed = text_width(marker)
        .saturating_add(text_width(glyph))
        .saturating_add(1)
        .saturating_add(kind_width as u16);
    let state_width = text_width(&state_text).max(14);
    let remaining = width
        .saturating_sub(fixed)
        .saturating_sub(state_width)
        .saturating_sub(2);
    let cwd_width = remaining.saturating_mul(2) / 5;
    let cwd = truncate_leading(&short_cwd(&row.cwd), usize::from(cwd_width));
    spans.push(Span::styled(
        format!("{cwd:<width$}", width = usize::from(cwd_width)),
        Style::default().fg(theme.agent_overlay_secondary),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{state_text:<width$}", width = usize::from(state_width)),
        state_style,
    ));

    // The agent's own words, and only ever for a pane on this machine — a
    // remote row's message is empty because the peer that owns it never sends
    // one. See `local_ipc::AgentOverlaySnapshotRow`.
    let message_width = remaining.saturating_sub(cwd_width);
    if !row.message.is_empty() && message_width > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            truncate_trailing(&row.message, usize::from(message_width)),
            Style::default().fg(theme.agent_overlay_muted),
        ));
    }

    let card_style = if selected {
        Style::default().bg(theme.agent_overlay_selected_background)
    } else {
        Style::default()
    };
    if selected {
        let content_width = spans.iter().fold(0_u16, |total, span| {
            total.saturating_add(text_width(&span.content))
        });
        spans.push(Span::styled(
            " ".repeat(usize::from(width.saturating_sub(content_width))),
            card_style,
        ));
    }
    Line::from(spans).style(card_style)
}

pub(in crate::tui) fn agent_overlay_animation_phase(now_unix_ms: u64) -> usize {
    now_unix_ms.saturating_div(AGENT_OVERLAY_ANIMATION_INTERVAL.as_millis() as u64) as usize
}

/// The working spinner.
///
/// Braille rather than the old quarter-circles: ten frames instead of four, so
/// the motion reads as continuous rather than as a thing flicking between four
/// positions, and every frame occupies exactly one column in every font that has
/// the block at all.
fn agent_overlay_working_glyph(animation_phase: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[animation_phase % FRAMES.len()]
}

/// The last two components of a working directory.
///
/// `/Users/pelazas/Desktop/p2pmux` reads as `Desktop/p2pmux`. The leading path
/// is the same for every row and identifies nothing; the tail is what the human
/// actually calls the project. Deliberately not `~`-relative — a row can belong
/// to a pane on someone else's machine, where this process's home directory
/// means nothing.
fn short_cwd(cwd: &str) -> String {
    let mut parts = cwd
        .trim_end_matches('/')
        .rsplit('/')
        .filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some(last), Some(parent)) => format!("{parent}/{last}"),
        (Some(last), None) => last.to_owned(),
        _ => cwd.to_owned(),
    }
}

fn overlay_kind_label(kind: &str) -> &'static str {
    match kind {
        "claude" => "claude",
        "codex" => "codex",
        "cursor" => "cursor",
        "pi" => "pi",
        "opencode" => "opencode",
        _ => "agent",
    }
}

fn format_agent_elapsed(working_since_unix_ms: u64, now_unix_ms: u64) -> String {
    let elapsed_seconds = now_unix_ms
        .saturating_sub(working_since_unix_ms)
        .saturating_div(1_000);
    if elapsed_seconds >= 3_600 {
        format!(
            "{}h{}m",
            elapsed_seconds / 3_600,
            (elapsed_seconds % 3_600) / 60
        )
    } else if elapsed_seconds >= 60 {
        format!("{}m", elapsed_seconds / 60)
    } else {
        format!("{elapsed_seconds}s")
    }
}
