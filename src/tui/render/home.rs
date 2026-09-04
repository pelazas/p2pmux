//! Drawing Home: the header count, a card per agent, the machines rail, and
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
            home_layout, home_page_size, machine_rows,
        },
        render::footer::{FooterSegment, footer_segments_width, render_footer_segments},
        text::{sanitize_single_line, text_width, truncate_leading, truncate_trailing},
    },
};

/// What a first run reads when nothing is running yet.
///
/// Not "no agents found", which sounds like a failure. This says what to do,
/// and that the screen will change on its own once you do it.
pub(in crate::tui) const HOME_EMPTY_NO_AGENTS: &str =
    "Start an agent in any terminal and it appears here.";
/// The first run, at length, in the space the cards freed.
///
/// The screen is emptiest exactly when its reader is newest, so that is when
/// there is room to answer the question one grey line could not: what do I do
/// to make something appear here. The steps are in the order they have to
/// happen — the hooks first, because an inbox that cannot say `needs you` is a
/// list of processes, and running an agent before wiring them teaches that.
const HOME_EMPTY_STEPS: &[(&str, &str)] = &[
    (
        "Run `p2pmux setup` once, so your agents can say what they need.",
        "",
    ),
    (
        "Start claude, codex or opencode in any terminal — on this machine",
        "or on any machine in the list — and it appears here on its own.",
    ),
];
/// The nudge shown when agents are running but nothing is reporting on them.
///
/// This is the state a default install lands in — the hooks are opt-in — and it
/// is the single line that decides whether anyone opens the inbox twice. An
/// inbox that cannot say `needs you` is a list of processes.
pub(in crate::tui) const HOME_EMPTY_NO_HOOKS: &str =
    "Run `p2pmux setup` to see which agents need you.";
/// The nudge shown to a fleet that was paired before fleets had an address.
///
/// It outranks the hooks nudge, which is unusual and deliberate. Unreported
/// agents make the inbox less useful; a fleet with no address of its own goes
/// on working perfectly until the session it was paired around ends, and then
/// strands in a way that looks like a network fault and only a person can undo.
/// One is worth saying whenever there is room, the other is worth saying before
/// it happens.
pub(in crate::tui) const HOME_FLEET_HAS_NO_ADDRESS: &str =
    "This fleet can only meet in one session. Run `p2pmux pair` once to fix that.";
/// What sits in the description column of a row nothing has reported on.
///
/// Per-row rather than a banner over the list: the warning belongs exactly
/// where the doubt is, and it is a standing nudge toward `p2pmux setup` for as
/// long as it is true.
pub(in crate::tui) const HOME_ROW_NO_HOOKS: &str = "state unknown — no hooks";

