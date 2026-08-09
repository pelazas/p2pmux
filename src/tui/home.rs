//! Home: the inbox screen you open, and what the keys on it do.
//!
//! Home is a local, client-side screen that sits *above* the tabs. It is not a
//! tab and not an overlay, and that is a deliberate structural choice rather
//! than an implementation shortcut:
//!
//! - Tabs are shared session state, replicated and signed. The inbox is one
//!   person's view of their own agents across machines, so making it a tab
//!   would push a private screen onto everyone else, spend one of the nine
//!   tabs, and sit at the wrong altitude — the inbox spans machines, a tab
//!   lives inside one session's layout.
//! - Keeping it local means it never touches layout or replication code. The
//!   whole screen is `agent_rows` — which the node already collects from every
//!   peer — sorted differently and drawn differently.
//!
//! It renders in the tab bar so that it *feels* like a tab. It is not one.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use crate::{
    layout::PaneId,
    protocol::AgentRosterState,
    tui::{AgentOverlayRow, ModalState, MultiPaneTui, UiIntent, debug_log::ui_debug_log},
};

/// Where a state sorts in the inbox. Higher comes first.
///
/// Deliberately *not* [`AgentRosterState::severity`], which the overlay and the
/// tab dots use. Severity ranks by how alarming a state is, so `working`
/// outranks `done`. The inbox ranks by how much a row wants a human, and a
/// finished agent wants one while a busy agent does not — the spec's
/// `blocked → done → running → idle`. A row that needs you must never appear
/// below one that does not; that sort order *is* the product.
pub(in crate::tui) fn home_rank(state: AgentRosterState) -> u8 {
    match state {
        AgentRosterState::Error => 5,
        AgentRosterState::Pending => 4,
        AgentRosterState::Done => 3,
        AgentRosterState::Working => 2,
        // An agent whose hooks never fired is still a running process, and the
        // row says so honestly in its own column. It sorts with the other
        // running rows rather than below `idle`, because "running, and I cannot
        // tell you more" is closer to running than to known-idle.
        AgentRosterState::Unknown => 2,
        AgentRosterState::Idle => 1,
    }
}

impl MultiPaneTui {
    pub fn home_open(&self) -> bool {
        self.home_open
    }

    /// Open or close Home. Returns whether anything changed, so a repeated key
    /// never costs a repaint.
    pub(in crate::tui) fn set_home_open(&mut self, open: bool, reason: &str) -> bool {
        if self.home_open == open {
            return false;
        }
        self.home_open = open;
        if open {
            // Any modal belongs to the screen underneath. Landing on Home with
            // a share panel still floating over it would be a dialog with no
            // owner.
            self.modal = ModalState::None;
            self.exit_chord_mode();
            // Arriving puts the cursor back on the top row, which is the one
            // that most wants a human. That is the whole reason for the sort
            // order, and a cursor left where a previous visit happened to end
            // would mean Enter on arrival opens whatever was urgent last time.
            //
            // Only on arrival. While Home is open the cursor stays exactly
            // where the user put it, and a row changing state under it must
            // never drag it somewhere else.
            self.home_selected = None;
            self.home_page = 0;
            self.repair_home_selection();
        }
        ui_debug_log(
            if open { "home_open" } else { "home_close" },
            format_args!("reason={reason}"),
        );
        true
    }

