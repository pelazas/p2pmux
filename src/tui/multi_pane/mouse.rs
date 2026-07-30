//! What a click, drag, or wheel does: focus, text selection, border resizes,
//! and forwarding to a child that asked for mouse reporting.

use ratatui::layout::Rect;

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

use crate::{
    layout::{Axis, PaneId},
    tui::{
        MouseHandling, MultiPaneTui, PaneMouseProtocol, PaneTextSelection, ScreenCell, UiIntent,
        geometry::{
            ResizeDrag, clamp_to_viewport, fixed_grid_viewport, mouse_to_screen_cell,
            nearest_split_for_pane, pane_at, pane_content_rect, rect_contains, resize_border_hit,
            resize_proposed_share,
        },
        input::mouse::encode_mouse,
        selection::selection_text,
    },
};

impl MultiPaneTui {
    pub(in crate::tui) fn clear_selection(&mut self) -> bool {
        let changed = self.selection.take().is_some();
        self.selection_dragging = false;
        changed
    }

    pub(in crate::tui) fn begin_selection_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        self.selection = Some(PaneTextSelection {
            pane_id,
            anchor: cell,
            cursor: cell,
        });
        self.selection_dragging = true;
        true
    }

    pub(in crate::tui) fn extend_selection_at(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> bool {
        if !self.selection_dragging {
            return false;
        }
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        if selection.pane_id != pane_id || selection.cursor == cell {
            return false;
        }
        selection.cursor = cell;
        true
    }

    pub(in crate::tui) fn end_selection_drag(&mut self) -> bool {
        std::mem::replace(&mut self.selection_dragging, false)
    }

    pub(in crate::tui) fn begin_resize_drag(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let geometry = self.geometry(area);
        let Some((pane_id, horizontal, vertical)) = resize_border_hit(&geometry.panes, column, row)
        else {
            return false;
        };
        self.resize_drag = Some(ResizeDrag {
            pane_id,
            base_revision: self.snapshot.revision,
            origin_column: column,
            origin_row: row,
            axis: None,
            horizontal,
            vertical,
            original_share_bps: 0,
            preview_first_share_bps: None,
            span: 1,
            content: geometry.content,
        });
        if horizontal != vertical {
            self.lock_resize_drag(if horizontal {
                Axis::LeftRight
            } else {
                Axis::TopBottom
            });
        }
        true
    }

    pub(in crate::tui) fn extend_resize_drag(&mut self, column: u16, row: u16) -> bool {
        let Some(drag) = self.resize_drag else {
            return false;
        };
        if drag.axis.is_none() {
            let horizontal = i32::from(column) - i32::from(drag.origin_column);
            let vertical = i32::from(row) - i32::from(drag.origin_row);
            if horizontal.unsigned_abs().max(vertical.unsigned_abs()) < 2 {
                return true;
            }
            let axis = match (drag.horizontal, drag.vertical) {
                (true, false) => Axis::LeftRight,
                (false, true) => Axis::TopBottom,
                (true, true) if horizontal.unsigned_abs() >= vertical.unsigned_abs() => {
                    Axis::LeftRight
                }
                (true, true) => Axis::TopBottom,
                (false, false) => {
                    self.resize_drag = None;
                    return true;
                }
            };
            self.lock_resize_drag(axis);
        }
        if let Some(drag) = self.resize_drag.as_mut()
            && drag.axis.is_some()
        {
            drag.preview_first_share_bps = Some(resize_proposed_share(*drag, column, row));
        }
        true
    }

    pub(in crate::tui) fn lock_resize_drag(&mut self, axis: Axis) {
        let Some(drag) = self.resize_drag else {
            return;
        };
        let Some(tab) = self.current_tab_layout() else {
            self.resize_drag = None;
            return;
        };
        let Some(target) = nearest_split_for_pane(&tab.root, drag.pane_id, axis, drag.content)
        else {
            self.resize_drag = None;
            return;
        };
        let drag = self.resize_drag.as_mut().expect("drag remains active");
        drag.axis = Some(axis);
        drag.original_share_bps = target.first_share_bps;
        drag.span = target.span;
    }

    pub(in crate::tui) fn end_resize_drag(&mut self, column: u16, row: u16) -> Option<UiIntent> {
        let drag = self.resize_drag.take()?;
        let axis = drag.axis?;
        let proposed = resize_proposed_share(drag, column, row);
        (proposed != drag.original_share_bps).then_some(UiIntent::SetSplitRatio {
            pane_id: drag.pane_id,
            axis,
            first_share_bps: proposed,
            base_revision: drag.base_revision,
        })
    }

    pub(in crate::tui) fn cancel_resize_drag(&mut self) -> bool {
        self.resize_drag.take().is_some()
    }

    pub(in crate::tui) fn selection(&self) -> Option<PaneTextSelection> {
        self.selection.filter(|selection| !selection.is_empty())
    }

    pub(crate) fn selection_pane(&self) -> Option<PaneId> {
        self.selection().map(|selection| selection.pane_id)
    }

    pub(crate) fn selected_text(&self, screen: &vt100::Screen) -> Option<String> {
        self.selection()
            .and_then(|selection| selection_text(screen, selection))
    }

    pub(in crate::tui) fn screen_cell_at(
        &self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Option<(PaneId, ScreenCell)> {
        let geometry = self.geometry(area);
        let (pane_id, pane_rect) = geometry.panes.iter().find_map(|(pane_id, rect)| {
            rect_contains(*rect, column, row).then_some((*pane_id, *rect))
        })?;
        let pane = self.snapshot.panes.get(&pane_id)?;
        let viewport =
            fixed_grid_viewport(pane_content_rect(pane_rect), pane.grid_rows, pane.grid_cols);
        mouse_to_screen_cell(viewport, column, row).map(|cell| (pane_id, cell))
    }

    pub(in crate::tui) fn pane_at_or_focused(&self, column: u16, row: u16, area: Rect) -> PaneId {
        pane_at(&self.geometry(area).panes, column, row).unwrap_or(self.focused_pane)
    }

    pub fn pane_at_or_focused_for_mouse(&self, column: u16, row: u16, area: Rect) -> PaneId {
        self.pane_at_or_focused(column, row, area)
    }

    pub(in crate::tui) fn focus_pane_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some(pane_id) = pane_at(&self.geometry(area).panes, column, row) else {
            return false;
        };
        self.select_pane(self.current_tab, pane_id, "mouse")
    }

    pub(in crate::tui) fn hover_pane_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let hovered_pane = pane_at(&self.geometry(area).panes, column, row);
        if self.hovered_pane == hovered_pane {
            return false;
        }
        self.hovered_pane = hovered_pane;
        true
    }

    pub(in crate::tui) fn switch_tab_at(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Option<UiIntent> {
        let tab_id = self
            .geometry(area)
            .tab_labels
            .iter()
            .find_map(|(tab_id, rect)| rect_contains(*rect, column, row).then_some(*tab_id))?;
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(UiIntent::SwitchTab { tab_id })
    }

    /// Applies the local half of mouse interaction and returns mutations for the node.
    pub fn handle_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        area: Rect,
        protocol: PaneMouseProtocol,
    ) -> MouseHandling {
        if self.modal_open() {
            self.mouse_forwarding = false;
            return MouseHandling::default();
        }
        if let Some(handling) = self.report_mouse_to_child(mouse, area, protocol) {
            return handling;
        }
        match mouse.kind {
            MouseEventKind::Moved if !self.overlay_open() => {
                self.hover_pane_at(mouse.column, mouse.row, area);
                MouseHandling::default()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.overlay_open() {
                    return MouseHandling {
                        intents: self.handle_agent_overlay_click(mouse.column, mouse.row, area),
                        ..MouseHandling::default()
                    };
                }
                self.clear_selection();
                if self.begin_resize_drag(mouse.column, mouse.row, area) {
                    return MouseHandling::default();
                }
                if let Some(intent) = self.switch_tab_at(mouse.column, mouse.row, area) {
                    return MouseHandling {
                        intents: vec![intent],
                        ..MouseHandling::default()
                    };
                }
                let changed = self.focus_pane_at(mouse.column, mouse.row, area);
                self.begin_selection_at(mouse.column, mouse.row, area);
                MouseHandling {
                    intents: changed
                        .then_some(UiIntent::FocusPane {
                            pane_id: self.focused_pane,
                        })
                        .into_iter()
                        .collect(),
                    ..MouseHandling::default()
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.extend_resize_drag(mouse.column, mouse.row) {
                    self.extend_selection_at(mouse.column, mouse.row, area);
                }
                MouseHandling::default()
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.end_selection_drag();
                let resize_intent = self.end_resize_drag(mouse.column, mouse.row);
                MouseHandling {
                    copy_selection_requested: resize_intent.is_none() && self.selection().is_some(),
                    intents: resize_intent.into_iter().collect(),
                    ..MouseHandling::default()
                }
            }
            _ => MouseHandling::default(),
        }
    }

    /// Hands a mouse event to the focused pane's child when that child asked for
    /// mouse reports, so a click lands as a caret move instead of a mux selection.
    ///
    /// Returns `None` when the mux keeps the event for its own focus, selection,
    /// resize, tab, and footer handling.
    pub(in crate::tui) fn report_mouse_to_child(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        area: Rect,
        protocol: PaneMouseProtocol,
    ) -> Option<MouseHandling> {
        // Shift is the standing escape hatch: it always selects, never forwards.
        if self.overlay_open()
            || !protocol.reports_mouse()
            || mouse.modifiers.contains(KeyModifiers::SHIFT)
            || self.scrollback_offset(self.focused_pane) != 0
        {
            self.mouse_forwarding = false;
            return None;
        }
        let viewport = self.focused_pane_viewport(area)?;
        let cell = match mouse.kind {
            // A press outside the focused pane still belongs to the mux: it moves
            // focus, drags a border, or switches a tab.
            MouseEventKind::Down(_) => {
                let cell = mouse_to_screen_cell(viewport, mouse.column, mouse.row)?;
                self.clear_selection();
                self.mouse_forwarding = true;
                cell
            }
            // Once a press is forwarded the child owns the rest of the gesture, even
            // if the pointer leaves the pane mid-drag.
            MouseEventKind::Drag(_) => {
                if !self.mouse_forwarding {
                    return None;
                }
                clamp_to_viewport(viewport, mouse.column, mouse.row)?
            }
            MouseEventKind::Up(_) => {
                if !std::mem::take(&mut self.mouse_forwarding) {
                    return None;
                }
                clamp_to_viewport(viewport, mouse.column, mouse.row)?
            }
            MouseEventKind::Moved => {
                // Hover chrome still tracks the pointer while motion is forwarded.
                self.hover_pane_at(mouse.column, mouse.row, area);
                mouse_to_screen_cell(viewport, mouse.column, mouse.row)?
            }
            _ => mouse_to_screen_cell(viewport, mouse.column, mouse.row)?,
        };
        Some(MouseHandling {
            forward_bytes: encode_mouse(mouse.kind, mouse.modifiers, cell, protocol),
            ..MouseHandling::default()
        })
    }
}