/// `m` is back, and means something different from what it used to. It expanded
/// the strip into a table, which the rail now is permanently; it now moves the
/// cursor to the fleet, so that `n` and `enter` have a machine to be about and
/// the arrow keys have a second list to walk.
const HOME_KEYS: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" open   "),
    FooterSegment::Key("m"),
    FooterSegment::Text(" pick machine   "),
    FooterSegment::Key("a"),
    FooterSegment::Text(" add machine   "),
    FooterSegment::Key("n"),
    FooterSegment::Text(" new terminal   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];
/// The bar while the cursor is on a machine, which is when both keys that open
/// something mean "there" rather than "here".
const HOME_KEYS_MACHINE: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" terminal on this machine   "),
    FooterSegment::Key("↑↓"),
    FooterSegment::Text(" pick   "),
    FooterSegment::Key("m esc"),
    FooterSegment::Text(" back to agents   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];
/// The same bar with the paging keys, shown only while there is a second page
/// for them to reach — by the same rule that took `m` off the bar.
const HOME_KEYS_PAGED: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" open   "),
    FooterSegment::Key("h l"),
    FooterSegment::Text(" page   "),
    FooterSegment::Key("a"),
    FooterSegment::Text(" add machine   "),
    FooterSegment::Key("n"),
    FooterSegment::Text(" new terminal   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];

/// What survives when the window is too narrow for the bar above it.
///
/// Ordered by what a person cannot work out for themselves. Enter is the whole
/// screen's verb and `q` is the way out, so those are last to go; adding a
/// machine and opening a terminal are discoverable from the machines rail once
/// you are there, and paging announces itself with `page 1 of 2` in the header.
const HOME_KEYS_SHORT: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" open   "),
    FooterSegment::Key("m"),
    FooterSegment::Text(" machines   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];
const HOME_KEYS_CORE: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" open   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];
const HOME_KEYS_TIERS: &[&[FooterSegment]] = &[HOME_KEYS, HOME_KEYS_SHORT, HOME_KEYS_CORE];
const HOME_KEYS_PAGED_TIERS: &[&[FooterSegment]] =
    &[HOME_KEYS_PAGED, HOME_KEYS_SHORT, HOME_KEYS_CORE];
/// On a machine row `enter` means something else, so the short tier says which.
const HOME_KEYS_MACHINE_SHORT: &[FooterSegment] = &[
    FooterSegment::Key("enter"),
    FooterSegment::Text(" terminal there   "),
    FooterSegment::Key("esc"),
    FooterSegment::Text(" back   "),
    FooterSegment::Key("q"),
    FooterSegment::Text(" quit"),
];
const HOME_KEYS_MACHINE_TIERS: &[&[FooterSegment]] =
    &[HOME_KEYS_MACHINE, HOME_KEYS_MACHINE_SHORT, HOME_KEYS_CORE];

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
                Paragraph::new(home_empty_state(layout.rows.height, theme)),
                layout.rows,
            );
        } else {
            let animation_phase = animation_phase(now_unix_ms);
            let card = home_card(layout.rows.height);
            let lines = rows
                .into_iter()
                .skip(tui.home_page_start())
                // A page, not everything the height happens to fit: past eight
                // agents the two stop being the same number, and drawing more
                // than a page holds puts agents on screen that `h` and `l` then
                // step over, under a marker claiming a page they are not on.
                .take(home_page_size(layout.rows.height))
                .flat_map(|row| {
                    format_home_card(
                        row,
                        card,
                        // Only one list holds the cursor at a time. With it on
                        // the fleet, an agent card still wearing the selection
                        // band would make two rows look live and leave the user
                        // guessing which one Enter is about.
                        tui.home_machine.is_none()
                            && tui.home_selected.as_ref() == Some(&row.row_id()),
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
        // An answer to something the reader just did outranks a standing nudge:
        // the nudge will still be true next frame, and "that machine is asleep"
        // is only worth saying now.
        let hint = tui.home_notice.clone().unwrap_or_else(|| {
            String::from(if tui.fleet_has_no_address {
                HOME_FLEET_HAS_NO_ADDRESS
            } else {
                HOME_EMPTY_NO_HOOKS
            })
        });
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {hint}"),
                Style::default()
                    .fg(theme.agent_overlay_attention)
                    .add_modifier(Modifier::BOLD),
            )),
            layout.hint,
        );
    }

    if layout.update.height > 0
        && let Some(update) = tui.update_notice.as_ref()
    {
        // Quieter than the nudge above it, and deliberately so: an install with
        // no hooks cannot do its job, while this one only has a newer version
        // to go and get. Both are worth a line; only one is worth the bold.
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {}", update.inbox_line()),
                Style::default().fg(theme.agent_overlay_secondary),
            )),
            layout.update,
        );
    }

    if layout.machines.height > 0 {
        let lines = match layout.machine_panel {
            MachinePanel::Rail => machine_rail(tui, theme, layout.machines.height),
            MachinePanel::Table => machine_table(tui, theme, layout.machines.width),
            MachinePanel::Strip => machine_strip(tui, theme),
            MachinePanel::Empty => Vec::new(),
        };
        frame.render_widget(Paragraph::new(lines), layout.machines);
    }

    if keys.width > 0 && keys.height > 0 {
        render_home_keys(
            frame.buffer_mut(),
            theme,
            keys,
            tui.home_page_count() > 1,
            tui.home_machine.is_some(),
        );
    }
}