    /// The inbox rows, in the order they are drawn.
    ///
    /// Sorted by [`home_rank`], then stably by machine, agent and pane. The
    /// secondary key is deliberately not elapsed time: this list repaints
    /// several times a second, and a row that slides under the cursor because a
    /// timer ticked is worse than any ordering that key could buy.
    pub(in crate::tui) fn home_rows(&self) -> Vec<&AgentOverlayRow> {
        let mut rows = self.agent_rows.iter().collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            home_rank(right.state)
                .cmp(&home_rank(left.state))
                .then_with(|| left.host.cmp(&right.host))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.pane_id.cmp(&right.pane_id))
        });
        rows
    }

    /// How many rows are blocked on a human. The header count, the tab-bar
    /// badge and the eventual notification all read this one number.
    pub fn home_needs_you_count(&self) -> usize {
        self.agent_rows
            .iter()
            .filter(|row| row.state.needs_you())
            .count()
    }

    /// Whether every agent on screen is unreported. Drives the onboarding empty
    /// state — the one that tells a first-time user to run `p2pmux setup`.
    pub(in crate::tui) fn home_all_unwired(&self) -> bool {
        !self.agent_rows.is_empty()
            && self
                .agent_rows
                .iter()
                .all(|row| row.state == AgentRosterState::Unknown)
    }

    /// Keeps the cursor on a row that still exists, and on the first row when
    /// it does not.
    pub(in crate::tui) fn repair_home_selection(&mut self) {
        let rows = self
            .home_rows()
            .into_iter()
            .map(|row| row.pane_id)
            .collect::<Vec<_>>();
        if self
            .home_selected
            .is_none_or(|pane_id| !rows.contains(&pane_id))
        {
            self.home_selected = rows.first().copied();
        }
        self.clamp_home_page();
        self.ensure_home_selection_visible();
    }

    pub(in crate::tui) fn set_home_viewport(&mut self, page_size: usize) {
        self.home_page_size = page_size.max(1);
        self.clamp_home_page();
    }

    /// Tell Home how many agents fit on a page, from the whole terminal rather
    /// than from a count the caller had to work out for itself.
    pub fn set_home_viewport_for(&mut self, area: Rect) {
        let rows = home_layout(self.geometry(area).content, self).rows.height;
        self.set_home_viewport(home_page_size(rows));
    }

    /// Wheel over the inbox. Returns whether anything moved, so a scroll at the
    /// end of the list never costs a repaint.
    ///
    /// A page at a time rather than an agent at a time: the list is paged, and
    /// a wheel that slid one card off the top would leave a page nobody chose,
    /// with the first agent half in view.
    pub fn scroll_home(&mut self, area: Rect, up: bool) -> bool {
        self.set_home_viewport_for(area);
        let previous = self.home_page;
        if up {
            self.home_page = self.home_page.saturating_sub(1);
        } else {
            self.home_page = self.home_page.saturating_add(1);
        }
        self.clamp_home_page();
        if self.home_page != previous {
            // The cursor follows the page. Leaving it behind on a page that is
            // no longer drawn means Enter opens an agent that is not on screen.
            self.home_selected = self
                .home_rows()
                .get(self.home_page.saturating_mul(self.home_page_size.max(1)))
                .map(|row| row.pane_id);
            return true;
        }
        false
    }

    /// How many pages the list has. Never zero: an empty inbox is one page.
    pub(in crate::tui) fn home_page_count(&self) -> usize {
        self.agent_rows
            .len()
            .div_ceil(self.home_page_size.max(1))
            .max(1)
    }

    pub(in crate::tui) fn home_page(&self) -> usize {
        self.home_page
    }

    /// The index of the first agent on the page being drawn.
    pub(in crate::tui) fn home_page_start(&self) -> usize {
        self.home_page.saturating_mul(self.home_page_size.max(1))
    }

    /// A click on an inbox row selects it and opens it, in one gesture.
    ///
    /// One click rather than select-then-Enter: the row already says everything
    /// there is to know about it, so there is nothing a selected-but-unopened
    /// row would let the user read.
    pub(in crate::tui) fn handle_home_click(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Vec<UiIntent> {
        let Some(pane_id) = self.home_row_at(column, row, area) else {
            return Vec::new();
        };
        self.home_selected = Some(pane_id);
        self.enter_pane_from_home(pane_id)
    }

    pub(in crate::tui) fn home_row_at(&self, column: u16, row: u16, area: Rect) -> Option<PaneId> {
        let layout = home_layout(self.geometry(area).content, self);
        if !crate::tui::geometry::rect_contains(layout.rows, column, row) {
            return None;
        }
        let line = usize::from(row.saturating_sub(layout.rows.y));
        let card = home_card(layout.rows.height).lines();
        // A click on a card's blank spacer belongs to the card above it, which
        // is the one the pointer looks like it is on.
        self.home_rows()
            .get(self.home_page_start().saturating_add(line / card))
            .map(|row| row.pane_id)
    }

    pub(in crate::tui) fn clamp_home_page(&mut self) {
        self.home_page = self.home_page.min(self.home_page_count().saturating_sub(1));
    }

    /// Puts the page the cursor is on on screen.
    ///
    /// The selection is what the page follows, never the other way round: the
    /// sort order decides which agent most wants a human, and a page that
    /// stayed put while the cursor walked off it would be a screen showing
    /// agents nobody chose.
    pub(in crate::tui) fn ensure_home_selection_visible(&mut self) {
        let Some(index) = self.home_selected.and_then(|pane_id| {
            self.home_rows()
                .iter()
                .position(|row| row.pane_id == pane_id)
        }) else {
            return;
        };
        self.home_page = index / self.home_page_size.max(1);
        self.clamp_home_page();
    }

    pub(in crate::tui) fn move_home_selection(&mut self, forward: bool) {
        let rows = self
            .home_rows()
            .into_iter()
            .map(|row| row.pane_id)
            .collect::<Vec<_>>();
        if rows.is_empty() {
            self.home_selected = None;
            return;
        }
        let current = self
            .home_selected
            .and_then(|pane_id| rows.iter().position(|id| *id == pane_id))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % rows.len()
        } else {
            (current + rows.len() - 1) % rows.len()
        };
        self.home_selected = Some(rows[next]);
        self.ensure_home_selection_visible();
    }

    /// Enter: leave Home and land in the selected agent's terminal, alone on
    /// screen.
    ///
    /// Full screen rather than the pane grid, because Home is what you open and
    /// the terminal is what you land in — arriving in a four-way split means
    /// hunting for the agent you just selected by name.
    pub(in crate::tui) fn open_home_selection(&mut self) -> Vec<UiIntent> {
        let Some(pane_id) = self.home_selected else {
            return Vec::new();
        };
        self.enter_pane_from_home(pane_id)
    }

    pub(in crate::tui) fn enter_pane_from_home(&mut self, pane_id: PaneId) -> Vec<UiIntent> {
        let Some(tab) = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| crate::tui::geometry::contains_leaf(&tab.root, pane_id))
        else {
            return Vec::new();
        };
        let tab_id = tab.tab_id;
        self.select_pane(tab_id, pane_id, "home_enter");
        self.zoomed_pane = Some(pane_id);
        self.set_home_open(false, "enter");
        vec![UiIntent::FocusPane { pane_id }]
    }

    /// The pane drawn alone, when Home handed the user into one.
    ///
    /// Cleared whenever the pane stops existing or focus moves elsewhere, so a
    /// stale zoom can never hide the rest of a tab.
    pub(in crate::tui) fn zoomed_pane(&self) -> Option<PaneId> {
        self.zoomed_pane
            .filter(|pane_id| *pane_id == self.focused_pane)
            .filter(|pane_id| self.snapshot.panes.contains_key(pane_id))
    }

    pub(in crate::tui) fn clear_zoom(&mut self) {
        self.zoomed_pane = None;
    }

    /// Give the focused pane the whole content area, or give it back.
    ///
    /// The same local view state Home already uses to hand you into an agent,
    /// reachable on purpose rather than only as a side effect of arriving from
    /// the inbox. Nothing about the layout changes: the pane keeps the grid the
    /// session gave it and simply stops sharing the screen with its siblings,
    /// so no other member sees anything happen.
    ///
    /// A tab with one pane is already zoomed, so the key does nothing there
    /// rather than lighting a badge that describes no change.
    pub(in crate::tui) fn toggle_zoom(&mut self) -> bool {
        if self.zoomed_pane().is_some() {
            self.clear_zoom();
            return true;
        }
        let siblings = self
            .current_tab_layout()
            .map(|tab| crate::tui::geometry::visible_leaf_panes(&tab.root).len())
            .unwrap_or_default();
        if siblings < 2 {
            return false;
        }
        self.zoomed_pane = Some(self.focused_pane);
        true
    }

    /// Which peer this client is, so the machine list can mark the row for the
    /// machine the user is sitting at. Only the node knows it.
    pub fn set_local_peer_id(&mut self, peer_id: Vec<u8>) {
        self.local_peer_id = Some(peer_id);
    }

    /// The machines paired with this one, read from the pairing record.
    ///
    /// A paired machine that is not a session member is one you own that is not
    /// answering, and the strip says `asleep` rather than dropping it.
    pub fn set_paired_machines(&mut self, machines: Vec<crate::tui::PairedMachine>) -> bool {
        if self.paired_machines == machines {
            return false;
        }
        self.paired_machines = machines;
        true
    }

    /// Land on Home rather than in the session. Set once, before the first
    /// frame, by a client that was started with no session named.
    pub fn open_home_on_start(&mut self) {
        self.set_home_open(true, "start");
    }

    /// Keys on Home.
    ///
    /// Unmodified letters are safe to claim here in a way they never are inside
    /// a pane: Home is the mux's own screen, and no program is listening.
    pub(in crate::tui) fn handle_home_key(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> crate::tui::KeyHandling {
        use crate::tui::KeyHandling;
        self.set_home_viewport_for(area);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                self.move_home_selection(false);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                self.move_home_selection(true);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                KeyHandling::Consumed(self.open_home_selection())
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let (grid_rows, grid_cols) =
                    crate::tui::geometry::grid_for_pane(self.geometry(area).content);
                // A new terminal, not a split: there is no pane in view to
                // split against, and a first run with no agents needs something
                // to do other than read an empty list.
                self.set_home_open(false, "new_terminal");
                self.clear_zoom();
                KeyHandling::Consumed(vec![UiIntent::CreateTab {
                    grid_rows,
                    grid_cols,
                }])
            }
            KeyCode::Char('a') if key.modifiers.is_empty() => {
                // Adding a machine used to mean leaving the screen the fleet is
                // on: `p2pmux pair` in a terminal, then finding out whether it
                // worked by running something else. Both halves belong here.
                self.open_add_machine();
                KeyHandling::Consumed(vec![])
            }
            // The same question Ctrl+Q asks, asked the same way. Home is the
            // one screen where a bare `q` is free, but it should not be the one
            // screen where leaving skips the prompt.
            KeyCode::Char('q') if key.modifiers.is_empty() => {
                if self.detachable {
                    self.open_quit_prompt()
                } else {
                    KeyHandling::Quit(crate::tui::QuitAction::Detach)
                }
            }
            KeyCode::Tab | KeyCode::Right if key.modifiers.is_empty() => {
                // Home sits left of Tab #1, so stepping right off it lands on
                // the tabs — the second path in, for people who navigate by tab.
                self.set_home_open(false, "tab_right");
                self.clear_zoom();
                KeyHandling::Consumed(vec![])
            }
            // Everything else, `Esc` included, is swallowed rather than
            // forwarded. There is no pane in view to forward it to, and a
            // stray key reaching a terminal the user cannot see is worse than
            // a key that does nothing.
            _ => KeyHandling::Consumed(vec![]),
        }
    }
}

