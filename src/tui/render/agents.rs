//! The Ctrl+A agents overlay: panel geometry, one card per agent, and the
//! help line under them.

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

pub(in crate::tui) const AGENT_OVERLAY_CARD_LINES: usize = 3;
pub(in crate::tui) fn render_agents_overlay(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    now_unix_ms: u64,
) {
    let area = frame.area();
    let panel = agents_overlay_panel(area);
    frame.render_widget(Clear, panel);
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
    let content = agents_overlay_content(area);
    frame.render_widget(block, panel);
    // The overlay is already the place you look to answer "what is happening in this
    // session". Who is here and where belongs in the same glance, above the agents.
    let members = presence_overlay_lines(tui);
    if tui.agent_rows.is_empty() {
        let mut lines = members;
        lines.push(Line::styled(
            "No agents running",
            Style::default().fg(tui.theme.agent_overlay_muted),
        ));
        frame.render_widget(Paragraph::new(lines), content);
    } else {
        let mut lines = members;
        lines.reserve(tui.agent_rows.len().saturating_mul(3));
        let animation_phase = agent_overlay_animation_phase(now_unix_ms);
        for (index, row) in tui.agent_rows.iter().enumerate() {
            lines.extend(format_agent_overlay_card(
                row,
                tui.agent_selected_pane == Some(row.pane_id),
                content.width,
                now_unix_ms,
                animation_phase,
                &tui.theme,
            ));
            if index + 1 < tui.agent_rows.len() {
                lines.push(Line::raw(""));
            }
        }
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(tui.agent_overlay_scroll_line)
                    .collect::<Vec<_>>(),
            ),
            content,
        );
    }
    render_agents_overlay_help(frame.buffer_mut(), &tui.theme, agents_overlay_help(area));
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
        "Members",
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

