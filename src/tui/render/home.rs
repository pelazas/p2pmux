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
        home::{
            HomeCard, HomeLayout, MACHINE_RAIL_WIDTH, MachinePanel, MachineRow, home_card,
            home_layout, machine_rows,
        },
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

/// `m machines` is gone from here: it expanded the strip into a table, and the
/// rail is that table, permanently. A key that changes nothing is worse on a
/// key bar than a key that is missing from one.
const HOME_KEYS: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" open   "),
    FooterSegment::Key("a"),
    FooterSegment::Text(" add machine   "),
    FooterSegment::Key("n"),
    FooterSegment::Text(" new terminal   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];

/// Column widths. The machine and agent columns are fixed so the eye can read
/// down them; everything the row has left over goes to what the agent is doing,
/// which is the only column whose content is worth more than its alignment.
const MACHINE_WIDTH: u16 = 10;
const AGENT_WIDTH: u16 = 9;
const STATE_WIDTH: u16 = 12;
/// Wide enough for `9h59m59s`, which is every clock anyone watches. A longer
/// one pushes into the description column rather than losing a digit.
const ELAPSED_WIDTH: u16 = 8;
/// What a rail line has left for words once the rule and its space are drawn.
const RAIL_TEXT_WIDTH: usize = MACHINE_RAIL_WIDTH as usize - 2;
/// A spacer, the name, and what the machine is doing.
const RAIL_LINES_PER_MACHINE: usize = 3;
/// The rule and the key under it, which the fleet never grows into.
const RAIL_FOOTER_LINES: usize = 2;
/// Where a card's second and third lines start: under the dot, not under the
/// marker, so the block of text hangs off the state glyph that introduces it.
const CARD_INDENT: u16 = 3;

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
                header_line(
                    tui.home_needs_you_count(),
                    tui.home_page(),
                    tui.home_page_count(),
                    layout.header.width,
                    theme,
                ),
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
            let card = home_card(layout.rows.height);
            let lines = rows
                .into_iter()
                .skip(tui.home_page_start())
                .take(usize::from(layout.rows.height) / card.lines())
                .flat_map(|row| {
                    format_home_card(
                        row,
                        card,
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
        let lines = match layout.machine_panel {
            MachinePanel::Rail => machine_rail(tui, theme, layout.machines.height),
            MachinePanel::Table => machine_table(tui, theme),
            MachinePanel::Strip => machine_strip(tui, theme),
            MachinePanel::Empty => Vec::new(),
        };
        frame.render_widget(Paragraph::new(lines), layout.machines);
    }

    if keys.width > 0 && keys.height > 0 {
        render_home_keys(frame.buffer_mut(), theme, keys);
    }
}

/// `Agents · 2 need you` — the whole value of the screen in one number, and the
/// exact sentence a notification will one day carry.
///
/// The page marker sits at the other end, and only when there is more than one
/// page. It never has to be read to know whether something is waiting: the
/// count beside the title covers the whole list, not the page on screen, and
/// the sort order puts what most wants a human on page one.
pub(in crate::tui) fn header_line(
    needs_you: usize,
    page: usize,
    pages: usize,
    width: u16,
    theme: &UiTheme,
) -> Line<'static> {
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
    if pages > 1 {
        let marker = format!("page {} of {pages} ", page.saturating_add(1), pages = pages);
        let used = spans.iter().fold(0_u16, |total, span| {
            total.saturating_add(text_width(&span.content))
        });
        let gap = width
            .saturating_sub(used)
            .saturating_sub(text_width(&marker));
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(usize::from(gap))));
            spans.push(Span::styled(
                marker,
                Style::default().fg(theme.agent_overlay_muted),
            ));
        }
    }
    Line::from(spans)
}

