//! What a click, drag, or wheel does: focus, text selection, border resizes,
//! and forwarding to a child that asked for mouse reporting.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

use crate::{
    layout::{Axis, PaneId},
    tui::{
        MouseHandling, MultiPaneTui, PaneMouseProtocol, PaneTextSelection, ScreenCell,
        SelectionPoint, UiIntent,
        geometry::{
            ResizeDrag, clamp_to_viewport, mouse_to_screen_cell, nearest_split_for_pane, pane_at,
            pane_content_rect, rect_contains, resize_border_hit, resize_proposed_share,
        },
        input::mouse::encode_mouse,
        selection::selection_text,
    },
};

/// How often a drag held past a pane's edge pulls one more row into view.
///
/// A terminal only reports a drag when the pointer changes cell, so a pointer
/// parked at the edge sends nothing at all: without a clock of its own this
/// would scroll one row and stop. Slow enough to read what goes past, fast
/// enough that reaching for something a page back is not a wait.
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(60);

/// A selection drag that has left its pane through the top or the bottom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) struct SelectionAutoscroll {
    pub(in crate::tui) pane_id: PaneId,
    /// Which way the *content* moves: past the top pulls older rows in.
    pub(in crate::tui) up: bool,
    /// Where the pointer is across the pane, so the row it drags over is
    /// selected up to the same column it would be inside the pane.
    column: u16,
    last_step: Option<Instant>,
}

impl MultiPaneTui {
    pub(in crate::tui) fn clear_selection(&mut self) -> bool {
        let changed = self.selection.take().is_some();
        self.selection_dragging = false;
        self.selection_autoscroll = None;
        changed
    }