/// Where the parts of the Home screen are drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) struct HomeLayout {
    /// `Agents · 2 need you`, with a blank line under it.
    pub(in crate::tui) header: Rect,
    /// The agent rows, one line each. Also where an empty state is written.
    pub(in crate::tui) rows: Rect,
    /// The `p2pmux setup` nudge. Zero-height unless every row is unreported.
    pub(in crate::tui) hint: Rect,
    /// Where the machines go. Zero-height only before the member list has
    /// arrived, and shaped by [`HomeLayout::machine_panel`].
    pub(in crate::tui) machines: Rect,
    pub(in crate::tui) machine_panel: MachinePanel,
}

/// How the fleet is drawn, which is a question of how much room there is.
///
/// The screen's spare space is horizontal as much as vertical — the column that
/// says what an agent is doing is rarely more than half used — so the widest
/// tier spends that width on machines rather than leaving it blank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum MachinePanel {
    /// A column down the right-hand side, with a line each for what a machine
    /// is and what it is running.
    Rail,
    /// The same facts as a table under the agents, when the terminal is too
    /// narrow to give a column away.
    Table,
    /// Names and ticks on one line, when there is not even room for a table.
    /// It still answers "is my fleet up", which is what earns it the space.
    Strip,
    /// Nothing known yet. The member list has not arrived.
    Empty,
}

