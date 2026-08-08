//! Drawing Home: the header count, one line per agent, the machine strip, and
//! the key bar.
//!
//! What is deliberately *not* here is as load-bearing as what is: no output
//! previews, no token or cost counters, no git status, no charts, and no list of
//! who else is in the session. Each of those turns a screen you glance at into a
//! screen you read, and Enter is one keypress away from the terminal that has
//! all of them.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    config::UiTheme,
    protocol::AgentRosterState,
    tui::{
        AgentOverlayRow, MultiPaneTui,
        home::{HomeLayout, MachineRow, home_layout, machine_rows},
        render::footer::{FooterSegment, render_footer_segments},
        text::{sanitize_single_line, text_width, truncate_leading, truncate_trailing},
    },
};

/// What a first run reads when nothing is running yet.
///
/// Not "no agents found", which sounds like a failure. This says what to do,
/// and that the screen will change on its own once you do it.
pub(in crate::tui) const HOME_EMPTY_NO_AGENTS: &str =
    "Start an agent in any terminal and it appears here.";
/// The nudge shown when agents are running but nothing is reporting on them.
///
/// This is the state a default install lands in — the hooks are opt-in — and it
/// is the single line that decides whether anyone opens the inbox twice. An
/// inbox that cannot say `needs you` is a list of processes.
pub(in crate::tui) const HOME_EMPTY_NO_HOOKS: &str =
    "Run `p2pmux setup` to see which agents need you.";
/// What sits in the description column of a row nothing has reported on.
///
/// Per-row rather than a banner over the list: the warning belongs exactly
/// where the doubt is, and it is a standing nudge toward `p2pmux setup` for as
/// long as it is true.
pub(in crate::tui) const HOME_ROW_NO_HOOKS: &str = "state unknown — no hooks";

const HOME_KEYS: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" open   "),
    FooterSegment::Key("n"),
    FooterSegment::Text(" new terminal   "),
    FooterSegment::Key("m"),
    FooterSegment::Text(" machines   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];

/// Column widths. The machine and agent columns are fixed so the eye can read
/// down them; everything the row has left over goes to what the agent is doing,
/// which is the only column whose content is worth more than its alignment.
const MACHINE_WIDTH: u16 = 10;
const AGENT_WIDTH: u16 = 9;
const STATE_WIDTH: u16 = 12;
const ELAPSED_WIDTH: u16 = 5;

pub(in crate::tui) fn render_home(frame: &mut Frame<'_>, tui: &MultiPaneTui, now_unix_ms: u64) {
    let geometry = tui.geometry(frame.area());
    render_home_in(
        frame,
        tui,
        home_layout(geometry.content, tui),
        geometry.footer,
        now_unix_ms,
    );
}

fn render_home_in(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    layout: HomeLayout,
    keys: Rect,
    now_unix_ms: u64,
) {
    let theme = &tui.theme;
    if layout.header.height > 0 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                header_line(tui.home_needs_you_count(), theme),
            ]),
            layout.header,
        );
    }

    if layout.rows.height > 0 {
        let rows = tui.home_rows();
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::raw(""),
                    Line::styled(
                        format!(" {HOME_EMPTY_NO_AGENTS}"),
                        Style::default().fg(theme.agent_overlay_muted),
                    ),
                ]),
                layout.rows,
            );
        } else {
            let animation_phase = animation_phase(now_unix_ms);
            let lines = rows
                .into_iter()
                .skip(tui.home_scroll_line)
                .take(usize::from(layout.rows.height))
                .map(|row| {
                    format_home_row(
                        row,
                        tui.home_selected == Some(row.pane_id),
                        layout.rows.width,
                        now_unix_ms,
                        animation_phase,
                        theme,
                    )
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), layout.rows);
        }
    }

    if layout.hint.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {HOME_EMPTY_NO_HOOKS}"),
                Style::default()
                    .fg(theme.agent_overlay_attention)
                    .add_modifier(Modifier::BOLD),
            )),
            layout.hint,
        );
    }

    if layout.machines.height > 0 {
        frame.render_widget(Paragraph::new(machine_block(tui, theme)), layout.machines);
    }

    if keys.width > 0 && keys.height > 0 {
        render_home_keys(frame.buffer_mut(), theme, keys);
    }
}