/// The screen a first run lands on: what is here (nothing), and what to do
/// about it.
///
/// The numbered form only where there is room for it. A terminal too short
/// falls back to the one line, which says the same thing in the space it has.
fn home_empty_state(height: u16, theme: &UiTheme) -> Vec<Line<'static>> {
    let muted = Style::default().fg(theme.agent_overlay_muted);
    // A blank, the heading, a blank, and two lines per step.
    let wanted = 3 + HOME_EMPTY_STEPS.len() * 3;
    if usize::from(height) < wanted {
        return vec![
            Line::raw(""),
            Line::styled(format!(" {HOME_EMPTY_NO_AGENTS}"), muted),
        ];
    }
    let mut lines = vec![
        Line::raw(""),
        Line::styled(
            String::from(" Nothing running yet."),
            Style::default()
                .fg(theme.agent_overlay_foreground)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for (index, (first, second)) in HOME_EMPTY_STEPS.iter().enumerate() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}  ", index + 1),
                Style::default().fg(theme.agent_overlay_chrome),
            ),
            Span::styled((*first).to_owned(), muted),
        ]));
        if !second.is_empty() {
            lines.push(Line::styled(format!("    {second}"), muted));
        }
    }
    lines
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
        return dim_unless_openable(
            row,
            vec![format_home_row(
                row,
                selected,
                width,
                now_unix_ms,
                animation_phase,
                theme,
            )],
        );
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
    dim_unless_openable(row, lines)
}

/// A card the cursor cannot stop on is drawn dim.
///
/// The difference has to be legible *before* the click rather than explained
/// after it, and dim is the one distinction that survives a themed terminal:
/// every color on this screen is configurable, so a row that said "you cannot
/// open this" by being grey would, on somebody's theme, be saying it in the
/// same grey as the line under it. The card keeps its state color — that an
/// agent is blocked is true wherever it is running, and the line below names
/// the session and the command that reaches it.
fn dim_unless_openable(row: &AgentOverlayRow, lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if row.reachable_from_here() {
        return lines;
    }
    lines
        .into_iter()
        .map(|line| {
            let dimmed = line.style.add_modifier(Modifier::DIM);
            line.style(dimmed)
        })
        .collect()
}