/// How much of the screen one agent gets.
///
/// A line each was the right answer when the screen was a list of processes.
/// It is the wrong one for a fleet: nobody runs thirty agents, so the list
/// occupied a fifth of a terminal and left the rest blank, and a row that had
/// to fit in one line could show what an agent said *or* which repository it
/// said it in, never both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum HomeCard {
    /// Who and where, what it said, and where Enter will land you.
    Full,
    /// Who and where, and what it said. The location line is the first thing
    /// worth dropping: it is the only one you can get by pressing Enter.
    Compact,
    /// One line, as it was. What a terminal too short for anything else gets.
    Row,
}

impl HomeCard {
    /// Lines on screen per agent, including the blank that separates two cards.
    pub(in crate::tui) fn lines(self) -> usize {
        match self {
            Self::Full => 4,
            Self::Compact => 3,
            Self::Row => 1,
        }
    }
}

/// The richest card the agent list has room for.
///
/// Chosen from the height alone, never from how many agents there are: a screen
/// that changed shape when a fifth agent appeared would be a screen you have to
/// re-read every time the fleet moves.
pub(in crate::tui) fn home_card(rows_height: u16) -> HomeCard {
    // Three cards' worth, so a tier is only taken when it can show a list
    // rather than a single agent and a gap.
    if rows_height >= 12 {
        HomeCard::Full
    } else if rows_height >= 9 {
        HomeCard::Compact
    } else {
        HomeCard::Row
    }
}

/// The most agents one page shows.
///
/// Not a space limit — a tall terminal fits more. It is the number of things a
/// screen can be glanced at rather than read, and the inbox is sorted so that
/// what is on page one is what most wants a human.
pub(in crate::tui) const HOME_PAGE_MAX: usize = 8;

/// How many agents fit on a page of a list this tall.
pub(in crate::tui) fn home_page_size(rows_height: u16) -> usize {
    (usize::from(rows_height) / home_card(rows_height).lines()).clamp(1, HOME_PAGE_MAX)
}