    pub(in crate::tui) fn begin_selection_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        let point = SelectionPoint {
            scrollback: self.scrollback_offset(pane_id),
            cell,
        };
        self.selection = Some(PaneTextSelection {
            pane_id,
            anchor: point,
            cursor: point,
        });
        self.selection_dragging = true;
        self.selection_autoscroll = None;
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
        let scrollback = self.scrollback_offset(pane_id);
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        let point = SelectionPoint { scrollback, cell };
        if selection.pane_id != pane_id || selection.cursor == point {
            return false;
        }
        selection.cursor = point;
        true
    }

    /// Note whether this drag has left the selection's pane through an edge,
    /// and which one.
    ///
    /// Only the top and the bottom: dragging out of the side of a pane is
    /// reaching for the pane next door, not for more of this one, and pulling
    /// the buffer under the pointer there would be a surprise.
    pub(in crate::tui) fn aim_selection_autoscroll(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> bool {
        let previous = self.selection_autoscroll;
        self.selection_autoscroll = self.aimed_autoscroll(column, row, area).map(|aim| {
            match previous {
                // Keep the clock running across a pointer still outside the
                // same edge, so a mouse wobbling above a pane does not scroll
                // faster than one held still.
                Some(previous) if previous.pane_id == aim.pane_id && previous.up == aim.up => {
                    SelectionAutoscroll {
                        column: aim.column,
                        ..previous
                    }
                }
                _ => aim,
            }
        });
        self.selection_autoscroll != previous
    }

    fn aimed_autoscroll(&self, column: u16, row: u16, area: Rect) -> Option<SelectionAutoscroll> {
        if !self.selection_dragging {
            return None;
        }
        let selection = self.selection?;
        let viewport = self.pane_screen_viewport(selection.pane_id, area)?;
        let up = if row < viewport.y {
            true
        } else if row >= viewport.y.saturating_add(viewport.height) {
            false
        } else {
            return None;
        };
        Some(SelectionAutoscroll {
            pane_id: selection.pane_id,
            up,
            column: column
                .saturating_sub(viewport.x)
                .min(viewport.width.saturating_sub(1)),
            // Pulled immediately: the drag that crossed the edge is itself the
            // first step, and waiting an interval before the first row makes
            // the edge feel dead.
            last_step: None,
        })
    }

    /// The pane the pointer is currently dragging out of, if any.
    pub(crate) fn selection_autoscroll_pane(&self) -> Option<PaneId> {
        self.selection_autoscroll.map(|aim| aim.pane_id)
    }

    /// Whether another row is due, and which way. Rate-limited, so a caller may
    /// ask on every loop iteration.
    pub(crate) fn selection_autoscroll_due(&mut self, now: Instant) -> Option<(PaneId, bool)> {
        let aim = self.selection_autoscroll.as_mut()?;
        if aim
            .last_step
            .is_some_and(|last| now.duration_since(last) < SELECTION_AUTOSCROLL_INTERVAL)
        {
            return None;
        }
        aim.last_step = Some(now);
        Some((aim.pane_id, aim.up))
    }

    /// Put the selection's moving end on the edge row the drag is pulling at.
    ///
    /// Called after the caller has actually scrolled the pane — which is not
    /// something this type can do for an attached client, where the rows have
    /// to be fetched before the viewport may move.
    pub(crate) fn follow_selection_autoscroll(&mut self, area: Rect) -> bool {
        let Some(aim) = self.selection_autoscroll else {
            return false;
        };
        let Some(viewport) = self.pane_screen_viewport(aim.pane_id, area) else {
            return false;
        };
        let scrollback = self.scrollback_offset(aim.pane_id);
        let point = SelectionPoint {
            scrollback,
            cell: self.source_cell(
                aim.pane_id,
                ScreenCell {
                    row: if aim.up {
                        0
                    } else {
                        viewport.height.saturating_sub(1)
                    },
                    col: aim.column,
                },
            ),
        };
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        if selection.pane_id != aim.pane_id || selection.cursor == point {
            return false;
        }
        selection.cursor = point;
        true
    }

    /// A pane's screen area — where its cells are drawn, inside its border.
    fn pane_screen_viewport(&self, pane_id: PaneId, area: Rect) -> Option<Rect> {
        let rect = *self.geometry(area).panes.get(&pane_id)?;
        let pane = self.snapshot.panes.get(&pane_id)?;
        self.pane_grid_viewport(
            pane_id,
            pane_content_rect(rect),
            (pane.grid_rows, pane.grid_cols),
        )
    }

    pub(in crate::tui) fn end_selection_drag(&mut self) -> bool {
        self.selection_autoscroll = None;
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

    /// Scalar selection ends for the local node protocol. The node rebuilds
    /// its private UI type before reading its authoritative scrollback.
    pub(crate) fn selection_copy_coordinates(
        &self,
    ) -> Option<(PaneId, u64, u16, u16, u64, u16, u16)> {
        let selection = self.selection()?;
        Some((
            selection.pane_id,
            selection.anchor.scrollback as u64,
            selection.anchor.cell.row,
            selection.anchor.cell.col,
            selection.cursor.scrollback as u64,
            selection.cursor.cell.row,
            selection.cursor.cell.col,
        ))
    }

    /// The selected text, read from whichever views the caller can supply.
    ///
    /// `view_at` answers "the pane, scrolled back this many rows". A selection
    /// that never left the viewport only ever asks for `0`; one dragged past
    /// the edge asks for the offsets it scrolled through.
    pub(crate) fn selected_text<'a>(
        &self,
        view_at: impl Fn(usize) -> Option<std::borrow::Cow<'a, vt100::Screen>>,
    ) -> Option<String> {
        self.selection()
            .and_then(|selection| selection_text(selection, view_at))
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
        let viewport = self.pane_grid_viewport(
            pane_id,
            pane_content_rect(pane_rect),
            (pane.grid_rows, pane.grid_cols),
        )?;
        mouse_to_screen_cell(viewport, column, row)
            .map(|cell| (pane_id, self.source_cell(pane_id, cell)))
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

    /// Hit-tests the tab bar. `None` means the click landed somewhere else;
    /// `Some` means the bar answered it, with whatever the node needs to hear.
    ///
    /// The two are worth distinguishing because a click the bar consumed must
    /// not go on to focus a pane or start a selection underneath it.
    pub(in crate::tui) fn switch_tab_at(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Option<Vec<UiIntent>> {
        let geometry = self.geometry(area);
        // The badge is drawn to look like a tab, so it has to answer a click
        // like one. Anything else makes the one clickable-looking thing on the
        // bar the one thing that does nothing.
        //
        // Both directions, the way `Ctrl+O` toggles: a badge that only ever
        // opens the inbox is a door with no handle on the inside, and the way
        // back out would be a key the mouse user never learned. `current_tab`
        // does not move while the inbox is open, so coming back needs no
        // remembered state — closing Home lands on the tab already in view.
        if rect_contains(self.inbox_rect(geometry.tab_bar), column, row) {
            let open = !self.home_open();
            self.set_home_open(open, "mouse");
            self.clear_zoom();
            return Some(Vec::new());
        }
        let tab_id = geometry
            .tab_labels
            .iter()
            .find_map(|(tab_id, rect)| rect_contains(*rect, column, row).then_some(*tab_id))?;
        // Clicking a tab is a way out of Home as well as a way between tabs.
        self.set_home_open(false, "mouse");
        self.clear_zoom();
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(vec![UiIntent::SwitchTab { tab_id }])
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
            MouseEventKind::Moved if !self.home_open() => {
                self.hover_pane_at(mouse.column, mouse.row, area);
                MouseHandling::default()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.home_open() {
                    // The tab bar is still on screen above the inbox, so it
                    // still answers clicks — the badge to come back out, a tab
                    // label to land on that tab. Without this the whole bar is
                    // painted but dead the moment the inbox opens.
                    if let Some(intents) = self.switch_tab_at(mouse.column, mouse.row, area) {
                        return MouseHandling {
                            intents,
                            ..MouseHandling::default()
                        };
                    }
                    return MouseHandling {
                        intents: self.handle_home_click(mouse.column, mouse.row, area),
                        ..MouseHandling::default()
                    };
                }
                self.clear_selection();
                if self.begin_resize_drag(mouse.column, mouse.row, area) {
                    return MouseHandling::default();
                }
                if let Some(intents) = self.switch_tab_at(mouse.column, mouse.row, area) {
                    return MouseHandling {
                        intents,
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
                    // Aimed after extending, not instead of it: a drag that
                    // leaves through the top still has a last position inside
                    // the pane worth selecting to.
                    self.aim_selection_autoscroll(mouse.column, mouse.row, area);
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

    /// Encodes a wheel notch for the child of the pane it was aimed at.
    ///
    /// The wheel is the one gesture addressed by where the pointer is rather
    /// than by what has focus, so the question "does this child want the
    /// wheel?" has to be asked of that pane and answered against that pane's
    /// viewport. `None` means the notch stays with the mux, for local
    /// scrollback.
    pub fn wheel_bytes_for_pane(
        &self,
        mouse: crossterm::event::MouseEvent,
        area: Rect,
        protocol: PaneMouseProtocol,
        pane_id: PaneId,
    ) -> Option<Vec<u8>> {
        // Shift is the standing escape hatch: it always scrolls, never forwards.
        // A pane already held back in its history keeps the wheel too, or there
        // would be no way to come back to the live edge.
        if self.home_open()
            || !protocol.reports_mouse()
            || mouse.modifiers.contains(KeyModifiers::SHIFT)
            || self.scrollback_offset(pane_id) != 0
        {
            return None;
        }
        let viewport = self.pane_screen_viewport(pane_id, area)?;
        let cell = mouse_to_screen_cell(viewport, mouse.column, mouse.row)?;
        encode_mouse(
            mouse.kind,
            mouse.modifiers,
            self.source_cell(pane_id, cell),
            protocol,
        )
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
        if self.home_open()
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
            forward_bytes: encode_mouse(
                mouse.kind,
                mouse.modifiers,
                self.source_cell(self.focused_pane, cell),
                protocol,
            ),
            ..MouseHandling::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ratatui::layout::Rect;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

    use crate::{
        layout::{Axis, Node, Tab},
        tui::{
            ChordMode, MultiPaneTui, PaneMouseProtocol, ScreenCell, UiIntent,
            test_support::{layout, mouse_protocol, split_layout},
        },
    };

    use super::SELECTION_AUTOSCROLL_INTERVAL;

    #[test]
    fn content_drag_keeps_the_selection_path() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(!tui.begin_resize_drag(2, 3, area));
        assert!(tui.resize_drag.is_none());
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        assert!(tui.end_selection_drag());
        assert!(tui.selection().is_some());
    }

    /// Issue #79: dragging out through the top of a pane has to keep pulling
    /// rows in for as long as the pointer stays there, and the pointer stops
    /// sending events the moment it stops moving.
    #[test]
    fn a_drag_out_of_a_panes_top_keeps_asking_for_another_row() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let viewport = tui.pane_screen_viewport(1, area).expect("pane 1 viewport");
        let inside = viewport.y + 2;
        let above = viewport.y.saturating_sub(1);

        assert!(tui.begin_selection_at(viewport.x + 3, inside, area));
        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                viewport.x + 3,
                inside,
            ),
            area,
            PaneMouseProtocol::default(),
        );
        assert_eq!(tui.selection_autoscroll_pane(), None, "still inside");

        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                viewport.x + 3,
                above,
            ),
            area,
            PaneMouseProtocol::default(),
        );
        assert_eq!(tui.selection_autoscroll_pane(), Some(1));

        // The first step is due immediately — an edge that waits before it
        // moves reads as an edge that does nothing.
        let start = Instant::now();
        assert_eq!(tui.selection_autoscroll_due(start), Some((1, true)));
        // …and the second is not, however many times it is asked. This is the
        // check that matters: this method is called on every pass of a loop
        // that runs at sixty frames a second.
        assert_eq!(tui.selection_autoscroll_due(start), None);
        assert_eq!(
            tui.selection_autoscroll_due(start + SELECTION_AUTOSCROLL_INTERVAL),
            Some((1, true))
        );

        // A pointer back inside the pane stops pulling; letting go stops it too.
        tui.handle_mouse(
            left_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                viewport.x + 3,
                inside,
            ),
            area,
            PaneMouseProtocol::default(),
        );
        assert_eq!(tui.selection_autoscroll_pane(), None);
        tui.aim_selection_autoscroll(viewport.x + 3, above, area);
        assert_eq!(tui.selection_autoscroll_pane(), Some(1));
        tui.end_selection_drag();
        assert_eq!(tui.selection_autoscroll_pane(), None);
    }

    /// Out through the bottom pulls the other way; out through the side pulls
    /// nothing, because that is reaching for the pane next door.
    #[test]
    fn only_the_top_and_bottom_edges_pull_a_pane() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let viewport = tui.pane_screen_viewport(1, area).expect("pane 1 viewport");

        assert!(tui.begin_selection_at(viewport.x + 3, viewport.y + 2, area));

        tui.aim_selection_autoscroll(viewport.x + 3, viewport.y + viewport.height, area);
        assert_eq!(
            tui.selection_autoscroll_due(Instant::now()),
            Some((1, false))
        );

        tui.aim_selection_autoscroll(viewport.x + viewport.width + 4, viewport.y + 2, area);
        assert_eq!(tui.selection_autoscroll_pane(), None);
    }

    /// The selection has to stay on the *text* it was dragged over. If it stayed
    /// on the screen rows instead, scrolling would drag the anchor along with
    /// the view and the selection would never grow.
    #[test]
    fn scrolling_under_a_drag_grows_the_selection_instead_of_moving_it() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let viewport = tui.pane_screen_viewport(1, area).expect("pane 1 viewport");

        assert!(tui.begin_selection_at(viewport.x + 3, viewport.y + 2, area));
        let anchor = tui.selection.expect("a selection").anchor;
        tui.aim_selection_autoscroll(viewport.x + 3, viewport.y.saturating_sub(1), area);

        let mut now = Instant::now();
        for expected in 1..=3 {
            assert!(tui.selection_autoscroll_due(now).is_some());
            now += SELECTION_AUTOSCROLL_INTERVAL;
            assert!(tui.scroll_pane(1, 100, true));
            tui.follow_selection_autoscroll(area);
            let selection = tui.selection.expect("a selection");
            assert_eq!(selection.anchor, anchor, "the anchor stays on its line");
            // Three wheel steps back, and the moving end is on the top row of
            // what is now on screen — which is three lines further from the
            // anchor each time.
            assert_eq!(selection.cursor.cell.row, 0);
            assert_eq!(
                selection.cursor.line(),
                anchor.line() - i64::from(expected) * 3 - 2
            );
        }
    }

    fn left_mouse(kind: MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn reporting_child() -> PaneMouseProtocol {
        mouse_protocol(
            vt100::MouseProtocolMode::ButtonMotion,
            vt100::MouseProtocolEncoding::Sgr,
        )
    }

    #[test]
    fn clicking_a_focused_reporting_pane_forwards_instead_of_selecting() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, Some(b"\x1b[<0;2;2M".to_vec()));
        assert!(handling.intents.is_empty());
        assert_eq!(tui.selection(), None);
    }

    #[test]
    fn focus_moving_mid_gesture_ends_the_forwarding_instead_of_releasing_elsewhere() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            protocol,
        );
        // The node moved focus while the button was still down.
        tui.set_focus(1, 2).expect("the split has a second pane");
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 4, 3),
            area,
            protocol,
        );

        assert_eq!(up.forward_bytes, None);
    }

    #[test]
    fn a_forwarded_press_keeps_the_drag_and_release_away_from_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            protocol,
        );
        let drag = tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 4, 3),
            area,
            protocol,
        );
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 4, 3),
            area,
            protocol,
        );

        assert_eq!(drag.forward_bytes, Some(b"\x1b[<32;4;2M".to_vec()));
        assert_eq!(up.forward_bytes, Some(b"\x1b[<0;4;2m".to_vec()));
        assert!(!up.copy_selection_requested);
        assert_eq!(tui.selection(), None);
    }

    #[test]
    fn a_drag_that_leaves_the_pane_still_reports_inside_its_edge() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            protocol,
        );
        let drag = tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 70, 20),
            area,
            protocol,
        );

        // The 10x4 grid clamps the report to its last cell rather than dropping it.
        assert_eq!(drag.forward_bytes, Some(b"\x1b[<32;10;4M".to_vec()));
    }

    #[test]
    fn shift_click_selects_even_when_the_child_reports_mouse() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let shift_down = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::SHIFT,
        };

        let handling = tui.handle_mouse(shift_down, area, reporting_child());

        assert_eq!(handling.forward_bytes, None);
        assert!(tui.selection_dragging);
    }

    #[test]
    fn clicking_an_unfocused_reporting_pane_only_moves_focus() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(tui.focused_pane(), 1);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 45, 3),
            area,
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
        assert_eq!(tui.focused_pane(), 2);
        assert!(matches!(
            handling.intents.as_slice(),
            [UiIntent::FocusPane { pane_id: 2 }]
        ));
    }

    #[test]
    fn a_scrolled_back_pane_selects_history_instead_of_forwarding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.set_pane_scrollback_offset(1, 5));

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            area,
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
        assert!(tui.selection_dragging);
    }

    #[test]
    fn the_wheel_reaches_a_reporting_child_over_its_own_pane() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::ScrollUp, 2, 3),
            area,
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, Some(b"\x1b[<64;2;2M".to_vec()));
    }

    #[test]
    fn the_wheel_stays_local_over_a_pane_the_reporting_child_does_not_own() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::ScrollDown, 45, 3),
            area,
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
    }

    #[test]
    fn the_wheel_reaches_a_reporting_child_in_a_pane_that_does_not_have_focus() {
        let tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        // Focus stays on pane 1 on the left; the pointer is over pane 2.
        let viewport = tui.pane_screen_viewport(2, area).expect("pane 2 is drawn");

        let bytes = tui.wheel_bytes_for_pane(
            left_mouse(MouseEventKind::ScrollUp, viewport.x, viewport.y),
            area,
            reporting_child(),
            2,
        );

        assert_eq!(tui.focused_pane(), 1);
        assert_eq!(bytes, Some(b"\x1b[<64;1;1M".to_vec()));
    }

    #[test]
    fn an_unfocused_pane_showing_history_keeps_its_own_wheel() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let viewport = tui.pane_screen_viewport(2, area).expect("pane 2 is drawn");
        assert!(tui.set_pane_scrollback_offset(2, 3));

        let bytes = tui.wheel_bytes_for_pane(
            left_mouse(MouseEventKind::ScrollUp, viewport.x, viewport.y),
            area,
            reporting_child(),
            2,
        );

        // Pane 1 has focus and is at the live edge, so reading the offset off
        // focus instead of off the pointer would have forwarded this notch and
        // stranded pane 2 in its history.
        assert_eq!(bytes, None);
    }

    #[test]
    fn the_wheel_stays_local_while_a_reporting_pane_shows_history() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.set_pane_scrollback_offset(1, 3));

        let handling = tui.handle_mouse(
            left_mouse(MouseEventKind::ScrollDown, 2, 3),
            area,
            reporting_child(),
        );

        assert_eq!(handling.forward_bytes, None);
    }

    #[test]
    fn a_reporting_child_never_takes_a_border_drag_from_the_mux() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let protocol = reporting_child();

        let down = tui.handle_mouse(
            left_mouse(MouseEventKind::Down(MouseButton::Left), 39, 5),
            area,
            protocol,
        );
        tui.handle_mouse(
            left_mouse(MouseEventKind::Drag(MouseButton::Left), 49, 5),
            area,
            protocol,
        );
        let up = tui.handle_mouse(
            left_mouse(MouseEventKind::Up(MouseButton::Left), 49, 5),
            area,
            protocol,
        );

        assert_eq!(down.forward_bytes, None);
        assert!(matches!(
            up.intents.as_slice(),
            [UiIntent::SetSplitRatio { .. }]
        ));
    }

    #[test]
    fn mouse_up_requests_copy_only_for_a_nonempty_selection_without_resize_commit() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let down = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };

        assert!(
            tui.handle_mouse(down, area, PaneMouseProtocol::default())
                .intents
                .is_empty()
        );
        tui.handle_mouse(drag, area, PaneMouseProtocol::default());
        let handling = tui.handle_mouse(up, area, PaneMouseProtocol::default());
        assert!(handling.intents.is_empty());
        assert!(handling.copy_selection_requested);

        let resize_down = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 39,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let resize_drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 49,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let resize_up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 49,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        tui.handle_mouse(resize_down, area, PaneMouseProtocol::default());
        tui.handle_mouse(resize_drag, area, PaneMouseProtocol::default());
        let handling = tui.handle_mouse(resize_up, area, PaneMouseProtocol::default());
        assert!(matches!(
            handling.intents.as_slice(),
            [UiIntent::SetSplitRatio { .. }]
        ));
        assert!(!handling.copy_selection_requested);
    }

    #[test]
    fn dragging_a_shared_vertical_border_resizes_its_split() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));
        assert!(matches!(
            tui.end_resize_drag(49, 5),
            Some(UiIntent::SetSplitRatio {
                pane_id: 1,
                axis: Axis::LeftRight,
                first_share_bps: 6_250,
                base_revision: 1,
            })
        ));
        assert!(tui.end_resize_drag(49, 5).is_none());

        assert!(tui.begin_resize_drag(40, 5, area));
        assert!(tui.extend_resize_drag(50, 5));
        assert!(matches!(
            tui.end_resize_drag(50, 5),
            Some(UiIntent::SetSplitRatio {
                pane_id: 2,
                axis: Axis::LeftRight,
                first_share_bps: 6_250,
                base_revision: 1,
            })
        ));
    }

    #[test]
    fn resize_drag_previews_geometry_without_mutating_the_snapshot() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let snapshot = tui.snapshot().clone();
        let initial = tui.geometry(area);

        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));

        let preview = tui.geometry(area);
        assert_eq!(initial.panes[&1], Rect::new(0, 1, 40, 22));
        assert_eq!(preview.panes[&1], Rect::new(0, 1, 50, 22));
        assert_eq!(tui.snapshot(), &snapshot);
        assert!(matches!(
            tui.end_resize_drag(49, 5),
            Some(UiIntent::SetSplitRatio {
                pane_id: 1,
                axis: Axis::LeftRight,
                first_share_bps: 6_250,
                base_revision: 1,
            })
        ));
        assert_eq!(tui.snapshot(), &snapshot);
        assert_eq!(tui.geometry(area), initial);
    }

    #[test]
    fn dragging_a_shared_horizontal_border_resizes_its_split() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let initial = tui.geometry(area);
        assert!(tui.begin_resize_drag(60, 11, area));
        assert!(tui.extend_resize_drag(60, 11));
        assert_eq!(tui.geometry(area), initial);
        assert!(tui.extend_resize_drag(60, 16));
        assert_eq!(tui.geometry(area).panes[&2], Rect::new(40, 1, 40, 15));
        assert!(matches!(
            tui.end_resize_drag(60, 16),
            Some(UiIntent::SetSplitRatio {
                pane_id: 2,
                axis: Axis::TopBottom,
                first_share_bps: 7_272,
                base_revision: 1,
            })
        ));
    }

    #[test]
    fn mouse_hit_testing_focuses_the_clicked_pane_and_ignores_misses() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );

        assert!(tui.focus_pane_at(60, 17, area));
        assert_eq!(tui.focused_pane(), 3);
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert!(!tui.focus_pane_at(10, 0, area));
        assert_eq!(tui.focused_pane(), 3);
    }

    #[test]
    fn hovering_an_unfocused_pane_does_not_move_focus() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert!(tui.hover_pane_at(60, 17, area));
        assert_eq!(tui.hovered_pane, Some(3));
        assert_eq!(tui.focused_pane(), 1);
        assert!(tui.hover_pane_at(10, 0, area));
        assert_eq!(tui.hovered_pane, None);
        assert_eq!(tui.focused_pane(), 1);
    }

    #[test]
    fn selection_hit_testing_uses_the_bordered_pane_content_area() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 19, 78)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(tui.screen_cell_at(0, 1, area), None);
        assert_eq!(
            tui.screen_cell_at(1, 2, area),
            Some((1, ScreenCell { row: 0, col: 0 }))
        );
    }

    #[test]
    fn selection_hit_testing_translates_the_local_viewport_origin() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 19, 78)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        tui.pane_view(1)
            .expect("pane view")
            .origin
            .set(ScreenCell { row: 5, col: 7 });

        assert_eq!(
            tui.screen_cell_at(4, 6, area),
            Some((1, ScreenCell { row: 9, col: 10 }))
        );
        assert!(tui.begin_selection_at(4, 6, area));
        assert_eq!(
            tui.selection().expect("selection").anchor.cell,
            ScreenCell { row: 9, col: 10 },
        );
    }

    #[test]
    fn tab_hit_testing_switches_the_clicked_rendered_label_without_exiting_tab_mode() {
        let mut tui = MultiPaneTui::new(layout(
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
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 32, 8);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );

        let second_tab = tui.geometry(area).tab_labels[&2];
        assert_eq!(
            tui.switch_tab_at(second_tab.x, 0, area),
            Some(vec![UiIntent::SwitchTab { tab_id: 2 }])
        );
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        assert_eq!(tui.switch_tab_at(0, 0, area), None);
    }

    /// The badge opens the inbox and closes it again, so the mouse alone is a
    /// complete way between the two — no reaching for `Ctrl+O` or `n`.
    #[test]
    fn clicking_the_inbox_badge_a_second_time_returns_to_the_tab_you_left() {
        let mut tui = MultiPaneTui::new(layout(
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
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 40, 8);
        tui.select_tab(2).expect("second tab exists");

        let badge = tui.inbox_rect(tui.geometry(area).tab_bar);
        assert_eq!(tui.switch_tab_at(badge.x, badge.y, area), Some(Vec::new()));
        assert!(tui.home_open(), "the badge opens the inbox");

        assert_eq!(tui.switch_tab_at(badge.x, badge.y, area), Some(Vec::new()));
        assert!(!tui.home_open(), "and the same click closes it again");
        assert_eq!(
            tui.current_tab(),
            2,
            "coming back lands on the tab that was in view, not the first one"
        );
    }

    /// Clicks on the tab bar reach the bar, not the inbox list underneath it.
    #[test]
    fn a_tab_label_clicked_from_inside_the_inbox_leaves_the_inbox_for_that_tab() {
        let mut tui = MultiPaneTui::new(layout(
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
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 40, 8);
        tui.set_home_open(true, "test");

        let second_tab = tui.geometry(area).tab_labels[&2];
        let handling = tui.handle_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: second_tab.x,
                row: second_tab.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
            PaneMouseProtocol::default(),
        );

        assert_eq!(handling.intents, vec![UiIntent::SwitchTab { tab_id: 2 }]);
        assert!(!tui.home_open());
        assert_eq!(tui.current_tab(), 2);
    }
}
