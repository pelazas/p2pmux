//! Rect and layout-tree math: where panes land, what a drag hits, how a
//! split divides its span.

use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};

use crossterm::event::KeyCode;
use ratatui::{layout::Rect, widgets::Block};

use crate::{
    layout::{Axis, Node, PaneId},
    tui::ScreenCell,
};

pub(in crate::tui) fn first_leaf(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf { pane_id } => Some(*pane_id),
        Node::Split { first, .. } => first_leaf(first),
    }
}
pub(in crate::tui) fn visible_leaf_panes(node: &Node) -> Vec<PaneId> {
    match node {
        Node::Leaf { pane_id } => vec![*pane_id],
        Node::Split { first, second, .. } => {
            let mut panes = visible_leaf_panes(first);
            panes.extend(visible_leaf_panes(second));
            panes
        }
    }
}
pub(in crate::tui) fn pane_at(
    panes: &BTreeMap<PaneId, Rect>,
    column: u16,
    row: u16,
) -> Option<PaneId> {
    panes
        .iter()
        .find_map(|(pane_id, rect)| rect_contains(*rect, column, row).then_some(*pane_id))
}
pub(in crate::tui) fn resize_border_hit(
    panes: &BTreeMap<PaneId, Rect>,
    column: u16,
    row: u16,
) -> Option<(PaneId, bool, bool)> {
    panes.iter().find_map(|(pane_id, rect)| {
        if !rect_contains(*rect, column, row)
            || rect_contains(pane_content_rect(*rect), column, row)
        {
            return None;
        }
        let vertical = (rect.width > 0
            && column == rect.x
            && panes.iter().any(|(other_id, other)| {
                other_id != pane_id
                    && other.right() == rect.x
                    && rect_contains(*other, other.x, row)
            }))
            || (rect.width > 0
                && column == rect.right().saturating_sub(1)
                && panes.iter().any(|(other_id, other)| {
                    other_id != pane_id
                        && other.x == rect.right()
                        && rect_contains(*other, other.x, row)
                }));
        let horizontal = (rect.height > 0
            && row == rect.y
            && panes.iter().any(|(other_id, other)| {
                other_id != pane_id
                    && other.bottom() == rect.y
                    && rect_contains(*other, column, other.y)
            }))
            || (rect.height > 0
                && row == rect.bottom().saturating_sub(1)
                && panes.iter().any(|(other_id, other)| {
                    other_id != pane_id
                        && other.y == rect.bottom()
                        && rect_contains(*other, column, other.y)
                }));
        (vertical || horizontal).then_some((*pane_id, vertical, horizontal))
    })
}
pub(in crate::tui) fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    u32::from(column) >= u32::from(rect.x)
        && u32::from(column) < u32::from(rect.x) + u32::from(rect.width)
        && u32::from(row) >= u32::from(rect.y)
        && u32::from(row) < u32::from(rect.y) + u32::from(rect.height)
}
pub(in crate::tui) fn resize_proposed_share(drag: ResizeDrag, column: u16, row: u16) -> u16 {
    let delta = match drag.axis.expect("locked resize drag has an axis") {
        Axis::LeftRight => i32::from(column) - i32::from(drag.origin_column),
        Axis::TopBottom => i32::from(row) - i32::from(drag.origin_row),
    };
    (i32::from(drag.original_share_bps) + delta * 10_000 / i32::from(drag.span.max(1)))
        .clamp(1, 9_999) as u16
}
pub(in crate::tui) fn contains_leaf(node: &Node, pane_id: PaneId) -> bool {
    match node {
        Node::Leaf { pane_id: candidate } => *candidate == pane_id,
        Node::Split { first, second, .. } => {
            contains_leaf(first, pane_id) || contains_leaf(second, pane_id)
        }
    }
}
pub(in crate::tui) fn nearest_split_for_pane(
    node: &Node,
    pane_id: PaneId,
    axis: Axis,
    area: Rect,
) -> Option<SplitTarget> {
    let Node::Split {
        axis: split_axis,
        first_share_bps,
        first,
        second,
    } = node
    else {
        return None;
    };
    let (first_area, second_area) = split_areas(*split_axis, *first_share_bps, first, second, area);
    if contains_leaf(first, pane_id) {
        nearest_split_for_pane(first, pane_id, axis, first_area).or_else(|| {
            (*split_axis == axis).then_some(SplitTarget {
                first_share_bps: *first_share_bps,
                span: match axis {
                    Axis::LeftRight => area.width,
                    Axis::TopBottom => area.height,
                },
            })
        })
    } else if contains_leaf(second, pane_id) {
        nearest_split_for_pane(second, pane_id, axis, second_area).or_else(|| {
            (*split_axis == axis).then_some(SplitTarget {
                first_share_bps: *first_share_bps,
                span: match axis {
                    Axis::LeftRight => area.width,
                    Axis::TopBottom => area.height,
                },
            })
        })
    } else {
        None
    }
}
pub(in crate::tui) fn node_minimum(node: &Node) -> (u16, u16) {
    match node {
        Node::Leaf { .. } => (3, 3),
        Node::Split {
            axis,
            first,
            second,
            ..
        } => {
            let (first_width, first_height) = node_minimum(first);
            let (second_width, second_height) = node_minimum(second);
            match axis {
                Axis::LeftRight => (
                    first_width.saturating_add(second_width),
                    first_height.max(second_height),
                ),
                Axis::TopBottom => (
                    first_width.max(second_width),
                    first_height.saturating_add(second_height),
                ),
            }
        }
    }
}
fn allocated_first_span(span: u16, share_bps: u16, first_min: u16, second_min: u16) -> u16 {
    let unconstrained = ((u32::from(span) * u32::from(share_bps)) / 10_000) as u16;
    if span >= first_min.saturating_add(second_min) {
        unconstrained.clamp(first_min, span.saturating_sub(second_min))
    } else {
        unconstrained.min(span)
    }
}
pub(in crate::tui) fn split_areas(
    axis: Axis,
    first_share_bps: u16,
    first: &Node,
    second: &Node,
    area: Rect,
) -> (Rect, Rect) {
    match axis {
        Axis::LeftRight => {
            let (first_min, _) = node_minimum(first);
            let (second_min, _) = node_minimum(second);
            let first_width =
                allocated_first_span(area.width, first_share_bps, first_min, second_min);
            (
                Rect::new(area.x, area.y, first_width, area.height),
                Rect::new(
                    area.x.saturating_add(first_width),
                    area.y,
                    area.width - first_width,
                    area.height,
                ),
            )
        }
        Axis::TopBottom => {
            let (_, first_min) = node_minimum(first);
            let (_, second_min) = node_minimum(second);
            let first_height =
                allocated_first_span(area.height, first_share_bps, first_min, second_min);
            (
                Rect::new(area.x, area.y, area.width, first_height),
                Rect::new(
                    area.x,
                    area.y.saturating_add(first_height),
                    area.width,
                    area.height - first_height,
                ),
            )
        }
    }
}
pub(in crate::tui) fn allocate_node_with_preview(
    node: &Node,
    area: Rect,
    panes: &mut BTreeMap<PaneId, Rect>,
    preview: Option<ResizePreview>,
) {
    match node {
        Node::Leaf { pane_id } => {
            panes.insert(*pane_id, area);
        }
        Node::Split {
            axis,
            first_share_bps,
            first,
            second,
        } => {
            let share = preview
                .filter(|preview| {
                    preview.axis == *axis
                        && split_is_nearest_for_pane(node, preview.pane_id, preview.axis)
                })
                .map_or(*first_share_bps, |preview| preview.first_share_bps);
            let (first_area, second_area) = split_areas(*axis, share, first, second, area);
            allocate_node_with_preview(first, first_area, panes, preview);
            allocate_node_with_preview(second, second_area, panes, preview);
        }
    }
}
pub(in crate::tui) fn split_is_nearest_for_pane(node: &Node, pane_id: PaneId, axis: Axis) -> bool {
    let Node::Split {
        axis: split_axis,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    if *split_axis != axis {
        return false;
    }
    let child = if contains_leaf(first, pane_id) {
        first
    } else if contains_leaf(second, pane_id) {
        second
    } else {
        return false;
    };
    !contains_split_for_pane(child, pane_id, axis)
}
pub(in crate::tui) fn contains_split_for_pane(node: &Node, pane_id: PaneId, axis: Axis) -> bool {
    let Node::Split {
        axis: split_axis,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    if contains_leaf(first, pane_id) {
        *split_axis == axis || contains_split_for_pane(first, pane_id, axis)
    } else if contains_leaf(second, pane_id) {
        *split_axis == axis || contains_split_for_pane(second, pane_id, axis)
    } else {
        false
    }
}
pub(crate) fn grid_for_pane(rect: Rect) -> (u16, u16) {
    let content = pane_content_rect(rect);
    (content.height.max(1), content.width.max(1))
}
pub(crate) fn initial_root_pane_grid(cols: u16, rows: u16) -> (u16, u16) {
    grid_for_pane(Rect::new(0, 0, cols, rows.saturating_sub(2)))
}
pub(in crate::tui) fn rect_center(rect: Rect) -> (u32, u32) {
    (
        u32::from(rect.x) * 2 + u32::from(rect.width),
        u32::from(rect.y) * 2 + u32::from(rect.height),
    )
}
/// How far two spans overlap, in cells. `0` means they do not.
fn span_overlap(start: u16, length: u16, other_start: u16, other_length: u16) -> u16 {
    let end = start.saturating_add(length);
    let other_end = other_start.saturating_add(other_length);
    end.min(other_end).saturating_sub(start.max(other_start))
}

/// The pane an arrow should move focus to, or `None` if there is nothing that
/// way.
///
/// Replaces a comparison of pane *centres*, which produced two complaints that
/// turn out to be the same bug. A centre says nothing about whether two
/// rectangles are actually beside each other, so a pane sitting diagonally
/// counted as being above:
///
/// ```text
/// +--------------+----+
/// |              | 2  |     Up from pane 1 moved focus to pane 2, because
/// |      1       +----+     2's centre is higher than 1's -- never mind that
/// |              | 3  |     2 is to the *right* and nothing is above 1 at all.
/// +--------------+----+
/// ```
///
/// Which also explains "the arrows cannot even get to the desired pane": focus
/// that leaves sideways when you press Up does not come back when you press
/// Down, and from some panes there was no sequence of arrows that reached the
/// one you were looking at.
///
/// So a candidate has to *start* beyond the source along the axis being
/// travelled, and a candidate whose perpendicular span overlaps the source's
/// beats one that merely floats past a corner. Among equals, the nearest edge
/// wins, then the nearest perpendicular centre, then the lowest pane id so the
/// answer never depends on map order.
pub(in crate::tui) fn nearest_in_direction(
    source: Rect,
    candidates: impl IntoIterator<Item = (PaneId, Rect)>,
    direction: KeyCode,
) -> Option<PaneId> {
    let vertical = matches!(direction, KeyCode::Up | KeyCode::Down);
    if !vertical && !matches!(direction, KeyCode::Left | KeyCode::Right) {
        return None;
    }
    candidates
        .into_iter()
        .filter_map(|(pane_id, rect)| {
            // Clear of the source's *far* edge, not merely of its near one.
            // Comparing near edges reads a short pane stacked beside a tall one
            // as being below it -- pane 3 in the diagram above starts lower
            // than pane 1 does, while pane 1 runs past the bottom of it.
            //
            // `BORDER_SLACK` because two panes either side of a split share the
            // row or column their borders are drawn in, so a genuine neighbour
            // can start one cell before this pane's rectangle ends. Erring
            // towards admitting one is the cheap direction to be wrong in: the
            // overlap ranking below sorts it, whereas excluding it would strand
            // the pane.
            const BORDER_SLACK: u16 = 1;
            let beyond = match direction {
                KeyCode::Left => rect.x + rect.width <= source.x + BORDER_SLACK,
                KeyCode::Right => rect.x + BORDER_SLACK >= source.x + source.width,
                KeyCode::Up => rect.y + rect.height <= source.y + BORDER_SLACK,
                KeyCode::Down => rect.y + BORDER_SLACK >= source.y + source.height,
                _ => false,
            };
            if !beyond {
                return None;
            }
            let overlap = if vertical {
                span_overlap(source.x, source.width, rect.x, rect.width)
            } else {
                span_overlap(source.y, source.height, rect.y, rect.height)
            };
            let (gap, offset) = if vertical {
                (
                    source.y.abs_diff(rect.y),
                    rect_center(rect).0.abs_diff(rect_center(source).0),
                )
            } else {
                (
                    source.x.abs_diff(rect.x),
                    rect_center(rect).1.abs_diff(rect_center(source).1),
                )
            };
            // `overlap == 0` first, and as a bool: `false` sorts before `true`,
            // so a pane genuinely beside this one is always preferred to one
            // that is only diagonally past it.
            Some((pane_id, (overlap == 0, u32::from(gap), offset, pane_id)))
        })
        .min_by_key(|(_, key)| *key)
        .map(|(pane_id, _)| pane_id)
}
pub(in crate::tui) fn fixed_grid_viewport(inner: Rect, rows: u16, cols: u16) -> Rect {
    let width = inner.width.min(cols);
    let height = inner.height.min(rows);
    Rect::new(inner.x, inner.y, width, height)
}
/// Keeps a local view's origin within the shared fixed grid.
pub(in crate::tui) fn clamp_grid_origin(
    origin: ScreenCell,
    grid: (u16, u16),
    view: (u16, u16),
) -> ScreenCell {
    ScreenCell {
        row: origin.row.min(grid.0.saturating_sub(view.0)),
        col: origin.col.min(grid.1.saturating_sub(view.1)),
    }
}

/// Follows the visible cursor with the smallest possible local camera move.
pub(in crate::tui) fn nudge_grid_origin(
    prior: ScreenCell,
    grid: (u16, u16),
    view: (u16, u16),
    cursor: ScreenCell,
    follow: bool,
) -> ScreenCell {
    let mut origin = clamp_grid_origin(prior, grid, view);
    if !follow || view.0 == 0 || view.1 == 0 {
        return origin;
    }
    if cursor.row < origin.row {
        origin.row = cursor.row;
    } else if cursor.row >= origin.row.saturating_add(view.0) {
        origin.row = cursor.row.saturating_add(1).saturating_sub(view.0);
    }
    if cursor.col < origin.col {
        origin.col = cursor.col;
    } else if cursor.col >= origin.col.saturating_add(view.1) {
        origin.col = cursor.col.saturating_add(1).saturating_sub(view.1);
    }
    clamp_grid_origin(origin, grid, view)
}
pub(in crate::tui) fn pane_content_rect(pane_rect: Rect) -> Rect {
    Block::bordered().inner(pane_rect)
}
/// Pins a pointer that wandered off the pane to the nearest cell inside it.
pub(in crate::tui) fn clamp_to_viewport(
    viewport: Rect,
    column: u16,
    row: u16,
) -> Option<ScreenCell> {
    if viewport.width == 0 || viewport.height == 0 {
        return None;
    }
    let last_column = viewport.x + viewport.width - 1;
    let last_row = viewport.y + viewport.height - 1;
    Some(ScreenCell {
        row: row.clamp(viewport.y, last_row) - viewport.y,
        col: column.clamp(viewport.x, last_column) - viewport.x,
    })
}
pub(in crate::tui) fn mouse_to_screen_cell(
    viewport: Rect,
    column: u16,
    row: u16,
) -> Option<ScreenCell> {
    rect_contains(viewport, column, row).then_some(ScreenCell {
        row: row.saturating_sub(viewport.y),
        col: column.saturating_sub(viewport.x),
    })
}
pub(in crate::tui) fn area_from_terminal_size(size: io::Result<(u16, u16)>) -> Option<Rect> {
    size.ok()
        .map(|(width, height)| Rect::new(0, 0, width, height))
}

/// How often a run loop re-reads the true terminal size.
const RESIZE_RECHECK_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) fn resize_recheck_due(last_checked: Option<Instant>, now: Instant) -> bool {
    last_checked.is_none_or(|checked| now.duration_since(checked) >= RESIZE_RECHECK_INTERVAL)
}

/// The size to adopt when the one the UI is drawing at has gone stale.
///
/// Resizes normally arrive as events, but one can go missing: attaching a
/// display resizes the window out from under us and the resize never reaches
/// the app, so the panes keep the old geometry until the window is resized by
/// hand. Re-reading the real size on a slow tick heals that without the event
/// path having to be perfect.
pub(crate) fn missed_resize(
    drawn_at: (u16, u16),
    actual: io::Result<(u16, u16)>,
) -> Option<(u16, u16)> {
    let (cols, rows) = actual.ok()?;
    (cols > 0 && rows > 0 && (cols, rows) != drawn_at).then_some((cols, rows))
}

/// The size a run loop should re-adopt, covering both ways the node falls behind.
///
/// [`missed_resize`] catches the window moving out from under the UI. It cannot catch
/// the other case: a resize that arrived, was drawn, and was never forwarded, because
/// `drawn_at` advanced all the same and the two sizes agree again. A modal open at that
/// moment swallows the message, and nothing else would ever send it — the panes keep
/// the old grid for the rest of the session while their borders draw at the new size.
///
/// Re-sending waits for the modal to close. Reflowing panes under an open dialog is
/// what the guard exists to prevent, and half a second later is soon enough.
pub(crate) fn stale_node_size(
    drawn_at: (u16, u16),
    node_told: (u16, u16),
    actual: io::Result<(u16, u16)>,
    modal_open: bool,
) -> Option<(u16, u16)> {
    missed_resize(drawn_at, actual).or((!modal_open && node_told != drawn_at).then_some(drawn_at))
}

#[derive(Clone, Copy, Debug)]
pub(in crate::tui) struct ResizeDrag {
    pub(in crate::tui) pane_id: PaneId,
    pub(in crate::tui) base_revision: u64,
    pub(in crate::tui) origin_column: u16,
    pub(in crate::tui) origin_row: u16,
    pub(in crate::tui) axis: Option<Axis>,
    pub(in crate::tui) horizontal: bool,
    pub(in crate::tui) vertical: bool,
    pub(in crate::tui) original_share_bps: u16,
    pub(in crate::tui) preview_first_share_bps: Option<u16>,
    pub(in crate::tui) span: u16,
    pub(in crate::tui) content: Rect,
}
#[derive(Clone, Copy, Debug)]
pub(in crate::tui) struct SplitTarget {
    pub(in crate::tui) first_share_bps: u16,
    pub(in crate::tui) span: u16,
}
#[derive(Clone, Copy, Debug)]
pub(in crate::tui) struct ResizePreview {
    pub(in crate::tui) pane_id: PaneId,
    pub(in crate::tui) axis: Axis,
    pub(in crate::tui) first_share_bps: u16,
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io};

    use ratatui::layout::Rect;

    use crate::{
        layout::{Axis, Node},
        tui::ScreenCell,
    };

    use crate::{
        layout::Tab,
        tui::{MultiPaneTui, test_support::layout},
    };

    use super::{
        RESIZE_RECHECK_INTERVAL, allocate_node_with_preview, area_from_terminal_size,
        clamp_grid_origin, grid_for_pane, initial_root_pane_grid, missed_resize,
        mouse_to_screen_cell, nearest_in_direction, nudge_grid_origin, resize_recheck_due,
        stale_node_size,
    };
    use crate::layout::PaneId;
    use crossterm::event::KeyCode;

    #[test]
    fn terminal_area_is_absent_when_terminal_size_is_unavailable() {
        assert_eq!(
            area_from_terminal_size(Err(io::Error::from(io::ErrorKind::WouldBlock))),
            None
        );
    }

    #[test]
    fn grid_origin_follows_only_far_enough_to_reveal_the_cursor() {
        assert_eq!(
            nudge_grid_origin(
                ScreenCell::default(),
                (40, 120),
                (10, 30),
                ScreenCell { row: 39, col: 119 },
                true,
            ),
            ScreenCell { row: 30, col: 90 },
        );
        assert_eq!(
            nudge_grid_origin(
                ScreenCell { row: 30, col: 90 },
                (40, 120),
                (10, 30),
                ScreenCell::default(),
                true,
            ),
            ScreenCell::default(),
        );
    }

    #[test]
    fn grid_origin_clamps_at_the_grid_edges() {
        assert_eq!(
            clamp_grid_origin(ScreenCell { row: 99, col: 99 }, (40, 120), (10, 30)),
            ScreenCell { row: 30, col: 90 },
        );
        assert_eq!(
            clamp_grid_origin(ScreenCell { row: 7, col: 8 }, (4, 5), (10, 30)),
            ScreenCell::default(),
        );
    }

    #[test]
    fn grid_origin_stays_put_when_following_is_unneeded_or_disabled() {
        let prior = ScreenCell { row: 5, col: 7 };
        assert_eq!(
            nudge_grid_origin(
                prior,
                (40, 120),
                (10, 30),
                ScreenCell { row: 8, col: 9 },
                true
            ),
            prior,
        );
        assert_eq!(
            nudge_grid_origin(
                prior,
                (40, 120),
                (10, 30),
                ScreenCell { row: 39, col: 119 },
                false,
            ),
            prior,
        );
    }

    #[test]
    fn a_window_that_grew_without_an_event_is_still_noticed() {
        assert_eq!(missed_resize((100, 30), Ok((160, 50))), Some((160, 50)));
    }

    #[test]
    fn a_size_that_still_matches_what_was_drawn_is_not_a_resize() {
        assert_eq!(missed_resize((100, 30), Ok((100, 30))), None);
    }

    #[test]
    fn an_unreadable_or_empty_size_never_resizes_the_ui() {
        assert_eq!(
            missed_resize((100, 30), Err(io::Error::from(io::ErrorKind::WouldBlock))),
            None
        );
        assert_eq!(missed_resize((100, 30), Ok((0, 50))), None);
        assert_eq!(missed_resize((100, 30), Ok((160, 0))), None);
    }

    /// The case `missed_resize` alone cannot see: the event arrived and was drawn, so
    /// the real size and the drawn size agree again, but a modal swallowed the message
    /// on its way to the node. Without the second comparison the panes keep the old
    /// grid for the rest of the session.
    #[test]
    fn a_resize_the_node_never_heard_is_re_sent_once_the_modal_closes() {
        assert_eq!(
            stale_node_size((160, 50), (100, 30), Ok((160, 50)), true),
            None,
            "reflowing panes under an open dialog is what the guard is for",
        );
        assert_eq!(
            stale_node_size((160, 50), (100, 30), Ok((160, 50)), false),
            Some((160, 50)),
        );
    }

    #[test]
    fn a_node_already_told_the_drawn_size_is_left_alone() {
        assert_eq!(
            stale_node_size((160, 50), (160, 50), Ok((160, 50)), false),
            None
        );
    }

    /// A window that moved out from under the UI still heals while a modal is open —
    /// that path keeps the UI drawing at the right size, and only the node message waits.
    #[test]
    fn a_window_that_grew_without_an_event_heals_even_under_a_modal() {
        assert_eq!(
            stale_node_size((100, 30), (100, 30), Ok((160, 50)), true),
            Some((160, 50)),
        );
    }

    #[test]
    fn the_size_recheck_runs_once_per_interval() {
        let start = std::time::Instant::now();
        assert!(resize_recheck_due(None, start));
        assert!(!resize_recheck_due(Some(start), start));
        assert!(!resize_recheck_due(
            Some(start),
            start + RESIZE_RECHECK_INTERVAL - std::time::Duration::from_millis(1)
        ));
        assert!(resize_recheck_due(
            Some(start),
            start + RESIZE_RECHECK_INTERVAL
        ));
    }

    #[test]
    fn ratio_allocation_respects_recursive_minima_and_small_areas() {
        let node = Node::Split {
            axis: Axis::LeftRight,
            first_share_bps: 7_500,
            first: Box::new(Node::Leaf { pane_id: 1 }),
            second: Box::new(Node::Split {
                axis: Axis::LeftRight,
                first_share_bps: 5_000,
                first: Box::new(Node::Leaf { pane_id: 2 }),
                second: Box::new(Node::Leaf { pane_id: 3 }),
            }),
        };
        let mut panes = BTreeMap::new();
        allocate_node_with_preview(&node, Rect::new(0, 0, 12, 9), &mut panes, None);
        assert_eq!(panes[&1].width, 6, "nested sibling needs six columns");
        assert_eq!(panes[&2].width, 3);
        assert_eq!(panes[&3].width, 3);

        panes.clear();
        allocate_node_with_preview(&node, Rect::new(0, 0, 2, 2), &mut panes, None);
        assert_eq!(panes[&1].width + panes[&2].width + panes[&3].width, 2);
    }

    #[test]
    fn mouse_coordinates_map_to_visible_screen_cells_only() {
        let viewport = Rect::new(10, 5, 3, 2);

        assert_eq!(
            mouse_to_screen_cell(viewport, 12, 6),
            Some(ScreenCell { row: 1, col: 2 })
        );
        assert_eq!(mouse_to_screen_cell(viewport, 9, 5), None);
        assert_eq!(mouse_to_screen_cell(viewport, 13, 6), None);
    }

    #[test]
    fn initial_root_pane_grid_matches_its_bordered_viewport() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        ))
        .expect("valid layout");

        let pane = tui
            .geometry(Rect::new(0, 0, 80, 24))
            .panes
            .get(&1)
            .copied()
            .expect("root pane");
        assert_eq!(grid_for_pane(pane), (20, 78));
        assert_eq!(initial_root_pane_grid(80, 24), (20, 78));
    }

    /// Issue #106, the half that is not about which key you press.
    ///
    /// ```text
    /// +--------------+----+
    /// |              | 2  |
    /// |      1       +----+
    /// |              | 3  |
    /// +--------------+----+
    /// ```
    #[test]
    fn an_arrow_never_leaves_sideways_when_nothing_is_that_way() {
        let one = Rect::new(0, 0, 40, 20);
        let two = Rect::new(40, 0, 20, 10);
        let three = Rect::new(40, 10, 20, 10);
        let from_one = [(2, two), (3, three)];

        // Comparing centres, pane 2's is higher than pane 1's, so Up used to
        // move focus up *and to the right*. Nothing is above pane 1.
        assert_eq!(nearest_in_direction(one, from_one, KeyCode::Up), None);
        assert_eq!(nearest_in_direction(one, from_one, KeyCode::Down), None);
        assert_eq!(nearest_in_direction(one, from_one, KeyCode::Left), None);
        assert_eq!(nearest_in_direction(one, from_one, KeyCode::Right), Some(2));

        // And the right-hand column navigates the way it looks.
        assert_eq!(
            nearest_in_direction(two, [(1, one), (3, three)], KeyCode::Down),
            Some(3)
        );
        assert_eq!(
            nearest_in_direction(three, [(1, one), (2, two)], KeyCode::Up),
            Some(2)
        );
        assert_eq!(
            nearest_in_direction(three, [(1, one), (2, two)], KeyCode::Left),
            Some(1)
        );
    }

    /// A pane that is beside the source beats one that is only past a corner,
    /// which is what "the arrows cannot even get to the desired pane" meant.
    ///
    /// ```text
    /// +----+----+
    /// | 1  | 2  |
    /// +----+----+
    /// | 3  | 4  |
    /// +----+----+
    /// ```
    #[test]
    fn a_neighbour_beats_a_diagonal_and_a_grid_walks_the_way_it_reads() {
        let panes = [
            (1, Rect::new(0, 0, 20, 10)),
            (2, Rect::new(20, 0, 20, 10)),
            (3, Rect::new(0, 10, 20, 10)),
            (4, Rect::new(20, 10, 20, 10)),
        ];
        let without = |id: PaneId| {
            panes
                .iter()
                .copied()
                .filter(move |(pane_id, _)| *pane_id != id)
                .collect::<Vec<_>>()
        };
        let rect_of = |id: PaneId| panes.iter().find(|(p, _)| *p == id).unwrap().1;

        for (from, direction, expected) in [
            (1, KeyCode::Right, Some(2)),
            (1, KeyCode::Down, Some(3)),
            (1, KeyCode::Up, None),
            (1, KeyCode::Left, None),
            (2, KeyCode::Left, Some(1)),
            (2, KeyCode::Down, Some(4)),
            (3, KeyCode::Up, Some(1)),
            (3, KeyCode::Right, Some(4)),
            (4, KeyCode::Up, Some(2)),
            (4, KeyCode::Left, Some(3)),
            (4, KeyCode::Down, None),
            (4, KeyCode::Right, None),
        ] {
            assert_eq!(
                nearest_in_direction(rect_of(from), without(from), direction),
                expected,
                "from pane {from} going {direction:?}"
            );
        }
    }

    /// Nothing overlaps, so the fallback has to pick *something* rather than
    /// stranding the user -- but only among panes genuinely that way.
    #[test]
    fn a_pane_past_a_corner_is_still_reachable_when_nothing_is_beside_you() {
        // A narrow strip at the top left, and a wide one below and to the right
        // that its columns do not touch.
        let source = Rect::new(0, 0, 10, 5);
        let away = Rect::new(30, 10, 20, 5);

        assert_eq!(
            nearest_in_direction(source, [(2, away)], KeyCode::Down),
            Some(2),
            "with no neighbour below, the one past the corner is better than nowhere"
        );
        assert_eq!(nearest_in_direction(source, [(2, away)], KeyCode::Up), None);
    }
}