/// `Agents · 2 need you` — the whole value of the screen in one number, and the
/// exact sentence a notification will one day carry.
pub(in crate::tui) fn header_line(needs_you: usize, theme: &UiTheme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " Agents",
        Style::default()
            .fg(theme.agent_overlay_foreground)
            .add_modifier(Modifier::BOLD),
    )];
    if needs_you > 0 {
        spans.push(Span::styled(
            format!(
                " · {needs_you} {}",
                if needs_you == 1 {
                    "needs you"
                } else {
                    "need you"
                }
            ),
            Style::default()
                .fg(theme.agent_overlay_attention)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// One agent, one line:
/// `‹dot› ‹machine› ‹agent› ‹state› ‹what it is doing› ‹elapsed›`.
///
/// Machine is a column rather than a panel because you care about a machine
/// almost exclusively through an agent that happens to be on it. Elapsed is
/// right-aligned and last because it is the first thing worth cutting when the
/// terminal is narrow.
pub(in crate::tui) fn format_home_row(
    row: &AgentOverlayRow,
    selected: bool,
    width: u16,
    now_unix_ms: u64,
    animation_phase: usize,
    theme: &UiTheme,
) -> Line<'static> {
    let (dot, state_word) = home_state_label(row.state, animation_phase);
    let state_color = home_state_color(row.state, theme);
    let marker = if selected { "›" } else { " " };

    let mut state_style = Style::default().fg(state_color);
    if row.state.needs_you() {
        // The one row on screen that is costing someone time right now gets the
        // only weight in the state column.
        state_style = state_style.add_modifier(Modifier::BOLD);
    }

    let mut spans = vec![
        Span::styled(marker, Style::default().fg(theme.agent_overlay_chrome)),
        Span::styled(dot, Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(
            pad(&sanitize_single_line(&row.host), MACHINE_WIDTH),
            Style::default().fg(theme.agent_overlay_foreground),
        ),
        Span::raw(" "),
        Span::styled(
            pad(home_kind_label(&row.kind), AGENT_WIDTH),
            Style::default()
                .fg(theme.agent_overlay_foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(pad(state_word, STATE_WIDTH), state_style),
        Span::raw(" "),
    ];

    let fixed = text_width(marker)
        .saturating_add(text_width(dot))
        .saturating_add(MACHINE_WIDTH)
        .saturating_add(AGENT_WIDTH)
        .saturating_add(STATE_WIDTH)
        .saturating_add(4);
    let elapsed = home_elapsed(row, now_unix_ms);
    let description_width = width
        .saturating_sub(fixed)
        .saturating_sub(ELAPSED_WIDTH)
        .saturating_sub(1);
    let (description, description_style) = home_description(row, theme);
    if description_width > 0 {
        spans.push(Span::styled(
            pad(
                &truncate_trailing(&description, usize::from(description_width)),
                description_width,
            ),
            description_style,
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{elapsed:>width$}", width = usize::from(ELAPSED_WIDTH)),
            Style::default().fg(theme.agent_overlay_muted),
        ));
    }

    let style = if selected {
        Style::default().bg(theme.agent_overlay_selected_background)
    } else {
        Style::default()
    };
    if selected {
        let used = spans.iter().fold(0_u16, |total, span| {
            total.saturating_add(text_width(&span.content))
        });
        spans.push(Span::styled(
            " ".repeat(usize::from(width.saturating_sub(used))),
            style,
        ));
    }
    Line::from(spans).style(style)
}

/// What the row says the agent is doing.
///
/// The agent's own words when a hook sent some, and an honest admission when
/// none did. Never a model-written summary: a richer sentence that can lie
/// about what an agent did defeats the entire point of not reading the terminal
/// yourself.
fn home_description(row: &AgentOverlayRow, theme: &UiTheme) -> (String, Style) {
    if row.state == AgentRosterState::Unknown {
        return (
            HOME_ROW_NO_HOOKS.to_owned(),
            Style::default().fg(theme.agent_overlay_secondary),
        );
    }
    if row.message.is_empty() {
        return (
            short_cwd(&row.cwd),
            Style::default().fg(theme.agent_overlay_secondary),
        );
    }
    (
        sanitize_single_line(&row.message),
        Style::default().fg(theme.agent_overlay_muted),
    )
}

/// Elapsed for every state that has a clock running. A row with no episode
/// behind it shows nothing rather than a zero.
fn home_elapsed(row: &AgentOverlayRow, now_unix_ms: u64) -> String {
    if row.working_since_unix_ms == 0 {
        return String::new();
    }
    let seconds = now_unix_ms
        .saturating_sub(row.working_since_unix_ms)
        .saturating_div(1_000);
    if seconds >= 3_600 {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// The glyph and the word for a state, as the inbox says them.
///
/// `Unknown` reads as `running`, not as `idle` and not as a shrug: process
/// detection genuinely knows the agent is alive, which is exactly as much as
/// watching processes is ever allowed to claim. What it does not know goes in
/// the description column, where [`HOME_ROW_NO_HOOKS`] says so in words.
fn home_state_label(
    state: AgentRosterState,
    animation_phase: usize,
) -> (&'static str, &'static str) {
    match state {
        AgentRosterState::Pending => ("●", "needs you"),
        AgentRosterState::Error => ("✗", "error"),
        AgentRosterState::Done => ("✓", "done"),
        AgentRosterState::Working => (working_glyph(animation_phase), "running"),
        AgentRosterState::Unknown => ("○", "running"),
        AgentRosterState::Idle => ("○", "idle"),
    }
}

/// Which spinner frame this instant lands on.
///
/// Derived from the clock rather than from a counter the draw loop advances, so
/// every row spins in step and a frame skipped by a slow repaint is a frame
/// skipped, not a spinner that falls behind.
pub(in crate::tui) fn animation_phase(now_unix_ms: u64) -> usize {
    now_unix_ms.saturating_div(crate::tui::AGENT_OVERLAY_ANIMATION_INTERVAL.as_millis() as u64)
        as usize
}

/// The working spinner.
///
/// Braille rather than quarter-circles: ten frames instead of four, so the
/// motion reads as continuous, and every frame occupies exactly one column in
/// every font that has the block at all.
fn working_glyph(animation_phase: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[animation_phase % FRAMES.len()]
}

fn home_state_color(state: AgentRosterState, theme: &UiTheme) -> Color {
    match state {
        AgentRosterState::Pending => theme.agent_overlay_attention,
        AgentRosterState::Error => theme.agent_overlay_error,
        AgentRosterState::Done => theme.agent_overlay_warm,
        AgentRosterState::Working => theme.agent_overlay_chrome,
        AgentRosterState::Idle | AgentRosterState::Unknown => theme.agent_overlay_muted,
    }
}

fn home_kind_label(kind: &str) -> &'static str {
    match kind {
        "claude" => "claude",
        "codex" => "codex",
        "cursor" => "cursor",
        "pi" => "pi",
        "opencode" => "opencode",
        _ => "agent",
    }
}

/// The machine strip: fleet health in one line, without a second screen.
pub(in crate::tui) fn machine_block(tui: &MultiPaneTui, theme: &UiTheme) -> Vec<Line<'static>> {
    let machines = machine_rows(tui);
    if machines.len() < 2 {
        return Vec::new();
    }
    if !tui.machines_expanded {
        let mut spans = vec![Span::raw(" ")];
        for machine in &machines {
            spans.push(Span::styled(
                sanitize_single_line(&machine.name),
                Style::default().fg(theme.agent_overlay_foreground),
            ));
            spans.push(Span::styled(
                if machine.reachable {
                    " ✓   ".to_owned()
                } else {
                    " asleep   ".to_owned()
                },
                Style::default().fg(if machine.reachable {
                    theme.agent_overlay_muted
                } else {
                    theme.agent_overlay_secondary
                }),
            ));
        }
        return vec![Line::raw(""), Line::from(spans)];
    }
    let mut lines = vec![
        Line::raw(""),
        Line::styled(
            format!(
                " {:<12} {:<8} {:<14} {}",
                "NAME", "STATUS", "ACCEPTS WORK", "RUNNING"
            ),
            Style::default()
                .fg(theme.agent_overlay_muted)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for machine in &machines {
        lines.push(Line::styled(
            format!(" {}", machine_line(machine)),
            Style::default().fg(theme.agent_overlay_foreground),
        ));
    }
    lines
}

/// One machine, formatted identically on Home and in `p2pmux machines`, so the
/// two can never drift into describing the same fleet differently.
pub fn machine_line(machine: &MachineRow) -> String {
    let status = if machine.reachable { "ready" } else { "asleep" };
    // A dash means "never said", not "no". The answer is given on the machine
    // it is about and has no way back here, so printing `no` would show a
    // refusal nobody made.
    let accepts = match machine.accepts_work {
        Some(true) => "yes",
        Some(false) => "no",
        None => "—",
    };
    let running = match machine.agents {
        0 if !machine.reachable => String::from("—"),
        0 => String::from("—"),
        1 => String::from("1 agent"),
        count => format!("{count} agents"),
    };
    let suffix = if machine.this_machine {
        "      (this machine)"
    } else {
        ""
    };
    format!(
        "{:<12} {:<8} {:<14} {running}{suffix}",
        truncate_trailing(&sanitize_single_line(&machine.name), 11),
        status,
        accepts
    )
}

fn render_home_keys(buffer: &mut Buffer, theme: &UiTheme, keys: Rect) {
    if keys.height == 0 {
        return;
    }
    buffer.set_stringn(
        keys.x,
        keys.y,
        " ".repeat(usize::from(keys.width)),
        usize::from(keys.width),
        Style::default().bg(theme.footer_background),
    );
    render_footer_segments(
        buffer,
        theme,
        keys.x.saturating_add(1),
        keys.y,
        keys.right(),
        HOME_KEYS,
    );
}

/// The last two components of a working directory: `Desktop/p2pmux` rather than
/// the whole path, which is the same for every row and identifies nothing.
fn short_cwd(cwd: &str) -> String {
    let mut parts = cwd
        .trim_end_matches('/')
        .rsplit('/')
        .filter(|part| !part.is_empty());
    match (parts.next(), parts.next()) {
        (Some(last), Some(parent)) => format!("{parent}/{last}"),
        (Some(last), None) => last.to_owned(),
        _ => cwd.to_owned(),
    }
}

/// Left-align into a fixed column, cutting from the *front* when the value is
/// too long: a machine or agent name is identified by its tail far more often
/// than by its head.
fn pad(value: &str, width: u16) -> String {
    let value = truncate_leading(value, usize::from(width));
    format!("{value:<width$}", width = usize::from(width))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        HOME_EMPTY_NO_AGENTS, HOME_EMPTY_NO_HOOKS, HOME_ROW_NO_HOOKS, format_home_row, header_line,
        machine_line,
    };
    use crate::{
        agent_detect::{AgentKind, AgentState, DetectedAgent, PaneAgentTracker},
        config::UiTheme,
        protocol::AgentRosterState,
        tui::{home::MachineRow, test_support::agent_row},
    };

    fn rendered(state: AgentRosterState, message: &str) -> String {
        let mut row = agent_row(1, 1, 1);
        row.state = state;
        row.host = String::from("droplet");
        row.kind = String::from("claude");
        row.message = message.to_owned();
        format_home_row(&row, false, 100, 0, 0, &UiTheme::default()).to_string()
    }

    /// The rule, checked at the boundary where it is consumed rather than only
    /// where it is enforced. Watching processes may say `running` and nothing
    /// more; only a hook may say `needs you`.
    #[test]
    fn a_scanned_agent_with_no_hook_can_only_ever_reach_the_screen_as_running() {
        let mut tracker = PaneAgentTracker::default();
        tracker.update(Some(DetectedAgent {
            kind: AgentKind::Claude,
            cwd: String::from("/repo"),
        }));

        let listed = tracker.listed_agent().expect("a detected agent has a row");
        assert_eq!(
            listed.state,
            AgentState::Unknown,
            "detection knows an agent exists and nothing else"
        );
        assert!(
            listed.message.is_empty(),
            "there is no sentence to show, and inventing one is what the rule forbids"
        );

        let line = rendered(AgentRosterState::Unknown, "");
        assert!(line.contains("running"), "{line:?}");
        assert!(!line.contains("needs you"), "{line:?}");
        assert!(
            line.contains(HOME_ROW_NO_HOOKS),
            "the doubt is labelled on the row it belongs to: {line:?}"
        );
    }

    #[test]
    fn only_a_pushed_status_can_put_needs_you_on_a_row() {
        let mut tracker = PaneAgentTracker::default();
        tracker.record_pushed_status(
            AgentKind::Claude,
            String::from("/repo"),
            AgentState::Pending,
            String::from("permission: write to /etc/hosts"),
            Instant::now(),
            0,
        );

        assert_eq!(
            tracker.listed_agent().expect("a pushed row").state,
            AgentState::Pending
        );
        let line = rendered(AgentRosterState::Pending, "permission: write to /etc/hosts");
        assert!(line.contains("needs you"), "{line:?}");
        assert!(
            line.contains("permission: write to /etc/hosts"),
            "the row carries the agent's own words, not a summary of them: {line:?}"
        );
        assert!(!line.contains(HOME_ROW_NO_HOOKS), "{line:?}");
    }

    #[test]
    fn a_row_falls_back_to_its_directory_rather_than_inventing_a_sentence() {
        // No hook text for a state a hook did report. The honest filler is a
        // fact the row already has -- never a model-written summary, which is
        // richer and can lie about what an agent did, which defeats the entire
        // point of not reading the terminal yourself.
        let line = rendered(AgentRosterState::Working, "");
        assert!(line.contains("repository/path"), "{line:?}");
    }

    #[test]
    fn the_header_says_the_count_in_words_a_notification_could_reuse() {
        let theme = UiTheme::default();
        assert_eq!(header_line(0, &theme).to_string().trim(), "Agents");
        assert_eq!(
            header_line(1, &theme).to_string().trim(),
            "Agents · 1 needs you"
        );
        assert_eq!(
            header_line(2, &theme).to_string().trim(),
            "Agents · 2 need you"
        );
    }

    #[test]
    fn the_empty_states_say_what_to_do_rather_than_what_is_missing() {
        assert_eq!(
            HOME_EMPTY_NO_AGENTS,
            "Start an agent in any terminal and it appears here."
        );
        assert_eq!(
            HOME_EMPTY_NO_HOOKS,
            "Run `p2pmux setup` to see which agents need you."
        );
    }

    #[test]
    fn an_unanswered_accepts_work_column_reads_as_a_dash_not_a_refusal() {
        let here = MachineRow {
            name: String::from("laptop"),
            reachable: true,
            accepts_work: None,
            agents: 2,
            this_machine: true,
        };
        let there = MachineRow {
            this_machine: false,
            accepts_work: Some(true),
            agents: 1,
            ..here.clone()
        };

        let line = machine_line(&here);
        assert!(
            line.contains('—'),
            "a dash means never said, not a refusal nobody made: {line:?}"
        );
        assert!(line.contains("2 agents"), "{line:?}");
        assert!(line.contains("(this machine)"), "{line:?}");
        assert!(machine_line(&there).contains("yes"));
        assert!(machine_line(&there).contains("1 agent"));
    }

    #[test]
    fn an_unreachable_machine_reads_asleep() {
        let asleep = MachineRow {
            name: String::from("oldbox"),
            reachable: false,
            accepts_work: Some(true),
            agents: 0,
            this_machine: false,
        };

        assert!(machine_line(&asleep).contains("asleep"));
    }
}