/// One agent, as many lines as the terminal has room for.
///
/// The `Full` card is the one worth reading:
///
/// ```text
/// › ● droplet    claude                     needs you        2m14s
///     permission: write to /etc/hosts
///     ~/work/p2pmux · tab 2 · pane 1
/// ```
///
/// The second line is what the agent said, at whatever length it said it, and
/// the third is the two facts a one-line row had to choose between showing —
/// which repository it is in, and where Enter is about to put you. It is never
/// a model-written summary: a richer sentence that can lie about what an agent
/// did defeats the entire point of not reading the terminal yourself.
pub(in crate::tui) fn format_home_card(
    row: &AgentOverlayRow,
    card: HomeCard,
    selected: bool,
    width: u16,
    now_unix_ms: u64,
    animation_phase: usize,
    theme: &UiTheme,
) -> Vec<Line<'static>> {
    if card == HomeCard::Row {
        return vec![format_home_row(
            row,
            selected,
            width,
            now_unix_ms,
            animation_phase,
            theme,
        )];
    }
    let background = if selected {
        Style::default().bg(theme.agent_overlay_selected_background)
    } else {
        Style::default()
    };
    let (said, said_style) = home_description(row, theme);
    let mut lines = vec![
        card_headline(row, selected, width, now_unix_ms, animation_phase, theme),
        card_line(&said, said_style, width, background),
    ];
    if card == HomeCard::Full {
        // The cwd only when the line above did not already have to be it: the
        // fallback for an agent that has said nothing is its directory, and
        // printing that twice would waste the line the card exists for.
        let location = home_location(row, said == short_cwd(&row.cwd));
        lines.push(card_line(
            &location,
            Style::default().fg(theme.agent_overlay_secondary),
            width,
            background,
        ));
    }
    // The blank between two cards, tinted with the selection so the band reads
    // as one agent rather than as three lines that happen to be adjacent.
    lines.push(card_line("", Style::default(), width, background));
    lines
}

/// `~/work/p2pmux · tab 2 · pane 1` — where the agent is, and where Enter goes.
fn home_location(row: &AgentOverlayRow, cwd_already_shown: bool) -> String {
    let where_it_runs = format!("tab {} · pane {}", row.tab_ordinal, row.pane_ordinal);
    if cwd_already_shown || row.cwd.is_empty() {
        return where_it_runs;
    }
    format!("{} · {where_it_runs}", short_cwd(&row.cwd))
}

/// A card's second or third line: indented under the dot, and padded so the
/// selection band covers the whole width.
fn card_line(text: &str, style: Style, width: u16, background: Style) -> Line<'static> {
    let room = usize::from(width.saturating_sub(CARD_INDENT));
    let text = truncate_trailing(&sanitize_single_line(text), room);
    Line::from(vec![
        Span::styled(" ".repeat(usize::from(CARD_INDENT)), background),
        Span::styled(format!("{text:<room$}"), style.patch(background)),
    ])
    .style(background)
}

