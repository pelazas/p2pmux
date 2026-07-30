//! The agents overlay's state: the roster rows, which one is selected, and
//! how the list scrolls.

use std::time::Instant;

use ratatui::layout::Rect;

use crate::{
    layout::PaneId,
    protocol::AgentRosterState,
    tui::{
        AGENT_TOGGLE_WINDOW, AgentOverlayRow, ModalState, MultiPaneTui, UiIntent,
        debug_log::ui_debug_log,
        geometry::{contains_leaf, rect_contains},
        render::agents::{AGENT_OVERLAY_CARD_LINES, agents_overlay_content},
    },
};

impl MultiPaneTui {
    pub fn set_agent_rows(&mut self, mut rows: Vec<AgentOverlayRow>) -> bool {
        rows.retain_mut(|row| {
            let Some((tab_ordinal, pane_ordinal)) = self.pane_location(row.pane_id) else {
                return false;
            };
            row.tab_ordinal = tab_ordinal;
            row.pane_ordinal = pane_ordinal;
            row.tab_label = self
                .snapshot
                .tabs
                .iter()
                .find(|tab| contains_leaf(&tab.root, row.pane_id))
                .and_then(|tab| tab.title.clone())
                .unwrap_or_else(|| format!("Tab #{tab_ordinal}"));
            row.pane_label = self
                .snapshot
                .panes
                .get(&row.pane_id)
                .and_then(|pane| pane.title.clone())
                .unwrap_or_else(|| format!("Pane #{pane_ordinal}"));
            true
        });
        rows.sort_by_key(|row| (row.tab_ordinal, row.pane_ordinal, row.pane_id));
        if self.agent_rows == rows {
            return false;
        }
        self.agent_rows = rows;
        if self
            .agent_selected_pane
            .is_some_and(|pane_id| !self.agent_rows.iter().any(|row| row.pane_id == pane_id))
        {
            self.agent_selected_pane = self.agent_rows.first().map(|row| row.pane_id);
        }
        if self.agent_selected_pane.is_none() {
            self.agent_selected_pane = self.agent_rows.first().map(|row| row.pane_id);
        }
        self.clamp_agent_overlay_scroll();
        self.ensure_agent_selection_visible();
        true
    }

    /// Updates attached-client agent rows and reports newly unread completions.
    ///
    /// The first observed roster only establishes the local baseline. Roster rows that disappear
    /// intentionally retain their previous state and unread marker until their pane is deleted.
    /// Apply the latest roster and return the panes whose agent just finished a work episode
    /// the user has not been told about. Announcing is keyed on the work episode rather than on
    /// the unread marker: focusing a pane clears the marker, and that must not re-arm the
    /// completion sound for work the user has already been notified about.
    pub fn update_attached_agent_rows(&mut self, rows: Vec<AgentOverlayRow>) -> Vec<PaneId> {
        self.set_agent_rows(rows);
        let mut newly_unread = Vec::new();
        for row in &self.agent_rows {
            if row.state == AgentRosterState::Working {
                self.prior_agent_episodes
                    .insert(row.pane_id, row.working_since_unix_ms);
            }
            let previous = self.prior_agent_states.insert(row.pane_id, row.state);
            if previous != Some(AgentRosterState::Working)
                || row.state != AgentRosterState::Idle
                || row.pane_id == self.focused_pane
            {
                continue;
            }
            let episode = self
                .prior_agent_episodes
                .get(&row.pane_id)
                .copied()
                .unwrap_or_default();
            if self.notified_agent_episodes.get(&row.pane_id) == Some(&episode) {
                continue;
            }
            self.notified_agent_episodes.insert(row.pane_id, episode);
            self.unread_agent_panes.insert(row.pane_id);
            newly_unread.push(row.pane_id);
        }
        newly_unread
    }

    pub(crate) fn set_agent_overlay_viewport(&mut self, area: Rect) {
        self.agent_overlay_viewport_lines = agents_overlay_content(area).height;
        self.clamp_agent_overlay_scroll();
    }

    pub(in crate::tui) fn agent_overlay_total_lines(&self) -> usize {
        self.agent_rows
            .len()
            .saturating_mul(AGENT_OVERLAY_CARD_LINES)
            .saturating_sub(1)
    }

    pub(in crate::tui) fn agent_overlay_max_scroll(&self) -> usize {
        self.agent_overlay_total_lines()
            .saturating_sub(usize::from(self.agent_overlay_viewport_lines.max(1)))
    }

