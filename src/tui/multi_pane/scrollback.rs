//! Per-pane scrollback: where each pane is looking, and what moves it.

use ratatui::layout::Rect;

use crate::{layout::PaneId, tui::MultiPaneTui};

const PANE_SCROLL_WHEEL_STEP: usize = 3;

impl MultiPaneTui {
    pub(in crate::tui) fn scrollback_offset(&self, pane_id: PaneId) -> usize {
        self.pane_views
            .get(&pane_id)
            .map_or(0, |view| view.scrollback)
    }

    pub(crate) fn pane_scrollback_offset(&self, pane_id: PaneId) -> usize {
        self.scrollback_offset(pane_id)
    }

    pub(crate) fn set_pane_scrollback_offset(&mut self, pane_id: PaneId, offset: usize) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == offset {
            return false;
        }
        view.scrollback = offset;
        true
    }

    pub fn scroll_mouse_pane(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
        scrollback_len: usize,
        up: bool,
    ) -> bool {
        if self.modal_open() {
            return false;
        }
        let pane_id = self.pane_at_or_focused(column, row, area);
        self.scroll_pane(pane_id, scrollback_len, up)
    }

    pub(in crate::tui) fn scroll_pane(
        &mut self,
        pane_id: PaneId,
        scrollback_len: usize,
        up: bool,
    ) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        let scrollback = if up {
            view.scrollback
                .saturating_add(PANE_SCROLL_WHEEL_STEP)
                .min(scrollback_len)
        } else {
            view.scrollback.saturating_sub(PANE_SCROLL_WHEEL_STEP)
        };
        if view.scrollback == scrollback {
            return false;
        }
        view.scrollback = scrollback;
        true
    }

    pub(in crate::tui) fn reset_scrollback(&mut self, pane_id: PaneId) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == 0 {
            return false;
        }
        view.scrollback = 0;
        true
    }

    /// Keeps a scrolled-back local viewport pinned while the host appends visual rows.
    pub fn pin_scrollback_after_output(
        &mut self,
        pane_id: PaneId,
        appended_rows: usize,
        scrollback_len: usize,
    ) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == 0 {
            return false;
        }
        let scrollback = view
            .scrollback
            .saturating_add(appended_rows)
            .min(scrollback_len);
        if view.scrollback == scrollback {
            return false;
        }
        view.scrollback = scrollback;
        true
    }
}