/// How wide the rail is, including the rule it hangs off.
pub(in crate::tui) const MACHINE_RAIL_WIDTH: u16 = 28;
/// The narrowest terminal that gets a rail.
///
/// Below this the agents would be paying for the fleet: a card whose sentence
/// column is under 40 columns truncates what the agent said, which is the one
/// thing on the screen worth reading in full.
const MACHINE_RAIL_MIN_WIDTH: u16 = 88;
/// The shortest terminal that gets a rail: a heading, a blank, and one machine.
const MACHINE_RAIL_MIN_HEIGHT: u16 = 6;

/// Every machine this client knows about, session members first.
///
/// A member is in the session and therefore reachable. A paired machine that is
/// not a member is one you own that is not answering — asleep, off, or without
/// a node running — and saying so is the whole reason the strip exists.
pub(in crate::tui) fn machine_rows(tui: &MultiPaneTui) -> Vec<MachineRow> {
    let mut rows = tui
        .snapshot
        .members
        .iter()
        .map(|member| {
            // Keyed on the member *label*, not the raw display name, because
            // that is what the agent rows carry and what the strip prints. Two
            // machines that chose the same name get disambiguated there, and
            // matching on the raw name would credit one's agents to the other.
            let name = crate::tui::member_label(&member.peer_id, &tui.snapshot.members);
            MachineRow {
                accepts_work: tui
                    .paired_machines
                    .iter()
                    .find(|paired| paired.name == name)
                    .and_then(|paired| paired.accepts_work),
                agents: tui.agent_rows.iter().filter(|row| row.host == name).count(),
                this_machine: tui.local_peer_id.as_deref() == Some(member.peer_id.as_slice()),
                reachable: true,
                name,
            }
        })
        .collect::<Vec<_>>();
    for paired in &tui.paired_machines {
        if rows.iter().any(|row| row.name == paired.name) {
            continue;
        }
        rows.push(MachineRow {
            name: paired.name.clone(),
            reachable: false,
            accepts_work: paired.accepts_work,
            agents: 0,
            this_machine: false,
        });
    }
    rows
}

/// One line of the machine strip, and one row of `p2pmux machines`.
///
/// Public because the CLI builds these too: the `m` key on Home and the command
/// print the same rows through the same formatter, so the two can never drift
/// into describing the same fleet differently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineRow {
    pub name: String,
    pub reachable: bool,
    /// `None` when that machine has never said. See [`crate::tui::PairedMachine`].
    pub accepts_work: Option<bool>,
    pub agents: usize,
    pub this_machine: bool,
}

/// How many lines the table under the agents wants: a blank spacer, a heading,
/// and one line per machine.
fn machine_table_lines(machines: usize) -> u16 {
    u16::try_from(machines)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
}

pub(in crate::tui) fn home_layout(area: Rect, tui: &MultiPaneTui) -> HomeLayout {
    // Header, then rows, then the machines — down the right on a terminal with
    // width to spare, and pinned to the bottom otherwise. The key bar is not
    // here: it takes over the window footer, so that four keys stay visible in
    // the same place they are on every other screen.
    let machines = machine_rows(tui).len();
    if machines == 0 {
        let (header, rows, hint) = stacked(area, tui, 0);
        return HomeLayout {
            header,
            rows,
            hint,
            machines: Rect::new(area.x, hint.bottom(), area.width, 0),
            machine_panel: MachinePanel::Empty,
        };
    }
    if area.width >= MACHINE_RAIL_MIN_WIDTH && area.height >= MACHINE_RAIL_MIN_HEIGHT {
        // The rail is full height rather than sized to the fleet: it is a column
        // of the screen, and a short one would leave a ragged hole beside the
        // agents. A fleet too tall for it says so on its last line.
        let rail = Rect::new(
            area.right().saturating_sub(MACHINE_RAIL_WIDTH),
            area.y,
            MACHINE_RAIL_WIDTH,
            area.height,
        );
        let (header, rows, hint) = stacked(
            Rect::new(
                area.x,
                area.y,
                area.width.saturating_sub(MACHINE_RAIL_WIDTH),
                area.height,
            ),
            tui,
            0,
        );
        return HomeLayout {
            header,
            rows,
            hint,
            machines: rail,
            machine_panel: MachinePanel::Rail,
        };
    }
    // Under the agents, then. The machines outrank agent rows when space runs
    // out, but never take the last one: a list with nothing left in it stops
    // being a list, and Home would be a screen about machines with the agents
    // it exists for cut off. All or nothing — half a block is a blank line
    // where a fleet should be.
    let left = area.height.saturating_sub(2u16.min(area.height));
    let table = machine_table_lines(machines);
    let (panel, height) = if table <= left.saturating_sub(1) {
        (MachinePanel::Table, table)
    } else if left.saturating_sub(1) >= 2 {
        (MachinePanel::Strip, 2)
    } else {
        (MachinePanel::Empty, 0)
    };
    let (header, rows, hint) = stacked(area, tui, height);
    HomeLayout {
        header,
        rows,
        hint,
        machines: Rect::new(area.x, hint.bottom(), area.width, height),
        machine_panel: panel,
    }
}