    pub(in crate::tui) fn clamp_agent_overlay_scroll(&mut self) {
        if self.agent_overlay_viewport_lines == 0 {
            self.agent_overlay_scroll_line = 0;
            return;
        }
        self.agent_overlay_scroll_line = self
            .agent_overlay_scroll_line
            .min(self.agent_overlay_max_scroll());
    }

    pub(in crate::tui) fn ensure_agent_selection_visible(&mut self) {
        if self.agent_overlay_viewport_lines == 0 {
            return;
        }
        let Some(index) = self
            .agent_selected_pane
            .and_then(|pane| self.agent_rows.iter().position(|row| row.pane_id == pane))
        else {
            return;
        };
        let viewport_lines = usize::from(self.agent_overlay_viewport_lines.max(1));
        let card_start = index.saturating_mul(AGENT_OVERLAY_CARD_LINES);
        let card_end = card_start.saturating_add(1);
        if card_start < self.agent_overlay_scroll_line {
            self.agent_overlay_scroll_line = card_start;
        } else if card_end
            >= self
                .agent_overlay_scroll_line
                .saturating_add(viewport_lines)
        {
            self.agent_overlay_scroll_line =
                card_end.saturating_add(1).saturating_sub(viewport_lines);
        }
        self.clamp_agent_overlay_scroll();
    }

    pub(crate) fn scroll_agent_overlay(&mut self, area: Rect, up: bool) -> bool {
        self.set_agent_overlay_viewport(area);
        let previous = self.agent_overlay_scroll_line;
        if up {
            self.agent_overlay_scroll_line = self
                .agent_overlay_scroll_line
                .saturating_sub(AGENT_OVERLAY_CARD_LINES);
        } else {
            self.agent_overlay_scroll_line = self
                .agent_overlay_scroll_line
                .saturating_add(AGENT_OVERLAY_CARD_LINES);
        }
        self.clamp_agent_overlay_scroll();
        self.agent_overlay_scroll_line != previous
    }

    pub(crate) fn agent_overlay_has_working_rows(&self) -> bool {
        self.overlay_open()
            && self
                .agent_rows
                .iter()
                .any(|row| row.state == AgentRosterState::Working)
    }

    pub(crate) fn expire_agent_toggle(&mut self, now: Instant) -> bool {
        if self
            .pending_agent_toggle
            .is_some_and(|then| now.duration_since(then) >= AGENT_TOGGLE_WINDOW)
        {
            self.pending_agent_toggle = None;
        }
        false
    }

    pub(in crate::tui) fn handle_agent_overlay_click(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Vec<UiIntent> {
        let Some(pane_id) = self.agent_overlay_row_at(column, row, area) else {
            return Vec::new();
        };
        self.agent_selected_pane = Some(pane_id);
        self.jump_to_agent_pane(pane_id, "mouse")
    }

    pub(in crate::tui) fn agent_overlay_row_at(
        &self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Option<PaneId> {
        let content = agents_overlay_content(area);
        if !rect_contains(content, column, row) {
            return None;
        }
        let line = usize::from(row.saturating_sub(content.y))
            .saturating_add(self.agent_overlay_scroll_line);
        if line % AGENT_OVERLAY_CARD_LINES == 2 {
            return None;
        }
        self.agent_rows
            .get(line / AGENT_OVERLAY_CARD_LINES)
            .map(|agent| agent.pane_id)
    }

    pub(in crate::tui) fn jump_to_agent_pane(
        &mut self,
        pane_id: PaneId,
        close_reason: &str,
    ) -> Vec<UiIntent> {
        let Some(tab) = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| contains_leaf(&tab.root, pane_id))
        else {
            return Vec::new();
        };
        self.select_pane(tab.tab_id, pane_id, "overlay_jump");
        self.modal = ModalState::None;
        self.pending_agent_toggle = None;
        ui_debug_log(
            "agents_overlay_close",
            format_args!("reason={close_reason} pane_id={pane_id}"),
        );
        vec![UiIntent::FocusPane { pane_id }]
    }

    pub(in crate::tui) fn move_agent_selection(&mut self, forward: bool) {
        if self.agent_rows.is_empty() {
            self.agent_selected_pane = None;
            return;
        }
        let current = self
            .agent_selected_pane
            .and_then(|pane| self.agent_rows.iter().position(|row| row.pane_id == pane))
            .unwrap_or(0);
        let len = self.agent_rows.len();
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.agent_selected_pane = Some(self.agent_rows[next].pane_id);
        self.ensure_agent_selection_visible();
    }
}

#[cfg(test)]
mod tests {

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use crate::{
        config::UiTheme,
        layout::{Axis, Node, Tab},
        tui::{
            KeyHandling, MultiPaneTui, UiIntent,
            render::agents::{
                agents_overlay_content, agents_overlay_help, agents_overlay_inner,
                format_agent_overlay_card,
            },
            test_support::{agent_overlay_tui, agent_row, layout},
        },
    };

