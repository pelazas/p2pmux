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

    /// Put every pane back at its live edge.
    ///
    /// For the client, which keeps no scrollback and holds only the viewports
    /// the node built for it: the moment those are thrown away — a resize
    /// reflows every retained row, so they are — the panes are showing live
    /// output again, and an offset left behind says otherwise. It hid the caret
    /// on a pane that was at its live edge, and made the next several notches of
    /// wheel-down walk back through rows that were no longer on screen.
    pub(crate) fn reset_all_scrollback(&mut self) -> bool {
        let mut moved = false;
        for view in self.pane_views.values_mut() {
            moved |= std::mem::take(&mut view.scrollback) != 0;
        }
        moved
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

#[cfg(test)]
mod tests {

    use ratatui::layout::Rect;

    use crate::tui::{MultiPaneTui, test_support::split_layout};

    #[test]
    fn mouse_wheel_adjusts_the_hovered_pane_scrollback_with_clamping() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let pane_id = tui.pane_at_or_focused(60, 17, area);
        assert_eq!(pane_id, 3);

        assert!(tui.scroll_pane(pane_id, 10, true));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 3);
        assert!(tui.scroll_pane(pane_id, 4, true));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 4);
        assert!(!tui.scroll_pane(pane_id, 4, true));
        assert!(tui.scroll_pane(pane_id, 4, false));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 1);
        assert!(tui.scroll_pane(pane_id, 4, false));
        assert!(!tui.scroll_pane(pane_id, 4, false));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 0);
    }

    /// A resize throws away every viewport the node built, so every pane is
    /// back at its live edge whether or not it was the one being scrolled.
    #[test]
    fn dropping_the_fetched_history_returns_every_pane_to_the_live_edge() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        assert!(tui.scroll_pane(1, 40, true));
        assert!(tui.scroll_pane(3, 40, true));
        assert!(tui.scroll_pane(3, 40, true));

        assert!(tui.reset_all_scrollback());

        assert_eq!(tui.pane_view(1).expect("pane view").scrollback, 0);
        assert_eq!(tui.pane_view(3).expect("pane view").scrollback, 0);
        assert!(
            !tui.reset_all_scrollback(),
            "nothing moved, so nothing to redraw"
        );
    }

    #[test]
    fn input_resets_the_pane_scrollback_to_the_live_edge() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        assert!(tui.scroll_pane(1, 4, true));
        assert!(tui.reset_scrollback(1));
        assert_eq!(tui.pane_view(1).expect("pane view").scrollback, 0);
        assert!(!tui.reset_scrollback(1));
    }

    #[test]
    fn appended_local_history_pins_a_scrolled_back_viewport() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        assert!(tui.scroll_pane(1, 10, true));
        assert!(tui.scroll_pane(1, 10, true));
        assert!(tui.pin_scrollback_after_output(1, 3, 10));
        assert_eq!(tui.pane_view(1).expect("pane view").scrollback, 9);
        tui.reset_scrollback(1);
        assert!(!tui.pin_scrollback_after_output(1, 3, 10));
    }
}