/// The header, the agent rows and the hint, stacked into whatever width and
/// height are left once the machines have taken their share.
fn stacked(area: Rect, tui: &MultiPaneTui, machines_height: u16) -> (Rect, Rect, Rect) {
    let header_height = 2u16.min(area.height);
    let left = area
        .height
        .saturating_sub(header_height)
        .saturating_sub(machines_height);
    let hint_height = u16::from(tui.home_all_unwired()).min(left);
    let rows_height = left.saturating_sub(hint_height);
    let header = Rect::new(area.x, area.y, area.width, header_height);
    let rows = Rect::new(area.x, header.bottom(), area.width, rows_height);
    let hint = Rect::new(area.x, rows.bottom(), area.width, hint_height);
    (header, rows, hint)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use super::{MACHINE_RAIL_WIDTH, MachinePanel, home_layout, machine_rows};
    use crate::{
        protocol::AgentRosterState,
        tui::{KeyHandling, MultiPaneTui, UiIntent, test_support::home_tui},
    };

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 24,
    };

    fn ordered(tui: &MultiPaneTui) -> Vec<(String, AgentRosterState)> {
        tui.home_rows()
            .into_iter()
            .map(|row| (row.host.clone(), row.state))
            .collect()
    }

    #[test]
    fn a_row_that_needs_you_never_sorts_below_one_that_does_not() {
        let tui = home_tui(&[
            ("laptop", "claude", AgentRosterState::Working),
            ("droplet", "claude", AgentRosterState::Idle),
            ("desktop", "claude", AgentRosterState::Pending),
            ("laptop", "codex", AgentRosterState::Done),
            ("desktop", "cursor", AgentRosterState::Error),
        ]);

        assert_eq!(
            ordered(&tui)
                .into_iter()
                .map(|(_, state)| state)
                .collect::<Vec<_>>(),
            vec![
                AgentRosterState::Error,
                AgentRosterState::Pending,
                AgentRosterState::Done,
                AgentRosterState::Working,
                AgentRosterState::Idle,
            ],
            "blocked → done → running → idle, with a failure above all of them"
        );
    }

    #[test]
    fn rows_within_a_state_keep_a_stable_order_across_repaints() {
        // The screen repaints several times a second. A row that slides under
        // the cursor because a timer ticked is worse than any ordering the
        // elapsed column could buy, so the secondary key is machine and agent.
        let mut tui = home_tui(&[
            ("laptop", "codex", AgentRosterState::Working),
            ("desktop", "claude", AgentRosterState::Working),
            ("laptop", "claude", AgentRosterState::Working),
        ]);
        let first = ordered(&tui);
        for row in &mut tui.agent_rows {
            row.working_since_unix_ms += 60_000;
        }

        assert_eq!(first, ordered(&tui));
        assert_eq!(
            first.into_iter().map(|(host, _)| host).collect::<Vec<_>>(),
            vec!["desktop", "laptop", "laptop"]
        );
    }

    #[test]
    fn the_header_count_only_counts_rows_a_hook_reported_as_blocked() {
        let tui = home_tui(&[
            ("laptop", "claude", AgentRosterState::Pending),
            ("droplet", "codex", AgentRosterState::Pending),
            // Detection knows this process is alive and nothing more. It must
            // never reach the count that a notification will one day carry.
            ("desktop", "cursor", AgentRosterState::Unknown),
            ("laptop", "codex", AgentRosterState::Working),
        ]);

        assert_eq!(tui.home_needs_you_count(), 2);
        assert!(!tui.home_all_unwired());
    }

    #[test]
    fn every_row_unreported_is_the_onboarding_empty_state() {
        let tui = home_tui(&[
            ("laptop", "claude", AgentRosterState::Unknown),
            ("laptop", "codex", AgentRosterState::Unknown),
        ]);

        assert!(tui.home_all_unwired());
        assert_eq!(tui.home_needs_you_count(), 0);
        assert_eq!(home_layout(AREA, &tui).hint.height, 1);
    }

    #[test]
    fn no_agents_at_all_asks_for_no_hint_line() {
        let tui = home_tui(&[]);

        assert!(!tui.home_all_unwired());
        assert_eq!(home_layout(AREA, &tui).hint.height, 0);
    }

    #[test]
    fn the_machine_list_holds_this_machine_with_nothing_paired() {
        // A fleet of one is still a fleet. Hiding the list made the screen
        // say "no machines" about the machine it was being read on.
        let tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);

        let machines = machine_rows(&tui);
        assert_eq!(machines.len(), 1);
        assert!(machines[0].this_machine);

        let layout = home_layout(AREA, &tui);
        assert_eq!(layout.machine_panel, MachinePanel::Rail);
        assert_eq!(layout.machines.height, AREA.height);
    }

    /// The rail is a column of the screen, so what it costs is width, and the
    /// agents keep every line they had.
    #[test]
    fn the_rail_takes_width_from_the_agents_and_never_a_row() {
        let tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);

        let layout = home_layout(AREA, &tui);
        assert_eq!(layout.rows.width, AREA.width - MACHINE_RAIL_WIDTH);
        assert_eq!(layout.machines.x, AREA.width - MACHINE_RAIL_WIDTH);
        assert_eq!(
            layout.rows.height,
            AREA.height - 2,
            "the header is the only thing above the agents"
        );
    }

    /// Narrow terminals get the same facts under the agents instead, and the
    /// machines outrank agent rows there — but never take the last one.
    #[test]
    fn a_terminal_too_narrow_for_a_rail_puts_the_machines_underneath() {
        let tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);

        // A spacer, a heading and one machine, with rows to spare.
        let roomy = home_layout(Rect::new(0, 0, 40, 10), &tui);
        assert_eq!(roomy.machine_panel, MachinePanel::Table);
        assert_eq!(roomy.machines.height, 3);
        assert_eq!(roomy.rows.height, 5);

        // No room for the table: the strip still answers "is my fleet up".
        let tight = home_layout(Rect::new(0, 0, 40, 5), &tui);
        assert_eq!(tight.machine_panel, MachinePanel::Strip);
        assert_eq!(tight.machines.height, 2);
        assert_eq!(tight.rows.height, 1);

        // One line fewer and the strip would take every row there is, so it
        // stands down whole — half a strip is a blank line where a fleet goes.
        let cramped = home_layout(Rect::new(0, 0, 40, 4), &tui);
        assert_eq!(cramped.machine_panel, MachinePanel::Empty);
        assert_eq!(cramped.machines.height, 0);
        assert_eq!(cramped.rows.height, 2);
    }

    /// A terminal too short for a rail falls back rather than drawing a
    /// two-line column beside a two-line list.
    #[test]
    fn a_wide_but_short_terminal_falls_back_from_the_rail() {
        let tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);

        assert_eq!(
            home_layout(Rect::new(0, 0, 120, 5), &tui).machine_panel,
            MachinePanel::Strip
        );
        assert_eq!(
            home_layout(Rect::new(0, 0, 120, 6), &tui).machine_panel,
            MachinePanel::Rail
        );
    }

    #[test]
    fn a_paired_machine_that_is_not_a_member_is_reported_asleep() {
        let mut tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.paired_machines = vec![crate::tui::PairedMachine {
            name: String::from("oldbox"),
            accepts_work: Some(true),
        }];

        let machines = machine_rows(&tui);
        assert_eq!(machines.len(), 2);
        let oldbox = machines
            .iter()
            .find(|machine| machine.name == "oldbox")
            .expect("the paired machine is listed");
        assert!(!oldbox.reachable);
        assert_eq!(oldbox.accepts_work, Some(true));
    }

    #[test]
    fn arrows_move_the_selection_and_enter_lands_in_that_agents_terminal() {
        let mut tui = home_tui(&[
            ("laptop", "claude", AgentRosterState::Working),
            ("desktop", "claude", AgentRosterState::Pending),
        ]);
        tui.set_home_open(true, "test");
        // Sorted, the blocked desktop row is first, so the cursor starts there.
        assert_eq!(tui.home_selected, Some(2));

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), AREA),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.home_selected, Some(1));

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), AREA),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 1 }])
        );
        assert!(!tui.home_open());
        assert_eq!(tui.focused_pane(), 1);
        assert_eq!(
            tui.zoomed_pane(),
            Some(1),
            "Enter lands in the terminal full screen, not in the pane grid"
        );
        assert_eq!(
            tui.geometry(AREA).panes.len(),
            1,
            "nothing else on that tab is drawn while the agent is zoomed"
        );
    }

    #[test]
    fn ctrl_o_returns_home_from_inside_a_pane_and_esc_never_does() {
        let mut tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

        assert_eq!(tui.handle_key(ctrl_o, AREA), KeyHandling::Consumed(vec![]));
        assert!(tui.home_open());
        assert_eq!(tui.handle_key(ctrl_o, AREA), KeyHandling::Consumed(vec![]));
        assert!(!tui.home_open());

        // Inside a pane, every unmodified key belongs to the program: Claude
        // Code interrupts on Esc and vim needs it constantly.
        for key in [KeyCode::Esc, KeyCode::Char('c'), KeyCode::Up, KeyCode::Left] {
            assert_eq!(
                tui.handle_key(KeyEvent::new(key, KeyModifiers::NONE), AREA),
                KeyHandling::Forward,
                "{key:?} must reach the program in the pane"
            );
        }
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                AREA
            ),
            KeyHandling::Forward
        );
    }

    #[test]
    fn n_opens_a_terminal_and_leaves_home_for_it() {
        let mut tui = home_tui(&[]);
        tui.set_home_open(true, "test");

        let KeyHandling::Consumed(intents) =
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), AREA)
        else {
            panic!("n is claimed by Home");
        };
        assert!(matches!(intents.as_slice(), [UiIntent::CreateTab { .. }]));
        assert!(
            !tui.home_open(),
            "a first run with nothing on it has to have somewhere to go"
        );
    }

    /// `m` used to expand the strip into the table. The rail is the table, so
    /// there is nothing left for the key to do and it no longer claims one.
    #[test]
    fn every_machine_is_listed_without_a_key_to_expand_them() {
        let mut tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.paired_machines = vec![crate::tui::PairedMachine {
            name: String::from("droplet"),
            accepts_work: None,
        }];
        tui.set_home_open(true, "test");

        assert_eq!(machine_rows(&tui).len(), 2);
        let before = home_layout(AREA, &tui);
        tui.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), AREA);
        assert_eq!(home_layout(AREA, &tui), before);
    }

    #[test]
    fn q_quits_from_home_but_only_from_home() {
        let mut tui = home_tui(&[]);
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(tui.handle_key(q, AREA), KeyHandling::Forward);

        tui.set_home_open(true, "test");
        assert_eq!(
            tui.handle_key(q, AREA),
            KeyHandling::Quit(crate::tui::QuitAction::Detach)
        );
    }

    /// Home is the one screen where a bare `q` is free. It should not also be
    /// the one screen where leaving skips the question Ctrl+Q asks.
    #[test]
    fn q_on_a_detachable_home_asks_the_same_question_ctrl_q_does() {
        let mut tui = home_tui(&[]);
        tui.set_detachable(true);
        tui.set_home_open(true, "test");

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), AREA),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.quit_open());
    }

    #[test]
    fn opening_home_puts_the_cursor_on_the_row_that_most_wants_a_human() {
        let mut tui = home_tui(&[
            ("laptop", "claude", AgentRosterState::Working),
            ("desktop", "codex", AgentRosterState::Working),
        ]);
        tui.set_home_open(true, "test");
        tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), AREA);
        let moved = tui.home_selected.expect("a row is selected");
        tui.set_home_open(false, "test");

        // A blocked agent appears while the user is elsewhere -- and it is not
        // the row the cursor was parked on.
        let mut rows = tui.agent_rows.clone();
        let other = rows
            .iter_mut()
            .find(|row| row.pane_id != moved)
            .expect("a row the cursor is not on");
        other.state = AgentRosterState::Pending;
        tui.set_agent_rows(rows);
        tui.set_home_open(true, "test");

        assert_ne!(tui.home_selected, Some(moved));
        assert_eq!(
            tui.home_selected,
            tui.home_rows().first().map(|row| row.pane_id),
            "Enter on arrival has to open the row the sort order put on top"
        );
    }

    #[test]
    fn the_selection_survives_a_row_changing_state_and_moves_when_it_vanishes() {
        let mut tui = home_tui(&[
            ("laptop", "claude", AgentRosterState::Working),
            ("desktop", "codex", AgentRosterState::Working),
        ]);
        tui.set_home_open(true, "test");
        tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), AREA);
        let selected = tui.home_selected.expect("a row is selected");

        let mut rows = tui.agent_rows.clone();
        rows[0].state = AgentRosterState::Pending;
        tui.set_agent_rows(rows);
        tui.repair_home_selection();
        assert_eq!(
            tui.home_selected,
            Some(selected),
            "re-sorting the list must not move the cursor to another agent"
        );

        tui.set_agent_rows(Vec::new());
        tui.repair_home_selection();
        assert_eq!(tui.home_selected, None);
    }
}