/// `~/work/p2pmux · tab 2 · pane 1` — where the agent is, and where Enter goes.
///
/// An agent p2pmux did not start has no tab and no pane to name, and Enter
/// there does something different: it opens a terminal on that agent's machine
/// and runs its chat command. The line says which of the two it is looking at,
/// because the rest of the card cannot be told apart.
fn home_location(row: &AgentOverlayRow, cwd_already_shown: bool) -> String {
    if row.in_another_session() {
        // The row Enter refuses, and therefore the row that has to answer
        // "where did that agent go" on its own. It names the session and the
        // command that reaches it, and — alone among the three — is never
        // prefixed with the directory: the command is the payload, and a
        // narrow terminal truncates the tail, which would cut exactly the half
        // worth reading.
        //
        // Without a name there is no command to offer, and the reason is worth
        // saying: a name comes from the session store and a store belongs to a
        // `HOME`, so a session started under a different one is visible in the
        // process table and absent from the records. Saying so beats both the
        // old answer, which called a pane two windows over `running outside
        // p2pmux`, and an `attach` line with a blank where the name goes.
        if row.session.is_empty() {
            return String::from("another p2pmux session · no record of it under this HOME");
        }
        return format!("another p2pmux session · p2pmux attach {}", row.session);
    }
    let where_it_runs = if row.outside_p2pmux() {
        // Which of the three things enter does, said on the row. An agent that
        // cannot join its own running conversation must not look like one that
        // can, and the moment to say so is before the keypress.
        let access = crate::agent_detect::AgentKind::from_wire(&row.kind)
            .map(|kind| kind.chat().access.on_a_row())
            .unwrap_or("no way in from here");
        format!("running outside p2pmux · {access}")
    } else {
        format!("tab {} · pane {}", row.tab_ordinal, row.pane_ordinal)
    };
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

/// The agent column, from the one list of agents rather than a second copy of
/// it.
///
/// This used to enumerate the kinds itself and fell a release behind: Hermes
/// and OpenClaw were detected, published, and drawn in this column as the word
/// "agent". `from_wire` already refuses anything it does not know, so deriving
/// the label from it keeps the fallback for a newer peer's agent while making
/// it impossible for a kind this build *does* know to go unnamed.
fn home_kind_label(kind: &str) -> &'static str {
    crate::agent_detect::AgentKind::from_wire(kind)
        .map(crate::agent_detect::AgentKind::wire_value)
        .unwrap_or("agent")
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
    for (index, machine) in machines.iter().enumerate().take(shown) {
        let selected = tui.home_machine == Some(index);
        lines.push(rail_line(Vec::new(), theme));
        lines.push(rail_line(
            vec![
                Span::styled(
                    machine_glyph(machine),
                    Style::default().fg(if machine.owned && machine.reachable {
                        theme.agent_overlay_chrome
                    } else {
                        theme.agent_overlay_secondary
                    }),
                ),
                Span::styled(
                    truncate_trailing(&sanitize_single_line(&machine.name), RAIL_TEXT_WIDTH - 2),
                    if selected {
                        Style::default()
                            .fg(theme.agent_overlay_foreground)
                            .bg(theme.agent_overlay_selected_background)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.agent_overlay_foreground)
                    },
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
    } else if !machine.owned {
        // Said before anything else about them, because it is the fact that
        // decides what the rest of the screen may offer to do here.
        parts.push(String::from(GUEST_DETAIL));
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

/// What a machine row says about itself when it is not one of yours.
pub(in crate::tui) const GUEST_DETAIL: &str = "joined this session";

/// The bullet in front of a machine's name.
///
/// A filled dot is compute you own and it is answering; a hollow one is yours
/// and asleep. A diamond is neither: someone collaborating from their own
/// laptop, which is not a machine that can be asleep *to you* and not one this
/// screen will ever offer to start work on.
fn machine_glyph(machine: &MachineRow) -> &'static str {
    match (machine.owned, machine.reachable) {
        (false, _) => "◇ ",
        (true, true) => "● ",
        (true, false) => "○ ",
    }
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
fn machine_table(tui: &MultiPaneTui, theme: &UiTheme, width: u16) -> Vec<Line<'static>> {
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
    let rows = machine_rows(tui);
    let (fleet, guests): (Vec<_>, Vec<_>) = rows.iter().partition(|row| row.owned);
    for machine in fleet {
        lines.push(Line::styled(
            format!(" {}", machine_line(machine, Some(width.saturating_sub(1)))),
            Style::default().fg(theme.agent_overlay_foreground),
        ));
    }
    // Their own heading rather than more rows in the fleet table. The table is
    // read as "the machines I have"; a person collaborating on this session is
    // not one, and the difference has to survive being glanced at.
    if !guests.is_empty() {
        lines.push(Line::styled(
            String::from(" IN THIS SESSION, NOT YOURS"),
            Style::default()
                .fg(theme.agent_overlay_muted)
                .add_modifier(Modifier::BOLD),
        ));
        for machine in guests {
            lines.push(Line::styled(
                format!(" {}", machine_line(machine, Some(width.saturating_sub(1)))),
                Style::default().fg(theme.agent_overlay_secondary),
            ));
        }
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
///
/// `width` is the room the line has, where the caller knows it. `None` is a
/// caller printing to a terminal it does not measure -- `p2pmux machines`,
/// whose output the shell wraps rather than clips.
pub fn machine_line(machine: &MachineRow, width: Option<u16>) -> String {
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
    let row = format!(
        "{:<12} {:<8} {:<14} {running}",
        truncate_trailing(&sanitize_single_line(&machine.name), 11),
        status,
        accepts
    );
    if !machine.this_machine {
        return row;
    }
    // The marker is the one part of this row a person can work out for
    // themselves, so it is the part to drop when the window is too narrow to
    // hold it. Clipped, it read `(this ma` hanging off the right edge.
    let suffix = "      (this machine)";
    match width {
        Some(width) if row.chars().count() + suffix.chars().count() > usize::from(width) => row,
        _ => format!("{row}{suffix}"),
    }
}

fn render_home_keys(
    buffer: &mut Buffer,
    theme: &UiTheme,
    keys: Rect,
    paged: bool,
    on_machine: bool,
) {
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
        home_footer(
            match (on_machine, paged) {
                // What the keys do now outranks how to page a list they are not on.
                (true, _) => HOME_KEYS_MACHINE_TIERS,
                (false, true) => HOME_KEYS_PAGED_TIERS,
                (false, false) => HOME_KEYS_TIERS,
            },
            keys.right().saturating_sub(keys.x.saturating_add(1)),
        ),
    );
}

/// The widest of these bars that fits, rather than the one bar clipped.
///
/// The chord footers have had this since a key pushed `Esc BACK` off a
/// 120-column terminal; the inbox's own bar never got it, and on a 60-column
/// window ended `n new ter` — which does not name a key, does not name what it
/// does, and reads as a rendering fault. Dropping a whole hint says less and
/// says it correctly.
fn home_footer(tiers: &[&'static [FooterSegment]], width: u16) -> &'static [FooterSegment] {
    tiers
        .iter()
        .copied()
        .find(|tier| footer_segments_width(tier) <= width)
        .unwrap_or_else(|| tiers.last().copied().unwrap_or(HOME_KEYS_CORE))
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
        ELAPSED_WIDTH, GUEST_DETAIL, HOME_EMPTY_NO_AGENTS, HOME_EMPTY_NO_HOOKS, HOME_KEYS_CORE,
        HOME_KEYS_MACHINE_TIERS, HOME_KEYS_PAGED_TIERS, HOME_KEYS_TIERS, HOME_ROW_NO_HOOKS,
        format_home_row, header_line, home_card, home_elapsed, home_footer, home_kind_label,
        machine_detail, machine_line,
    };
    use crate::tui::render::footer::{FooterSegment, footer_segments_width};
    use crate::{
        agent_detect::{AgentKind, AgentState, DetectedAgent, PaneAgentTracker},
        config::UiTheme,
        protocol::AgentRosterState,
        tui::{
            MultiPaneTui,
            home::{HOME_PAGE_MAX, HomeCard, MachineRow},
            test_support::agent_row,
        },
    };

    /// A page has to be what the screen *draws*, not only what `h` and `l` step
    /// by. A tall terminal has room for more agents than a page holds, and
    /// filling it drew agents that paging then stepped straight over, under a
    /// marker naming a page they were not on.
    #[test]
    fn a_tall_terminal_draws_one_page_rather_than_everything_that_fits() {
        use ratatui::layout::Rect;

        let agents = (0..9)
            .map(|_| ("laptop", "claude", AgentRosterState::Working))
            .collect::<Vec<_>>();
        let mut tui = crate::tui::test_support::home_tui(&agents);
        tui.set_home_open(true, "test");
        tui.set_home_viewport_for(Rect::new(0, 0, 120, 60));

        assert_eq!(tui.home_page_count(), 2, "nine agents is more than a page");
        let drawn = screen(&tui, 120, 60)
            .iter()
            .filter(|line| line.contains("claude"))
            .count();
        assert_eq!(
            drawn, HOME_PAGE_MAX,
            "a page holds eight however tall the terminal is"
        );
    }

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

    /// Issue #77: a release nobody is told about is a release nobody installs.
    /// It has to say the version *and* the command, because "an update is
    /// available" with no way to take it is a line people learn to skip.
    #[test]
    fn a_new_release_says_so_on_the_inbox_with_the_command_to_take_it() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("mac", "claude", AgentRosterState::Working)]);
        tui.set_home_open(true, "test");
        assert!(!screen(&tui, 120, 30).join("\n").contains("is out"));

        let notice = crate::update_check::UpdateNotice {
            version: String::from("9.9.9"),
            command: "brew update && brew upgrade p2pmux",
        };
        assert!(tui.set_update_notice(notice.clone()));
        // Once. A repeat answer from a later check costs no repaint.
        assert!(!tui.set_update_notice(notice));

        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(drawn.contains("9.9.9 is out"), "{drawn}");
        assert!(drawn.contains("brew upgrade p2pmux"), "{drawn}");
        assert!(drawn.contains("u update"), "{drawn}");

        // The setup nudge keeps its own line too: an install that cannot say
        // `needs you` and an install that is a version behind are two different
        // problems, and the one that loses a shared slot is the one nobody sees.
        let mut unwired =
            crate::tui::test_support::home_tui(&[("mac", "claude", AgentRosterState::Unknown)]);
        unwired.set_home_open(true, "test");
        assert!(unwired.set_update_notice(crate::update_check::UpdateNotice {
            version: String::from("9.9.9"),
            command: "brew update && brew upgrade p2pmux",
        }));
        let drawn = screen(&unwired, 120, 30).join("\n");
        assert!(drawn.contains(HOME_EMPTY_NO_HOOKS), "{drawn}");
        assert!(drawn.contains("9.9.9 is out"), "{drawn}");

        // A fleet that can only meet in one session outranks the setup nudge.
        // Unreported agents make the inbox less useful; a fleet with no address
        // works perfectly until the day it strands, and only a person can undo
        // that — so it is worth saying before it happens rather than after.
        let mut stranding =
            crate::tui::test_support::home_tui(&[("mac", "claude", AgentRosterState::Unknown)]);
        stranding.set_home_open(true, "test");
        assert!(stranding.set_fleet_has_no_address(true));
        let drawn = screen(&stranding, 120, 30).join("\n");
        assert!(drawn.contains("p2pmux pair"), "{drawn}");
        assert!(!drawn.contains(HOME_EMPTY_NO_HOOKS), "{drawn}");

        // It has a line of its own, so an answer to something the reader just
        // did does not push it off the screen — the two are different messages
        // and the loser of one slot is the one nobody would ever see.
        tui.home_notice = Some(String::from("that machine is asleep"));
        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(drawn.contains("that machine is asleep"), "{drawn}");
        assert!(drawn.contains("9.9.9 is out"), "{drawn}");
    }

    /// The screen is emptiest exactly when its reader is newest, so that is
    /// when there is room to say what to do rather than only that nothing is
    /// here — and the hooks come first, because starting an agent before
    /// wiring them teaches an inbox that cannot say `needs you`.
    #[test]
    fn a_first_run_gets_the_steps_rather_than_one_grey_line() {
        let mut tui = crate::tui::test_support::home_tui(&[]);
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(drawn.contains("Nothing running yet."), "{drawn}");
        let setup = drawn.find("p2pmux setup").expect("the first step");
        let start = drawn.find("Start claude").expect("the second step");
        assert!(setup < start, "wiring the hooks comes first: {drawn}");
        assert!(drawn.contains(" 1  "), "{drawn}");
        assert!(drawn.contains(" 2  "), "{drawn}");
    }

    /// A terminal too short for the steps says the same thing in one line
    /// rather than showing half a checklist.
    #[test]
    fn a_short_first_run_keeps_the_one_line_it_has_room_for() {
        let mut tui = crate::tui::test_support::home_tui(&[]);
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 8).join("\n");
        assert!(drawn.contains(HOME_EMPTY_NO_AGENTS), "{drawn}");
        assert!(!drawn.contains("Nothing running yet."), "{drawn}");
    }

    #[test]
    fn an_unanswered_accepts_work_column_reads_as_a_dash_not_a_refusal() {
        let here = MachineRow {
            name: String::from("laptop"),
            peer_id: None,
            owned: true,
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

        let line = machine_line(&here, None);
        assert!(
            line.contains('—'),
            "a dash means never said, not a refusal nobody made: {line:?}"
        );
        assert!(line.contains("2 agents"), "{line:?}");
        assert!(line.contains("(this machine)"), "{line:?}");
        assert!(machine_line(&there, None).contains("yes"));
        assert!(machine_line(&there, None).contains("1 agent"));
    }

    /// The bar narrows by dropping whole hints, not by clipping a word.
    ///
    /// `n new ter` names no key and no action; it reads as a rendering fault.
    /// The chord footers have had tiers since one of them lost `Esc BACK` off
    /// the end of a 120-column terminal, and this is the same rule for the
    /// screen bare `p2pmux` opens on.
    #[test]
    fn the_inbox_bar_drops_hints_it_cannot_fit_whole() {
        for width in [30u16, 40, 50, 60, 70, 80, 120] {
            for tiers in [
                HOME_KEYS_TIERS,
                HOME_KEYS_PAGED_TIERS,
                HOME_KEYS_MACHINE_TIERS,
            ] {
                let chosen = home_footer(tiers, width);
                let drawn = footer_segments_width(chosen);
                assert!(
                    drawn <= width || chosen == HOME_KEYS_CORE,
                    "at {width} columns the bar drew {drawn}: {chosen:?}"
                );
                assert!(
                    chosen
                        .iter()
                        .any(|segment| matches!(segment, FooterSegment::Key("enter"))),
                    "every tier keeps the key the whole screen is about: {chosen:?}"
                );
            }
        }
    }

    /// On a narrow window the row drops its marker rather than hanging off the
    /// edge, where it was clipped to `(this ma`.
    #[test]
    fn a_narrow_machine_row_drops_the_marker_rather_than_half_of_it() {
        let here = MachineRow {
            name: String::from("host"),
            peer_id: None,
            owned: true,
            reachable: true,
            accepts_work: None,
            agents: 6,
            this_machine: true,
        };

        let roomy = machine_line(&here, Some(80));
        assert!(roomy.contains("(this machine)"), "{roomy:?}");

        let narrow = machine_line(&here, Some(58));
        assert!(
            !narrow.contains("(this"),
            "a marker that does not fit is left out whole: {narrow:?}"
        );
        assert!(
            narrow.chars().count() <= 58,
            "and what is left fits: {narrow:?}"
        );
        assert!(narrow.contains("6 agents"), "the facts stay: {narrow:?}");
    }

    #[test]
    fn an_unreachable_machine_reads_asleep() {
        let asleep = MachineRow {
            name: String::from("oldbox"),
            peer_id: None,
            owned: true,
            reachable: false,
            accepts_work: Some(true),
            agents: 0,
            this_machine: false,
        };

        assert!(machine_line(&asleep, None).contains("asleep"));
    }

    /// The line under a machine's name says what it is and what it is running.
    /// A machine that is up and idle still says something — a blank line under
    /// a name reads as a machine the screen knows nothing about.
    #[test]
    fn the_line_under_a_machine_names_what_it_is_and_what_it_runs() {
        let base = MachineRow {
            name: String::from("droplet"),
            peer_id: None,
            owned: true,
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

    /// Issue #121: the same card, for a session this machine cannot name.
    ///
    /// A name comes from the session store and a store belongs to a `HOME`, so
    /// two sessions started on one box under two of them — an ad-hoc script
    /// with its own sandbox, a session started under `sudo` — see each other's
    /// processes and not each other's records. The row used to read `running
    /// outside p2pmux`, and offer a keypress that starts a *second* copy of an
    /// agent already running two windows over.
    #[test]
    fn an_agent_in_a_session_this_home_cannot_name_still_says_which_it_is() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        let mut rows = tui.agent_rows.clone();
        rows.push(crate::tui::AgentOverlayRow {
            pane_id: 0,
            process_pid: 985,
            host: String::from("laptop"),
            kind: String::from("claude"),
            state: AgentRosterState::Pending,
            // The node above it was found; the name for that node was not.
            session: String::new(),
            in_another_session: true,
            ..agent_row(0, 0, 0)
        });
        tui.set_agent_rows(rows);
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(
            drawn.contains("another p2pmux session · no record of it under this HOME"),
            "the row says which of the two things it is, and why it can offer no command: {drawn}"
        );
        assert!(
            !drawn.contains("running outside p2pmux"),
            "and never claims a pane two windows over is not in p2pmux at all: {drawn}"
        );
        assert!(
            !drawn.contains("p2pmux attach "),
            "there is no name to attach to, so no attach line is offered: {drawn}"
        );
    }

    /// The card the click used to send the user nowhere. It has to say — before
    /// any keypress, and without needing one — which session that agent is in
    /// and what reaches it, and it has to look unlike the rows Enter opens.
    #[test]
    fn an_agent_in_another_session_is_drawn_dim_and_names_the_way_in() {
        use ratatui::{Terminal, backend::TestBackend, style::Modifier};

        let mut tui =
            crate::tui::test_support::home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        let mut rows = tui.agent_rows.clone();
        rows.push(crate::tui::AgentOverlayRow {
            // No pane of this session's, and a session name: an agent left
            // behind in a p2pmux nobody here is attached to.
            pane_id: 0,
            process_pid: 985,
            host: String::from("laptop"),
            kind: String::from("claude"),
            state: AgentRosterState::Pending,
            session: String::from("dakar"),
            in_another_session: true,
            ..agent_row(0, 0, 0)
        });
        tui.set_agent_rows(rows);
        tui.set_home_open(true, "test");

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| super::render_home(frame, &tui, 0))
            .expect("render");
        let buffer = terminal.backend().buffer().clone();
        let line_holding = |needle: &str| -> u16 {
            (0..30u16)
                .find(|row| {
                    (0..120u16)
                        .map(|column| buffer[(column, *row)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("nothing on screen holds {needle}"))
        };

        let detached = line_holding("another p2pmux session · p2pmux attach dakar");
        assert!(
            buffer[(2, detached)].modifier.contains(Modifier::DIM),
            "the row Enter refuses has to look like one"
        );
        assert!(
            buffer[(2, detached.saturating_sub(2))]
                .modifier
                .contains(Modifier::DIM),
            "the whole card is dim, not only the line naming the session"
        );
        let openable = line_holding("tab 1 · pane 1");
        assert!(
            !buffer[(2, openable)].modifier.contains(Modifier::DIM),
            "and the agent Enter does open must not be dimmed with it"
        );
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
            machine_id: None,
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

    /// The line this whole issue is about, drawn rather than merely computed.
    ///
    /// A stranger who joined with a ticket and a droplet paired six months ago
    /// used to render identically. They must not: everything the screen goes on
    /// to offer — start a terminal there, keep it in every session — is only a
    /// safe thing to offer about compute you own.
    #[test]
    fn a_person_who_joined_does_not_render_as_one_of_your_machines() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.snapshot.members[0].display_name = String::from("laptop");
        tui.snapshot.members.push(crate::layout::Member {
            peer_id: vec![0xca, 0xfe, 0xca, 0xfe],
            endpoint_addr: vec![2],
            display_name: String::from("sam"),
            // Says it is a machine, and is still not one of yours: the claim is
            // not in the pairing record, and only the record can put it there.
            kind: crate::layout::MemberKind::Machine,
            machine_proof: Default::default(),
            machine_id: Default::default(),
        });
        tui.paired_machines = vec![crate::tui::PairedMachine {
            name: String::from("droplet"),
            machine_id: None,
            accepts_work: None,
        }];
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 120, 30).join("\n");
        assert!(
            drawn.contains("◇ sam"),
            "a guest gets its own mark: {drawn}"
        );
        assert!(
            drawn.contains(GUEST_DETAIL),
            "and its own words, said before anything else about it: {drawn}"
        );
        assert!(
            drawn.contains("● laptop") && drawn.contains("○ droplet"),
            "while your machines keep the marks that mean awake and asleep: {drawn}"
        );
    }

    /// The narrow tier makes the same distinction with a heading, because a
    /// table read as "the machines I have" must not quietly contain someone
    /// else's laptop.
    #[test]
    fn the_table_puts_people_under_their_own_heading() {
        let mut tui =
            crate::tui::test_support::home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.snapshot.members[0].display_name = String::from("laptop");
        tui.snapshot.members.push(crate::layout::Member {
            peer_id: vec![0xca, 0xfe, 0xca, 0xfe],
            endpoint_addr: vec![2],
            display_name: String::from("sam"),
            kind: crate::layout::MemberKind::Person,
            machine_proof: Default::default(),
            machine_id: Default::default(),
        });
        tui.set_home_open(true, "test");

        let drawn = screen(&tui, 70, 20).join("\n");
        assert!(drawn.contains("IN THIS SESSION, NOT YOURS"), "{drawn}");
        let fleet_heading = drawn.find("NAME").expect("the fleet heading");
        let guest_heading = drawn.find("IN THIS SESSION").expect("the guest heading");
        assert!(
            fleet_heading < guest_heading,
            "your machines come first: {drawn}"
        );
    }

    /// The agent column named Hermes and OpenClaw "agent" for a release,
    /// because it kept its own list of the kinds it knew. It reads the one
    /// list now, and this is the test that says so.
    #[test]
    fn every_agent_this_build_knows_is_named_in_the_agent_column() {
        for kind in [
            crate::agent_detect::AgentKind::Claude,
            crate::agent_detect::AgentKind::Codex,
            crate::agent_detect::AgentKind::Cursor,
            crate::agent_detect::AgentKind::Pi,
            crate::agent_detect::AgentKind::OpenCode,
            crate::agent_detect::AgentKind::Hermes,
            crate::agent_detect::AgentKind::OpenClaw,
        ] {
            assert_eq!(
                home_kind_label(kind.wire_value()),
                kind.wire_value(),
                "{kind:?} is drawn as something other than its own name"
            );
        }
        assert_eq!(
            home_kind_label("an-agent-from-a-newer-build"),
            "agent",
            "a kind this build does not know still gets a word rather than a blank"
        );
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
                machine_id: None,
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