/// The first line of a card: who, where, what state, and for how long.
fn card_headline(
    row: &AgentOverlayRow,
    selected: bool,
    width: u16,
    now_unix_ms: u64,
    animation_phase: usize,
    theme: &UiTheme,
) -> Line<'static> {
    let (dot, state_word) = home_state_label(row.state, animation_phase);
    let state_color = home_state_color(row.state, theme);
    let background = if selected {
        Style::default().bg(theme.agent_overlay_selected_background)
    } else {
        Style::default()
    };
    let mut state_style = Style::default().fg(state_color);
    if row.state.needs_you() {
        state_style = state_style.add_modifier(Modifier::BOLD);
    }

    let mut spans = vec![
        Span::styled(
            if selected { "›" } else { " " },
            Style::default()
                .fg(theme.agent_overlay_chrome)
                .patch(background),
        ),
        Span::styled(dot, Style::default().fg(state_color).patch(background)),
        Span::styled(" ", background),
        Span::styled(
            pad(&sanitize_single_line(&row.host), MACHINE_WIDTH),
            Style::default()
                .fg(theme.agent_overlay_foreground)
                .patch(background),
        ),
        Span::styled(" ", background),
        Span::styled(
            pad(home_kind_label(&row.kind), AGENT_WIDTH),
            Style::default()
                .fg(theme.agent_overlay_foreground)
                .add_modifier(Modifier::BOLD)
                .patch(background),
        ),
    ];
    // State and elapsed are pushed to the right-hand edge, where they line up
    // down the page and leave the middle to the name. Both drop off a terminal
    // too narrow to hold them rather than pushing the name off the front.
    let used = text_width("  ")
        .saturating_add(text_width(dot))
        .saturating_add(MACHINE_WIDTH)
        .saturating_add(AGENT_WIDTH)
        .saturating_add(1);
    let tail = STATE_WIDTH.saturating_add(ELAPSED_WIDTH).saturating_add(1);
    if width > used.saturating_add(tail) {
        spans.push(Span::styled(
            " ".repeat(usize::from(width.saturating_sub(used).saturating_sub(tail))),
            background,
        ));
        spans.push(Span::styled(
            pad(state_word, STATE_WIDTH),
            state_style.patch(background),
        ));
        spans.push(Span::styled(
            format!(
                "{:>width$}",
                home_elapsed(row, now_unix_ms),
                width = usize::from(ELAPSED_WIDTH)
            ),
            Style::default()
                .fg(theme.agent_overlay_muted)
                .patch(background),
        ));
    }
    let drawn = spans.iter().fold(0_u16, |total, span| {
        total.saturating_add(text_width(&span.content))
    });
    spans.push(Span::styled(
        " ".repeat(usize::from(width.saturating_sub(drawn))),
        background,
    ));
    Line::from(spans).style(background)
}

