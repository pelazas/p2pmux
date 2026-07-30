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