    #[test]
    fn attached_agent_rows_mark_only_unfocused_working_to_idle_transitions_unread() {
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
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        let working = agent_row(2, 1, 2);
        let mut idle = working.clone();
        idle.state = crate::protocol::AgentRosterState::Idle;

        assert_eq!(
            tui.update_attached_agent_rows(vec![working.clone()]),
            Vec::<u64>::new()
        );
        assert_eq!(tui.update_attached_agent_rows(vec![idle.clone()]), vec![2]);
        assert!(tui.unread_agent_panes.contains(&2));
        assert_eq!(
            tui.update_attached_agent_rows(vec![idle]),
            Vec::<u64>::new()
        );

        let mut focused_working = working;
        focused_working.pane_id = 1;
        focused_working.pane_ordinal = 1;
        let mut focused_idle = focused_working.clone();
        focused_idle.state = crate::protocol::AgentRosterState::Idle;
        assert_eq!(
            tui.update_attached_agent_rows(vec![focused_working]),
            Vec::<u64>::new()
        );
        assert_eq!(
            tui.update_attached_agent_rows(vec![focused_idle]),
            Vec::<u64>::new()
        );
        assert!(!tui.unread_agent_panes.contains(&1));
    }

    #[test]
    fn focusing_a_pane_does_not_re_announce_the_same_work_episode() {
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
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        let working = agent_row(2, 1, 2);
        let mut idle = working.clone();
        idle.state = crate::protocol::AgentRosterState::Idle;
        // A real host sends the `0` sentinel on a non-working row, so the episode a completion
        // refers to is only ever visible on the working row that preceded it.
        idle.working_since_unix_ms = 0;

        tui.update_attached_agent_rows(vec![working.clone()]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle.clone()]), vec![2]);

        // Looking at the pane clears the unread marker. That must not re-arm the sound: the
        // user has already been told about this episode.
        tui.set_focus(1, 2).expect("known pane");
        tui.set_focus(1, 1).expect("known pane");
        assert!(!tui.unread_agent_panes.contains(&2));

        tui.update_attached_agent_rows(vec![working.clone()]);
        assert_eq!(
            tui.update_attached_agent_rows(vec![idle.clone()]),
            Vec::<u64>::new(),
            "the same work episode must only be announced once"
        );