/// One agent, one line:
/// `‹dot› ‹machine› ‹agent› ‹state› ‹what it is doing› ‹elapsed›`.
///
/// What a terminal too short for a card falls back to. Machine is a column
/// rather than a panel because you care about a machine almost exclusively
/// through an agent that happens to be on it. Elapsed is right-aligned and last
/// because it is the first thing worth cutting when the terminal is narrow.
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
///
/// The seconds never drop off. A minute-only clock stops moving for a whole
/// minute at a time, and a row you are waiting on that looks frozen is worse
/// than two extra characters in the narrowest column.
fn home_elapsed(row: &AgentOverlayRow, now_unix_ms: u64) -> String {
    if row.working_since_unix_ms == 0 {
        return String::new();
    }
    let elapsed = now_unix_ms
        .saturating_sub(row.working_since_unix_ms)
        .saturating_div(1_000);
    let (hours, minutes, seconds) = (elapsed / 3_600, (elapsed % 3_600) / 60, elapsed % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
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

/// The fleet down the right-hand side: every machine, two lines each.
///
/// A column rather than a footer because the space it spends was never being
/// used — the sentence an agent gets is rarely half the width of the screen —
/// and because a fleet you can see the whole time is the difference between
/// machines being part of the product and being a command you remember to run.
fn machine_rail(tui: &MultiPaneTui, theme: &UiTheme, height: u16) -> Vec<Line<'static>> {
    let machines = machine_rows(tui);
    let mut lines = vec![
        rail_line(Vec::new(), theme),
        rail_line(
            vec![Span::styled(
                format!("MACHINES · {}", machines.len()),
                Style::default()
                    .fg(theme.agent_overlay_muted)
                    .add_modifier(Modifier::BOLD),
            )],
            theme,
        ),
    ];
    // A spacer and two lines each, inside whatever the heading and the key at
    // the foot leave behind. A fleet too tall for that gives up two more lines
    // to a count of what did not fit, because a rail that simply stopped would
    // be a fleet with machines silently missing from it.
    let height = usize::from(height);
    let body = height
        .saturating_sub(lines.len())
        .saturating_sub(RAIL_FOOTER_LINES);
    let mut shown = body / RAIL_LINES_PER_MACHINE;
    if shown < machines.len() {
        shown = body.saturating_sub(2) / RAIL_LINES_PER_MACHINE;
    }
    for machine in machines.iter().take(shown) {
        lines.push(rail_line(Vec::new(), theme));
        lines.push(rail_line(
            vec![
                Span::styled(
                    if machine.reachable { "● " } else { "○ " },
                    Style::default().fg(if machine.reachable {
                        theme.agent_overlay_chrome
                    } else {
                        theme.agent_overlay_secondary
                    }),
                ),
                Span::styled(
                    truncate_trailing(&sanitize_single_line(&machine.name), RAIL_TEXT_WIDTH - 2),
                    Style::default().fg(theme.agent_overlay_foreground),
                ),
            ],
            theme,
        ));
        lines.push(rail_line(
            vec![Span::styled(
                format!(
                    "  {}",
                    truncate_trailing(&machine_detail(machine), RAIL_TEXT_WIDTH - 2)
                ),
                Style::default().fg(theme.agent_overlay_muted),
            )],
            theme,
        ));
    }
    if shown < machines.len() {
        let rest = machines.len() - shown;
        lines.push(rail_line(Vec::new(), theme));
        lines.push(rail_line(
            vec![Span::styled(
                format!("+{rest} more"),
                Style::default().fg(theme.agent_overlay_secondary),
            )],
            theme,
        ));
    }
    // The way to add another one, at the foot of the list of the ones you have,
    // which is where somebody looking at a fleet of one will be looking.
    while lines.len() + RAIL_FOOTER_LINES < height {
        lines.push(rail_line(Vec::new(), theme));
    }
    lines.push(rail_line(
        vec![Span::styled(
            "─".repeat(RAIL_TEXT_WIDTH.saturating_sub(1)),
            Style::default().fg(theme.agent_overlay_secondary),
        )],
        theme,
    ));
    lines.push(rail_line(
        vec![
            Span::styled(
                "a",
                Style::default()
                    .fg(theme.agent_overlay_chrome)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  add a machine",
                Style::default().fg(theme.agent_overlay_foreground),
            ),
        ],
        theme,
    ));
    lines
}

/// What a machine is, under its name: whether it is this one, whether it is
/// answering, and what it is running.
///
/// `accepts work` appears only when that machine actually said so. The answer
/// is given during its own pairing and has no way back here, so printing
/// anything for a machine that never said would be inventing consent.
fn machine_detail(machine: &MachineRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if machine.this_machine {
        parts.push(String::from("this machine"));
    } else if !machine.reachable {
        parts.push(String::from("asleep"));
    }
    if machine.accepts_work == Some(true) {
        parts.push(String::from("accepts work"));
    }
    match machine.agents {
        0 => {}
        1 => parts.push(String::from("1 agent")),
        count => parts.push(format!("{count} agents")),
    }
    if parts.is_empty() {
        parts.push(String::from("ready"));
    }
    parts.join(" · ")
}

/// One rail line, hung off the rule that separates it from the agents.
fn rail_line(mut spans: Vec<Span<'static>>, theme: &UiTheme) -> Line<'static> {
    let mut line = vec![Span::styled(
        "│ ",
        Style::default().fg(theme.agent_overlay_secondary),
    )];
    line.append(&mut spans);
    Line::from(line)
}

/// The same facts as a table under the agents, for a terminal too narrow to
/// give a column away.
fn machine_table(tui: &MultiPaneTui, theme: &UiTheme) -> Vec<Line<'static>> {
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
    for machine in &machine_rows(tui) {
        lines.push(Line::styled(
            format!(" {}", machine_line(machine)),
            Style::default().fg(theme.agent_overlay_foreground),
        ));
    }
    lines
}

/// Fleet health in one line, for a terminal with room for nothing else.
fn machine_strip(tui: &MultiPaneTui, theme: &UiTheme) -> Vec<Line<'static>> {
    let mut spans = vec![Span::raw(" ")];
    for machine in &machine_rows(tui) {
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
    vec![Line::raw(""), Line::from(spans)]
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
        ELAPSED_WIDTH, HOME_EMPTY_NO_AGENTS, HOME_EMPTY_NO_HOOKS, HOME_ROW_NO_HOOKS,
        format_home_row, header_line, home_card, home_elapsed, machine_detail, machine_line,
    };
    use crate::{
        agent_detect::{AgentKind, AgentState, DetectedAgent, PaneAgentTracker},
        config::UiTheme,
        protocol::AgentRosterState,
        tui::{
            MultiPaneTui,
            home::{HomeCard, MachineRow},
            test_support::agent_row,
        },
    };

    fn rendered(state: AgentRosterState, message: &str) -> String {
        let mut row = agent_row(1, 1, 1);
        row.state = state;
        row.host = String::from("droplet");
        row.kind = String::from("claude");
        row.message = message.to_owned();
        format_home_row(&row, false, 100, 0, 0, &UiTheme::default()).to_string()
    }

    /// Home drawn into a terminal of the given size, one string per screen row.
    pub(in crate::tui) fn screen(tui: &MultiPaneTui, width: u16, height: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| super::render_home(frame, tui, 0))
            .expect("render");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect()
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

    /// The seconds stay on past the first minute. A clock that only moves once
    /// a minute is indistinguishable from a clock that has stopped, which is
    /// the one thing the column exists to rule out.
    #[test]
    fn the_elapsed_column_keeps_its_seconds_at_every_length() {
        let mut row = agent_row(1, 1, 1);
        row.working_since_unix_ms = 1_000_000;
        let at = |seconds: u64| home_elapsed(&row, 1_000_000 + seconds * 1_000);

        assert_eq!(at(0), "0s");
        assert_eq!(at(45), "45s");
        assert_eq!(at(65), "1m05s");
        assert_eq!(at(3_599), "59m59s");
        assert_eq!(at(3_600), "1h00m00s");
        assert_eq!(at(35_999), "9h59m59s");

        assert!(
            at(35_999).len() <= usize::from(ELAPSED_WIDTH),
            "the column has to hold the longest clock anyone watches"
        );
        row.working_since_unix_ms = 0;
        assert!(
            home_elapsed(&row, 1_000_000).is_empty(),
            "a row with no episode behind it shows nothing rather than a zero"
        );
    }

    #[test]
    fn the_header_says_the_count_in_words_a_notification_could_reuse() {
        let theme = UiTheme::default();
        let header = |needs_you| header_line(needs_you, 0, 1, 80, &theme).to_string();

        assert_eq!(header(0).trim(), "Agents");
        assert_eq!(header(1).trim(), "Agents · 1 needs you");
        assert_eq!(header(2).trim(), "Agents · 2 need you");
    }

    /// The page marker appears only when there is a second page, and never
    /// where it could be read as part of the count: the count is of the whole
    /// list, so a page you cannot see can never be hiding something urgent.
    #[test]
    fn the_header_marks_the_page_only_when_there_is_more_than_one() {
        let theme = UiTheme::default();

        assert!(
            !header_line(2, 0, 1, 80, &theme)
                .to_string()
                .contains("page")
        );
        let paged = header_line(2, 1, 3, 80, &theme).to_string();
        assert!(paged.starts_with(" Agents · 2 need you"), "{paged:?}");
        assert!(paged.trim_end().ends_with("page 2 of 3"), "{paged:?}");
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

    /// The line under a machine's name says what it is and what it is running.
    /// A machine that is up and idle still says something — a blank line under
    /// a name reads as a machine the screen knows nothing about.
    #[test]
    fn the_line_under_a_machine_names_what_it_is_and_what_it_runs() {
        let base = MachineRow {
            name: String::from("droplet"),
            reachable: true,
            accepts_work: None,
            agents: 0,
            this_machine: false,
        };

        assert_eq!(machine_detail(&base), "ready");
        assert_eq!(
            machine_detail(&MachineRow {
                agents: 1,
                ..base.clone()
            }),
            "1 agent"
        );
        assert_eq!(
            machine_detail(&MachineRow {
                this_machine: true,
                agents: 3,
                ..base.clone()
            }),
            "this machine · 3 agents"
        );
        assert_eq!(
            machine_detail(&MachineRow {
                reachable: false,
                ..base.clone()
            }),
            "asleep"
        );
        // Never said is never printed: the answer is given during that
        // machine's own pairing and has no way back here, so anything else
        // would be inventing consent nobody gave.
        assert_eq!(
            machine_detail(&MachineRow {
                accepts_work: None,
                agents: 2,
                ..base.clone()
            }),
            "2 agents"
        );
        assert_eq!(
            machine_detail(&MachineRow {
                accepts_work: Some(true),
                agents: 2,
                ..base
            }),
            "accepts work · 2 agents"
        );
    }

    /// The card shows what a one-line row had to choose between: what the agent
    /// said, *and* which repository it said it in.
    #[test]
    fn a_card_shows_the_words_and_the_place_a_row_could_only_pick_between() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("droplet", "claude", AgentRosterState::Pending)]);
        tui.agent_rows[0].message = String::from("permission: write to /etc/hosts");
        tui.agent_rows[0].cwd = String::from("/Users/sam/work/p2pmux");
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(drawn.contains("permission: write to /etc/hosts"), "{drawn}");
        assert!(drawn.contains("work/p2pmux · tab 1 · pane 1"), "{drawn}");
        assert!(drawn.contains("needs you"), "{drawn}");
    }

    /// An agent that has said nothing falls back to its directory, and the card
    /// must not then print the directory twice.
    #[test]
    fn a_card_with_nothing_said_does_not_print_the_directory_twice() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("droplet", "claude", AgentRosterState::Working)]);
        tui.agent_rows[0].cwd = String::from("/Users/sam/work/p2pmux");
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 30).join("\n");
        assert_eq!(
            drawn.matches("work/p2pmux").count(),
            1,
            "the fallback line already is the directory: {drawn}"
        );
        assert!(drawn.contains("tab 1 · pane 1"), "{drawn}");
    }

    /// A terminal too short for cards keeps the one-line rows rather than
    /// showing one agent and a gap.
    #[test]
    fn a_short_terminal_falls_back_to_one_line_a_row() {
        let tui = crate::tui::test_support::home_tui(&[
            ("droplet", "claude", AgentRosterState::Pending),
            ("laptop", "codex", AgentRosterState::Working),
        ]);

        assert_eq!(home_card(20), HomeCard::Full);
        assert_eq!(home_card(11), HomeCard::Compact);
        assert_eq!(home_card(8), HomeCard::Row);
        let _ = tui;
    }

    /// The fleet stays on screen the whole time the inbox is up, in width the
    /// agents were never using.
    #[test]
    fn the_rail_draws_every_machine_beside_the_agents() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.snapshot.members[0].display_name = String::from("laptop");
        tui.paired_machines = vec![crate::tui::PairedMachine {
            name: String::from("oldbox"),
            accepts_work: None,
        }];
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(drawn.contains("MACHINES · 2"), "{drawn}");
        assert!(drawn.contains("● laptop"), "{drawn}");
        assert!(drawn.contains("this machine · 1 agent"), "{drawn}");
        assert!(drawn.contains("○ oldbox"), "{drawn}");
        assert!(drawn.contains("asleep"), "{drawn}");
        assert!(
            drawn.contains('│'),
            "the rail hangs off a rule rather than floating: {drawn}"
        );
        // At the foot of the machines you have, which is where somebody looking
        // for how to add another one is already looking.
        assert!(drawn.contains("add a machine"), "{drawn}");
    }

    /// A rail with more machines than lines says how many it left out. One that
    /// simply stopped would be a fleet with machines silently missing from it.
    #[test]
    fn a_rail_too_short_for_the_fleet_counts_what_it_cut() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.paired_machines = (1..=6)
            .map(|index| crate::tui::PairedMachine {
                name: format!("box{index}"),
                accepts_work: None,
            })
            .collect();
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 14).join("\n");
        assert!(drawn.contains("MACHINES · 7"), "{drawn}");
        assert!(
            drawn.contains(" more"),
            "the machines that did not fit are counted, not dropped: {drawn}"
        );
    }
}
