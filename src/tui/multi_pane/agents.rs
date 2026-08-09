//! The agent roster this client holds, and the unread markers it drives.
//!
//! Where the rows are *drawn* is [`crate::tui::home`]. What lives here is the
//! bookkeeping every screen shares: which panes have an agent, what state each
//! was last seen in, and which of them arrived at a state wanting a human while
//! the user was looking somewhere else.

use std::time::Instant;

use crate::{
    layout::PaneId,
    protocol::AgentRosterState,
    tui::{AgentOverlayRow, MultiPaneTui, geometry::contains_leaf},
};

impl MultiPaneTui {
    pub fn set_agent_rows(&mut self, mut rows: Vec<AgentOverlayRow>) -> bool {
        rows.retain_mut(|row| {
            // An agent running outside p2pmux has no pane to be located in, and
            // dropping it here is exactly how a bot under systemd stayed
            // invisible. Its labels came from whoever built the row.
            if row.outside_p2pmux() {
                return true;
            }
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
        rows.sort_by_key(|row| {
            (
                row.tab_ordinal,
                row.pane_ordinal,
                row.pane_id,
                row.process_pid,
            )
        });
        if self.agent_rows == rows {
            return false;
        }
        self.agent_rows = rows;
        // Home reads these rows in its own order, so its cursor is repaired
        // from the same place they are set. Doing it here rather than at each
        // call site is what keeps the two from drifting apart.
        self.repair_home_selection();
        true
    }

    /// Updates attached-client agent rows and reports panes that just started wanting a human.
    ///
    /// The first observed roster only establishes the local baseline. Roster rows that disappear
    /// intentionally retain their previous state and unread marker until their pane is deleted.
    ///
    /// The trigger is arriving at a state that wants attention — `done`, `needs
    /// you`, or `error` — from one that did not. It used to be `working → idle`,
    /// which was the shape of the old inference: `idle` was what a pane decayed
    /// into once it had been quiet long enough, so the marker fired on a guess
    /// about silence and could never fire for an agent blocked on a question.
    /// Now the agent says when it is done, and says when it is stuck.
    ///
    /// Keyed on the work episode rather than on the marker itself: focusing a
    /// pane clears the marker, and that must not re-arm it for work the user
    /// has already seen.
    pub fn update_attached_agent_rows(&mut self, rows: Vec<AgentOverlayRow>) -> Vec<PaneId> {
        self.set_agent_rows(rows);
        let mut newly_unread = Vec::new();
        for row in &self.agent_rows {
            if row.state == AgentRosterState::Working {
                self.prior_agent_episodes
                    .insert(row.pane_id, row.working_since_unix_ms);
            }
            let previous = self.prior_agent_states.insert(row.pane_id, row.state);
            if previous.is_none_or(AgentRosterState::needs_attention)
                || !row.state.needs_attention()
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

    /// Whether any row is spinning, so the draw loop knows to keep repainting.
    ///
    /// Only while Home is on screen: a spinner nobody can see is a wake-up per
    /// frame for nothing.
    pub(crate) fn home_has_working_rows(&self) -> bool {
        self.home_open
            && self
                .agent_rows
                .iter()
                .any(|row| row.state == AgentRosterState::Working)
    }

    pub(crate) fn expire_home_toggle(&mut self, now: Instant) -> bool {
        if self
            .pending_home_toggle
            .is_some_and(|then| now.duration_since(then) >= crate::tui::HOME_TOGGLE_WINDOW)
        {
            self.pending_home_toggle = None;
        }
        false
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        layout::{Axis, Node, Tab},
        tui::{
            MultiPaneTui,
            test_support::{agent_row, layout},
        },
    };

    #[test]
    fn attached_agent_rows_mark_only_unfocused_arrivals_at_attention_unread() {
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
        idle.state = crate::protocol::AgentRosterState::Done;

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
        idle.state = crate::protocol::AgentRosterState::Done;
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
        idle.state = crate::protocol::AgentRosterState::Done;

        tui.update_attached_agent_rows(vec![working]);
        assert_eq!(tui.update_attached_agent_rows(vec![idle]), vec![2]);
        tui.update_attached_agent_rows(Vec::new());
        assert_eq!(
            tui.prior_agent_states[&2],
            crate::protocol::AgentRosterState::Done
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
    }

    #[test]
    fn agent_rows_are_stamped_with_the_chrome_locations_of_their_panes() {
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
    }

    #[test]
    fn a_row_for_a_pane_this_client_cannot_place_is_dropped() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();

        tui.set_agent_rows(vec![agent_row(1, 1, 1), agent_row(99, 1, 1)]);

        assert_eq!(tui.agent_rows.len(), 1);
        assert_eq!(tui.agent_rows[0].pane_id, 1);
    }
}