        // A genuinely new working interval is a new episode and does announce again.
        let mut next_working = working;
        next_working.working_since_unix_ms += 1;
        tui.update_attached_agent_rows(vec![next_working]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle]), vec![2]);
    }

    #[test]
    fn attached_agent_rows_preserve_state_and_unread_when_rows_vanish() {
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
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");
        let working = agent_row(2, 1, 2);
        let mut idle = working.clone();
        idle.state = crate::protocol::AgentRosterState::Idle;

        tui.update_attached_agent_rows(vec![working]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle]), vec![2]);
        tui.update_attached_agent_rows(Vec::new());
        assert_eq!(
            tui.prior_agent_states[&2],
            crate::protocol::AgentRosterState::Idle
        );
        assert!(tui.unread_agent_panes.contains(&2));
    }

    #[test]
    fn focus_routes_clear_unread_agent_markers() {
        let snapshot = layout(
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
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("valid layout");

        tui.unread_agent_panes.insert(2);
        tui.select_tab(2).expect("known tab");
        assert!(!tui.unread_agent_panes.contains(&2));

        tui.unread_agent_panes.insert(1);
        tui.set_focus(1, 1).expect("known pane");
        assert!(!tui.unread_agent_panes.contains(&1));

        tui.unread_agent_panes.insert(2);
        assert_eq!(
            tui.jump_to_agent_pane(2, "test"),
            vec![UiIntent::FocusPane { pane_id: 2 }]
        );
        assert!(!tui.unread_agent_panes.contains(&2));
    }

    #[test]
    fn agents_overlay_rows_use_chrome_locations_and_sort_by_them() {
        let snapshot = layout(
            vec![
                Tab {
                    tab_id: 10,
                    root: Node::Split {
                        axis: Axis::LeftRight,
                        first_share_bps: 5_000,
                        first: Box::new(Node::Leaf { pane_id: 8 }),
                        second: Box::new(Node::Leaf { pane_id: 6 }),
                    },

                    title: None,
                },
                Tab {
                    tab_id: 20,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
            ],
            &[(8, 2, 8), (6, 2, 8), (3, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();

        tui.set_agent_rows(vec![
            agent_row(3, 99, 99),
            agent_row(6, 99, 99),
            agent_row(8, 99, 99),
        ]);

        assert_eq!(
            tui.agent_rows
                .iter()
                .map(|row| (row.pane_id, row.tab_ordinal, row.pane_ordinal))
                .collect::<Vec<_>>(),
            vec![(8, 1, 1), (6, 1, 2), (3, 2, 1)]
        );
        assert!(
            format_agent_overlay_card(&tui.agent_rows[2], false, 120, 0, 0, &UiTheme::default(),)
                [1]
            .to_string()
            .contains("Tab #2 · Pane #1")
        );
    }

    #[test]
    fn agents_overlay_click_jumps_to_the_clicked_row_and_ignores_outside_clicks() {
        let snapshot = layout(
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
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();
        tui.set_agent_rows(vec![agent_row(1, 1, 1), agent_row(2, 2, 1)]);
        let area = Rect::new(0, 0, 80, 24);
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        tui.handle_key(ctrl_a, area);

        assert_eq!(tui.handle_agent_overlay_click(0, 0, area), Vec::new());
        assert!(tui.overlay_open());
        let inner = agents_overlay_inner(area);
        let content = agents_overlay_content(area);
        let help = agents_overlay_help(area).expect("help row");
        assert_eq!(
            tui.agent_overlay_row_at(content.x.saturating_sub(1), content.y, area),
            None
        );
        assert_eq!(tui.agent_overlay_row_at(content.x, inner.y, area), None);
        assert_eq!(tui.agent_overlay_row_at(help.x, help.y, area), None);
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y, area),
            Some(1)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(1), area),
            Some(1)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(3), area),
            Some(2)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(4), area),
            Some(2)
        );
        assert_eq!(
            tui.handle_agent_overlay_click(content.x, content.y.saturating_add(3), area),
            vec![UiIntent::FocusPane { pane_id: 2 }]
        );
        assert!(!tui.overlay_open());
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
        assert_eq!(tui.pending_agent_toggle, None);
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
    }

    #[test]
    fn agents_overlay_hit_test_accounts_for_terminal_line_scroll() {
        let area = Rect::new(0, 0, 80, 8);
        let mut tui = agent_overlay_tui(3);
        let content = agents_overlay_content(area);

        assert!(tui.scroll_agent_overlay(area, false));
        assert_eq!(tui.agent_overlay_scroll_line, 3);
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y, area),
            Some(2)
        );
        assert_eq!(
            tui.agent_overlay_row_at(content.x, content.y.saturating_add(2), area),
            None
        );
    }

    #[test]
    fn agents_overlay_selection_auto_scrolls_to_keep_cards_visible() {
        let area = Rect::new(0, 0, 80, 8);
        let mut tui = agent_overlay_tui(4);

        tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area);
        assert_eq!(tui.agent_selected_pane, Some(2));
        assert_eq!(tui.agent_overlay_scroll_line, 3);
        tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area);
        assert_eq!(tui.agent_selected_pane, Some(3));
        assert_eq!(tui.agent_overlay_scroll_line, 6);
    }

    #[test]
    fn agents_overlay_scroll_clamps_when_roster_shrinks() {
        let area = Rect::new(0, 0, 80, 8);
        let mut tui = agent_overlay_tui(4);

        assert!(tui.scroll_agent_overlay(area, false));
        assert!(tui.scroll_agent_overlay(area, false));
        assert_eq!(tui.agent_overlay_scroll_line, 6);
        tui.set_agent_rows(vec![agent_row(1, 1, 1)]);
        assert_eq!(tui.agent_overlay_scroll_line, 0);
    }
}
