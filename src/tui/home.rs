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
            self.machines_expanded = false;
            // Arriving puts the cursor back on the top row, which is the one
            // that most wants a human. That is the whole reason for the sort
            // order, and a cursor left where a previous visit happened to end
            // would mean Enter on arrival opens whatever was urgent last time.
            //
            // Only on arrival. While Home is open the cursor stays exactly
            // where the user put it, and a row changing state under it must
            // never drag it somewhere else.
            self.home_selected = None;
            self.home_scroll_line = 0;
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
        self.clamp_home_scroll();
        self.ensure_home_selection_visible();
    }

    pub(in crate::tui) fn set_home_viewport(&mut self, lines: u16) {
        self.home_viewport_lines = lines;
        self.clamp_home_scroll();
    }

    /// Tell Home how many rows fit, from the whole terminal rather than from a
    /// line count the caller had to work out for itself.
    pub fn set_home_viewport_for(&mut self, area: Rect) {
        let rows = home_layout(self.geometry(area).content, self).rows.height;
        self.set_home_viewport(rows);
    }

    /// Wheel over the inbox. Returns whether anything moved, so a scroll at the
    /// end of the list never costs a repaint.
    pub fn scroll_home(&mut self, area: Rect, up: bool) -> bool {
        self.set_home_viewport_for(area);
        let previous = self.home_scroll_line;
        if up {
            self.home_scroll_line = self.home_scroll_line.saturating_sub(1);
        } else {
            self.home_scroll_line = self.home_scroll_line.saturating_add(1);
        }
        self.clamp_home_scroll();
        self.home_scroll_line != previous
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
        let rows = home_layout(self.geometry(area).content, self).rows;
        if !crate::tui::geometry::rect_contains(rows, column, row) {
            return None;
        }
        let line = usize::from(row.saturating_sub(rows.y)).saturating_add(self.home_scroll_line);
        self.home_rows().get(line).map(|row| row.pane_id)
    }

    pub(in crate::tui) fn home_max_scroll(&self) -> usize {
        self.agent_rows
            .len()
            .saturating_sub(usize::from(self.home_viewport_lines.max(1)))
    }

    pub(in crate::tui) fn clamp_home_scroll(&mut self) {
        if self.home_viewport_lines == 0 {
            self.home_scroll_line = 0;
            return;
        }
        self.home_scroll_line = self.home_scroll_line.min(self.home_max_scroll());
    }

    pub(in crate::tui) fn ensure_home_selection_visible(&mut self) {
        if self.home_viewport_lines == 0 {
            return;
        }
        let Some(index) = self.home_selected.and_then(|pane_id| {
            self.home_rows()
                .iter()
                .position(|row| row.pane_id == pane_id)
        }) else {
            return;
        };
        let viewport = usize::from(self.home_viewport_lines.max(1));
        if index < self.home_scroll_line {
            self.home_scroll_line = index;
        } else if index >= self.home_scroll_line.saturating_add(viewport) {
            self.home_scroll_line = index.saturating_add(1).saturating_sub(viewport);
        }
        self.clamp_home_scroll();
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

    pub(in crate::tui) fn toggle_machines_expanded(&mut self) {
        self.machines_expanded = !self.machines_expanded;
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
        self.set_home_viewport(home_layout(self.geometry(area).content, self).rows.height);
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
            KeyCode::Char('m') if key.modifiers.is_empty() => {
                self.toggle_machines_expanded();
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
    /// The one-line machine strip, or the expanded list under `m`.
    /// Zero-height only before the member list has arrived.
    pub(in crate::tui) machines: Rect,
}

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

/// How many lines the machine block wants.
///
/// One machine still gets a strip. Hiding it until a second machine appeared
/// made a fleet of one look like a fleet of none — the screen said "no
/// machines" about the machine it was being read on. A strip with one name in
/// it says the fleet is up and has room for more, which is the truth.
pub(in crate::tui) fn machine_lines(tui: &MultiPaneTui) -> u16 {
    let machines = machine_rows(tui).len();
    if machines == 0 {
        return 0;
    }
    if tui.machines_expanded {
        // A blank spacer, a heading, and one line each.
        u16::try_from(machines)
            .unwrap_or(u16::MAX)
            .saturating_add(2)
    } else {
        2
    }
}

pub(in crate::tui) fn home_layout(area: Rect, tui: &MultiPaneTui) -> HomeLayout {
    // Header, then rows, then the machine block pinned to the bottom. The key
    // bar is not here: it takes over the window footer, so that four keys stay
    // visible in the same place they are on every other screen. A terminal too
    // short to hold everything loses agent rows before the machine strip — the
    // strip is one line and answers "is my fleet even up", which a list of
    // agents cannot.
    let header_height = 2u16.min(area.height);
    let mut left = area.height.saturating_sub(header_height);
    // The strip outranks agent rows when space runs out, but it never takes the
    // last one: a list with nothing left in it stops being a list, and Home
    // would be a screen about machines with the agents it exists for cut off.
    // All or nothing — half a strip is a blank line where a fleet should be.
    let wanted = machine_lines(tui);
    let machines_height = if wanted <= left.saturating_sub(1) {
        wanted
    } else {
        0
    };
    left = left.saturating_sub(machines_height);
    let hint_height = u16::from(tui.home_all_unwired()).min(left);
    let rows_height = left.saturating_sub(hint_height);
    let header = Rect::new(area.x, area.y, area.width, header_height);
    let rows = Rect::new(area.x, header.bottom(), area.width, rows_height);
    let hint = Rect::new(area.x, rows.bottom(), area.width, hint_height);
    let machines = Rect::new(area.x, hint.bottom(), area.width, machines_height);
    HomeLayout {
        header,
        rows,
        hint,
        machines,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use super::{home_layout, machine_lines, machine_rows};
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
    fn the_machine_strip_lists_this_machine_with_nothing_paired() {
        // A fleet of one is still a fleet. Hiding the strip made the screen
        // say "no machines" about the machine it was being read on.
        let tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);

        let machines = machine_rows(&tui);
        assert_eq!(machines.len(), 1);
        assert!(machines[0].this_machine);
        assert_eq!(machine_lines(&tui), 2);
        assert_eq!(home_layout(AREA, &tui).machines.height, 2);
    }

    /// The strip outranks agent rows, but not the last one.
    #[test]
    fn a_home_too_short_for_both_keeps_a_row_rather_than_the_whole_strip() {
        let tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);

        // Two lines of header, three left: the strip fits with a row to spare.
        let roomy = home_layout(Rect::new(0, 0, 40, 5), &tui);
        assert_eq!(roomy.machines.height, 2);
        assert_eq!(roomy.rows.height, 1);

        // One line fewer and the strip would take every row there is, so it
        // stands down whole — half a strip is a blank line where a fleet goes.
        let cramped = home_layout(Rect::new(0, 0, 40, 4), &tui);
        assert_eq!(cramped.machines.height, 0);
        assert_eq!(cramped.rows.height, 2);
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
        assert_eq!(machine_lines(&tui), 2);
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

    #[test]
    fn m_expands_the_machines_list_in_place() {
        let mut tui = home_tui(&[("laptop", "claude", AgentRosterState::Working)]);
        tui.paired_machines = vec![crate::tui::PairedMachine {
            name: String::from("droplet"),
            accepts_work: None,
        }];
        tui.set_home_open(true, "test");
        assert_eq!(machine_lines(&tui), 2);

        tui.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), AREA);
        assert!(tui.machines_expanded);
        assert_eq!(machine_lines(&tui), 4, "a spacer, a heading, and two rows");
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