fn agents_overlay_panel(area: Rect) -> Rect {
    let width = area.width.saturating_sub(2).clamp(24, 96);
    let height = area.height.saturating_sub(2).clamp(5, 28);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.min(area.width),
        height.min(area.height),
    )
}
pub(in crate::tui) fn agents_overlay_inner(area: Rect) -> Rect {
    Block::bordered().inner(agents_overlay_panel(area))
}
pub(in crate::tui) fn agents_overlay_help(area: Rect) -> Option<Rect> {
    let inner = agents_overlay_inner(area);
    (inner.height >= 4).then(|| {
        Rect::new(
            inner.x,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
            inner.width,
            1,
        )
    })
}
pub(in crate::tui) fn agents_overlay_content(area: Rect) -> Rect {
    let inner = agents_overlay_inner(area);
    let y = inner.y.saturating_add(1);
    let height = agents_overlay_help(area)
        .map(|help| help.y.saturating_sub(y))
        .unwrap_or_else(|| inner.height.saturating_sub(1));
    Rect::new(
        inner.x.saturating_add(1),
        y,
        inner.width.saturating_sub(2),
        height,
    )
}
fn render_agents_overlay_help(buffer: &mut Buffer, theme: &UiTheme, help: Option<Rect>) {
    let Some(help) = help else {
        return;
    };
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
        AgentRosterState::Idle | AgentRosterState::Done => theme.agent_overlay_muted,
    }
}
/// Overlay title, carrying a count of agents blocked on a human when there are
/// any. The whole point of opening the overlay is finding those, so the count
/// belongs where it is readable before the eye reaches the cards.
pub(in crate::tui) fn agents_overlay_title(rows: &[AgentOverlayRow]) -> String {
    let needs_you = rows.iter().filter(|row| row.state.needs_you()).count();
    match needs_you {
        0 => String::from(" Agents "),
        1 => String::from(" Agents · 1 needs you "),
        count => format!(" Agents · {count} need you "),
    }
}
pub(in crate::tui) fn format_agent_overlay_card(
    row: &AgentOverlayRow,
    selected: bool,
    width: u16,
    now_unix_ms: u64,
    animation_phase: usize,
    theme: &UiTheme,
) -> [Line<'static>; 2] {
    let marker = if selected { "›" } else { " " };
    let kind = overlay_kind_label(&row.kind);
    let first_prefix = format!("{marker} {kind} · ");
    let cwd = truncate_leading(
        &row.cwd,
        usize::from(width.saturating_sub(text_width(&first_prefix))),
    );
    let card_style = if selected {
        Style::default().bg(theme.agent_overlay_selected_background)
    } else {
        Style::default()
    };
    let first_line = agent_overlay_line(
        vec![
            Span::styled(marker, Style::default().fg(theme.agent_overlay_chrome)),
            Span::raw(" "),
            Span::styled(
                kind,
                Style::default()
                    .fg(theme.agent_overlay_foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(theme.agent_overlay_muted)),
            Span::styled(cwd, Style::default().fg(theme.agent_overlay_secondary)),
        ],
        card_style,
        selected,
        width,
    );

    let (glyph, state, elapsed) = match row.state {
        AgentRosterState::Working => (
            agent_overlay_working_glyph(animation_phase),
            "working",
            Some(format_agent_elapsed(row.working_since_unix_ms, now_unix_ms)),
        ),
        AgentRosterState::Idle => ("○", "idle", None),
        AgentRosterState::Done => ("✓", "done", None),
        AgentRosterState::Pending => ("◆", "needs you", None),
        AgentRosterState::Error => ("✗", "error", None),
    };
    let state_color = agent_overlay_state_color(row.state, theme);
    let location = format!(
        " · {} · {} · host: {}",
        truncate_trailing(&row.tab_label, usize::from(width / 3)),
        truncate_trailing(&row.pane_label, usize::from(width / 3)),
        row.host
    );
    let state_prefix = match elapsed.as_deref() {
        Some(elapsed) => format!("{glyph} {state} {elapsed}"),
        None => format!("{glyph} {state}"),
    };
    let controller_prefix = " · control: ";
    let controller_width = width.saturating_sub(
        text_width(&state_prefix)
            .saturating_add(text_width(&location))
            .saturating_add(text_width(controller_prefix)),
    );
    let controller = (controller_width > 0)
        .then(|| truncate_trailing(&row.controller, usize::from(controller_width)));
    let mut state_style = Style::default().fg(state_color);
    if row.state.needs_you() {
        // A blocked agent is the one row in the overlay that is costing someone
        // time right now, so it gets the only weight in the state column.
        state_style = state_style.add_modifier(Modifier::BOLD);
    }
    let mut second_spans = vec![
        Span::styled(glyph, Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(state, state_style),
    ];
    if let Some(elapsed) = elapsed {
        second_spans.push(Span::raw(" "));
        second_spans.push(Span::styled(
            elapsed,
            Style::default().fg(theme.agent_overlay_warm),
        ));
    }
    second_spans.push(Span::styled(
        location,
        Style::default().fg(theme.agent_overlay_muted),
    ));
    if let Some(controller) = controller {
        second_spans.push(Span::styled(
            controller_prefix,
            Style::default().fg(theme.agent_overlay_muted),
        ));
        second_spans.push(Span::styled(
            controller,
            Style::default().fg(theme.agent_overlay_secondary),
        ));
    }
    [
        first_line,
        agent_overlay_line(second_spans, card_style, selected, width),
    ]
}
pub(in crate::tui) fn agent_overlay_animation_phase(now_unix_ms: u64) -> usize {
    now_unix_ms.saturating_div(AGENT_OVERLAY_ANIMATION_INTERVAL.as_millis() as u64) as usize
}
fn agent_overlay_working_glyph(animation_phase: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    FRAMES[animation_phase % FRAMES.len()]
}
fn agent_overlay_line(
    mut spans: Vec<Span<'static>>,
    card_style: Style,
    selected: bool,
    width: u16,
) -> Line<'static> {
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
fn overlay_kind_label(kind: &str) -> &'static str {
    match kind {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor Agent",
        "pi" => "Pi",
        "opencode" => "OpenCode",
        _ => "Unknown",
    }
}
fn format_agent_elapsed(working_since_unix_ms: u64, now_unix_ms: u64) -> String {
    let elapsed_seconds = now_unix_ms
        .saturating_sub(working_since_unix_ms)
        .saturating_div(1_000);
    if elapsed_seconds >= 3_600 {
        format!(
            "{}h {}m",
            elapsed_seconds / 3_600,
            (elapsed_seconds % 3_600) / 60
        )
    } else if elapsed_seconds >= 60 {
        format!("{}m {}s", elapsed_seconds / 60, elapsed_seconds % 60)
    } else {
        format!("{elapsed_seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Color, Modifier},
    };

    use crate::{
        config::UiTheme,
        layout::{Node, Tab},
        tui::{
            MultiPaneTui,
            test_support::{agent_row, layout, named_members, watcher},
        },
    };

    use super::{
        agent_overlay_working_glyph, agents_overlay_content, agents_overlay_help,
        agents_overlay_inner, agents_overlay_panel, agents_overlay_title, format_agent_elapsed,
        format_agent_overlay_card, presence_overlay_lines, render_agents_overlay,
    };

    #[test]
    fn agents_overlay_cards_show_state_elapsed_and_location() {
        let row = agent_row(2, 2, 1);

        let rendered =
            format_agent_overlay_card(&row, true, 120, 1_725_000_084_123, 0, &UiTheme::default());

        assert!(rendered[0].to_string().starts_with("› Codex · "));
        assert!(
            rendered[0]
                .to_string()
                .contains("/very/long/repository/path")
        );
        assert!(rendered[1].to_string().starts_with("◐ working 1m 24s"));
        assert!(rendered[1].to_string().contains("Tab #2 · Pane #1"));
        assert!(rendered[1].to_string().contains("host: Host"));
        assert!(rendered[1].to_string().contains("control: free"));
    }

    #[test]
    fn agents_overlay_cards_distinguish_idle_done_and_future_work() {
        let mut row = agent_row(2, 2, 1);
        row.state = crate::protocol::AgentRosterState::Idle;
        let idle =
            format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &UiTheme::default());
        assert!(idle[1].to_string().starts_with("○ idle"));

        row.state = crate::protocol::AgentRosterState::Done;
        let done =
            format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &UiTheme::default());
        assert!(done[1].to_string().starts_with("✓ done"));

        row.state = crate::protocol::AgentRosterState::Working;
        row.working_since_unix_ms = 1_725_000_001_123;
        let future =
            format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &UiTheme::default());
        assert!(future[1].to_string().starts_with("◐ working 0s"));
    }

    #[test]
    fn agents_overlay_cards_mark_blocked_and_failed_agents() {
        use crate::protocol::AgentRosterState;

        let theme = UiTheme::default();
        let mut row = agent_row(2, 2, 1);

        row.state = AgentRosterState::Pending;
        let pending = format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &theme);
        assert!(pending[1].to_string().starts_with("◆ needs you"));
        // A blocked agent is the one row costing someone time, so it carries
        // the attention colour and the only weight in the state column.
        let state_span = &pending[1].spans[2];
        assert_eq!(state_span.style.fg, Some(theme.agent_overlay_attention));
        assert!(state_span.style.add_modifier.contains(Modifier::BOLD));

        row.state = AgentRosterState::Error;
        let error = format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &theme);
        assert!(error[1].to_string().starts_with("✗ error"));
        assert_eq!(error[1].spans[2].style.fg, Some(theme.agent_overlay_error));

        // Working keeps the chrome accent and stays unweighted.
        row.state = AgentRosterState::Working;
        let working = format_agent_overlay_card(&row, false, 120, 1_725_000_000_123, 0, &theme);
        assert_eq!(
            working[1].spans[2].style.fg,
            Some(theme.agent_overlay_chrome)
        );
        assert!(
            !working[1].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn agents_overlay_title_counts_agents_blocked_on_a_human() {
        use crate::protocol::AgentRosterState;

        let working = agent_row(1, 1, 1);
        assert_eq!(agents_overlay_title(&[]), " Agents ");
        assert_eq!(
            agents_overlay_title(std::slice::from_ref(&working)),
            " Agents "
        );

        let mut pending = agent_row(2, 1, 2);
        pending.state = AgentRosterState::Pending;
        assert_eq!(
            agents_overlay_title(&[working.clone(), pending.clone()]),
            " Agents · 1 needs you "
        );

        // Errors count too: a failed turn is blocking whoever asked for it.
        let mut failed = agent_row(3, 1, 3);
        failed.state = AgentRosterState::Error;
        assert_eq!(
            agents_overlay_title(&[working.clone(), pending, failed]),
            " Agents · 2 need you "
        );

        // A finished agent is worth a glance but is not holding anyone up.
        let mut done = agent_row(4, 1, 4);
        done.state = AgentRosterState::Done;
        assert_eq!(agents_overlay_title(&[working, done]), " Agents ");
    }

    #[test]
    fn agents_overlay_elapsed_uses_compact_saturating_durations() {
        assert_eq!(format_agent_elapsed(1_000, 43_000), "42s");
        assert_eq!(format_agent_elapsed(1_000, 85_000), "1m 24s");
        assert_eq!(format_agent_elapsed(1_000, 3_721_000), "1h 2m");
        assert_eq!(format_agent_elapsed(2_000, 1_000), "0s");
    }

    #[test]
    fn agents_overlay_uses_orange_red_chrome_soft_selection_and_modal_padding() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .expect("valid layout");
        tui.set_agent_rows(vec![agent_row(1, 1, 1)]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

        terminal
            .draw(|frame| render_agents_overlay(frame, &tui, 1_725_000_084_123))
            .expect("render");
        let area = Rect::new(0, 0, 80, 24);
        let panel = agents_overlay_panel(area);
        let inner = agents_overlay_inner(area);
        let content = agents_overlay_content(area);
        let help = agents_overlay_help(area).expect("help row");
        let buffer = terminal.backend().buffer();

        assert_eq!(panel, Rect::new(1, 1, 78, 22));
        assert_eq!(buffer[(panel.x, panel.y)].fg, Color::Rgb(255, 69, 0));
        assert_eq!(
            buffer[(panel.x.saturating_add(1), panel.y)].modifier,
            Modifier::BOLD
        );
        assert_eq!(buffer[(content.x, content.y)].bg, Color::Rgb(42, 42, 42));
        assert_eq!(
            buffer[(content.x.saturating_add(50), content.y.saturating_add(1))].bg,
            Color::Rgb(42, 42, 42)
        );
        assert_eq!(
            buffer[(content.right().saturating_sub(1), content.y)].bg,
            Color::Rgb(42, 42, 42)
        );
        assert_eq!(buffer[(inner.x, content.y)].bg, Color::Reset);
        assert_eq!(buffer[(content.x, inner.y)].bg, Color::Reset);
        assert_eq!(
            buffer[(content.x, content.y.saturating_add(2))].bg,
            Color::Reset
        );
        for x in help.x..help.right() {
            assert_eq!(buffer[(x, help.y)].bg, tui.theme.footer_background);
        }
        assert_eq!(buffer[(help.x.saturating_add(1), help.y)].symbol(), "<");
        assert_eq!(buffer[(help.x.saturating_add(2), help.y)].symbol(), "↑");
        assert_eq!(buffer[(help.x.saturating_add(14), help.y)].symbol(), "E");
    }

    #[test]
    fn agents_overlay_help_renders_for_empty_rosters() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

        terminal
            .draw(|frame| render_agents_overlay(frame, &tui, 1_725_000_084_123))
            .expect("render");
        let help = agents_overlay_help(Rect::new(0, 0, 80, 24)).expect("help row");
        let buffer = terminal.backend().buffer();

        for x in help.x..help.right() {
            assert_eq!(buffer[(x, help.y)].bg, tui.theme.footer_background);
        }
        assert_eq!(buffer[(help.x.saturating_add(2), help.y)].symbol(), "↑");
        assert_eq!(buffer[(help.x.saturating_add(14), help.y)].symbol(), "E");
    }

    #[test]
    fn agents_overlay_hides_help_below_the_minimum_content_height() {
        let short = Rect::new(0, 0, 80, 7);
        let threshold = Rect::new(0, 0, 80, 8);

        assert_eq!(agents_overlay_inner(short).height, 3);
        assert_eq!(agents_overlay_help(short), None);
        assert_eq!(agents_overlay_content(short).height, 2);
        assert!(agents_overlay_help(threshold).is_some());
        assert_eq!(agents_overlay_content(threshold).height, 2);
    }

    #[test]
    fn agents_overlay_working_glyph_uses_animation_phase() {
        assert_eq!(agent_overlay_working_glyph(0), "◐");
        assert_eq!(agent_overlay_working_glyph(1), "◓");
        assert_eq!(agent_overlay_working_glyph(4), "◐");
    }

    #[test]
    fn the_overlay_lists_who_is_here_and_where() {
        let mut snapshot = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },
                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        );
        snapshot.members = named_members();
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");

        assert!(
            presence_overlay_lines(&tui).is_empty(),
            "a solo session gains no header for nobody"
        );

        assert!(tui.set_presence(vec![watcher(b"tis", 2, 2), watcher(b"ana", 1, 404)]));
        let lines = presence_overlay_lines(&tui);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered[0], "Members");
        assert_eq!(rendered[1], "● tis · Tab 2 · Pane 1");
        assert_eq!(
            rendered[2], "● ana · elsewhere",
            "a pane this client cannot resolve is reported, not guessed at"
        );
        assert_eq!(
            rendered[3], "",
            "the agents below get their own breathing room"
        );
    }
}
