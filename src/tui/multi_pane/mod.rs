//! `MultiPaneTui`: the local view of a shared layout, and everything a
//! keystroke or click does to it.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use crate::{
    config::UiTheme,
    layout::{Axis, LayoutError, LayoutSnapshot, NewPanePosition, PaneId, TabId, normalize_title},
    protocol::AgentRosterState,
    tui::{
        AGENT_TOGGLE_WINDOW, AgentOverlayRow, ChordMode, KeyHandling, ModalState, MouseHandling,
        PaneGeometry, PaneMouseProtocol, PaneTextSelection, PaneViewState, RenamePrompt,
        RenameTarget, ScreenCell, ShareCopy, UiIntent,
        debug_log::ui_debug_log,
        geometry::{
            ResizeDrag, ResizePreview, allocate_node_with_preview, clamp_to_viewport,
            contains_leaf, direction_distance, first_leaf, fixed_grid_viewport, grid_for_pane,
            is_in_direction, mouse_to_screen_cell, nearest_split_for_pane, pane_at,
            pane_content_rect, rect_center, rect_contains, resize_border_hit,
            resize_proposed_share, visible_leaf_panes,
        },
        input::{
            keys::{is_chord_command, is_chord_navigation, is_option_arrow, is_quit},
            mouse::encode_mouse,
        },
        render::{
            agents::{AGENT_OVERLAY_CARD_LINES, agents_overlay_content},
            panes::{TAB_BAR_SEPARATOR, TOP_BAR_BRAND, TOP_BAR_BRAND_SEPARATOR, tab_label},
        },
        selection::selection_text,
        text::text_width,
    },
};

pub(in crate::tui) const CHORD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const PANE_SCROLL_WHEEL_STEP: usize = 3;
/// Pure local rendering and selection state for a revisioned shared layout.
#[derive(Clone, Debug)]
pub struct MultiPaneTui {
    pub(in crate::tui) title: String,
    pub(in crate::tui) theme: UiTheme,
    pub(in crate::tui) snapshot: LayoutSnapshot,
    pub(in crate::tui) current_tab: TabId,
    pub(in crate::tui) focused_pane: PaneId,
    pub(in crate::tui) hovered_pane: Option<PaneId>,
    pub(in crate::tui) chord_mode: ChordMode,
    pub(in crate::tui) chord_last_activity: Option<Instant>,
    pub(in crate::tui) pane_views: BTreeMap<PaneId, PaneViewState>,
    pub(in crate::tui) pending_created_tab: Option<TabId>,
    pub(in crate::tui) pending_created_pane: Option<PaneId>,
    pub(in crate::tui) selection: Option<PaneTextSelection>,
    pub(in crate::tui) selection_dragging: bool,
    pub(in crate::tui) agent_rows: Vec<AgentOverlayRow>,
    pub(in crate::tui) prior_agent_states: BTreeMap<PaneId, AgentRosterState>,
    /// Start of the working interval last seen for a pane. An idle row carries the `0`
    /// sentinel, so the episode a completion refers to has to be remembered while it runs.
    pub(in crate::tui) prior_agent_episodes: BTreeMap<PaneId, u64>,
    /// Working interval a pane was last announced for. Keyed separately from
    /// `unread_agent_panes` because that set is cleared by focusing the pane.
    pub(in crate::tui) notified_agent_episodes: BTreeMap<PaneId, u64>,
    pub(in crate::tui) unread_agent_panes: BTreeSet<PaneId>,
    pub(in crate::tui) modal: ModalState,
    pub(in crate::tui) agent_selected_pane: Option<PaneId>,
    /// Terminal-line offset into the cards (two card lines plus one spacer each).
    pub(in crate::tui) agent_overlay_scroll_line: usize,
    pub(in crate::tui) agent_overlay_viewport_lines: u16,
    pub(in crate::tui) pending_agent_toggle: Option<Instant>,
    pub(in crate::tui) resize_drag: Option<ResizeDrag>,
    /// Set while a press forwarded to a child owns the drag and release that follow.
    pub(in crate::tui) mouse_forwarding: bool,
    /// A share-modal copy request. Invite material lives in the node's rendezvous record
    /// rather than in the layout, so the attached client resolves and copies it.
    pub(in crate::tui) pending_share_copy: Option<ShareCopy>,
    /// Whether the coordinator is refusing new peers. Mirrored from the node rather than
    /// derived, because only the coordinator knows it and any peer may be drawing it.
    pub(in crate::tui) session_locked: bool,
}
impl MultiPaneTui {
    pub fn new(snapshot: LayoutSnapshot) -> Result<Self, LayoutError> {
        Self::with_theme(snapshot, UiTheme::default())
    }

    pub fn with_theme(snapshot: LayoutSnapshot, theme: UiTheme) -> Result<Self, LayoutError> {
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let current_tab = snapshot.tabs[0].tab_id;
        let focused_pane = first_leaf(&snapshot.tabs[0].root).expect("validated layout has a leaf");
        let pane_views = snapshot
            .panes
            .keys()
            .map(|pane_id| (*pane_id, PaneViewState::default()))
            .collect();
        Ok(Self {
            title: TOP_BAR_BRAND.into(),
            theme,
            snapshot,
            current_tab,
            focused_pane,
            hovered_pane: None,
            chord_mode: ChordMode::None,
            chord_last_activity: None,
            pane_views,
            pending_created_tab: None,
            pending_created_pane: None,
            selection: None,
            selection_dragging: false,
            agent_rows: Vec::new(),
            prior_agent_states: BTreeMap::new(),
            prior_agent_episodes: BTreeMap::new(),
            notified_agent_episodes: BTreeMap::new(),
            unread_agent_panes: BTreeSet::new(),
            modal: ModalState::None,
            agent_selected_pane: None,
            agent_overlay_scroll_line: 0,
            agent_overlay_viewport_lines: 0,
            pending_agent_toggle: None,
            resize_drag: None,
            session_locked: false,
            mouse_forwarding: false,
            pending_share_copy: None,
        })
    }

    pub fn snapshot(&self) -> &LayoutSnapshot {
        &self.snapshot
    }

    /// Local clients may decorate the shared chrome without changing layout state.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn current_tab(&self) -> TabId {
        self.current_tab
    }

    pub fn focused_pane(&self) -> PaneId {
        self.focused_pane
    }

    pub fn chord_mode(&self) -> ChordMode {
        self.chord_mode
    }

    pub fn overlay_open(&self) -> bool {
        matches!(self.modal, ModalState::Agents)
    }

    /// Whether a blocking dialog is open, excluding the interactive agents overlay.
    pub fn modal_open(&self) -> bool {
        matches!(
            self.modal,
            ModalState::Rename(_) | ModalState::ConfirmDeleteTab { .. } | ModalState::Share
        )
    }

    pub fn share_open(&self) -> bool {
        matches!(self.modal, ModalState::Share)
    }

    /// Takes the clipboard write the share modal asked for, if any.
    ///
    /// The mux never touches the clipboard itself — the attaching process owns it, exactly as
    /// it already does for selection copies.
    pub fn take_share_copy_request(&mut self) -> Option<ShareCopy> {
        self.pending_share_copy.take()
    }

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

    pub(in crate::tui) fn pane_location(&self, pane_id: PaneId) -> Option<(usize, usize)> {
        self.snapshot
            .tabs
            .iter()
            .enumerate()
            .find_map(|(tab_index, tab)| {
                visible_leaf_panes(&tab.root)
                    .iter()
                    .position(|id| *id == pane_id)
                    .map(|pane_index| (tab_index + 1, pane_index + 1))
            })
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

    pub(in crate::tui) fn exit_chord_mode(&mut self) {
        self.chord_mode = ChordMode::None;
        self.chord_last_activity = None;
    }

    pub(in crate::tui) fn enter_chord_mode(&mut self, mode: ChordMode) {
        self.chord_mode = mode;
        self.chord_last_activity = Some(Instant::now());
    }

    pub(in crate::tui) fn touch_chord_activity(&mut self) {
        if self.chord_mode != ChordMode::None {
            self.chord_last_activity = Some(Instant::now());
        }
    }

    /// Clears sticky pane/tab mode after [`CHORD_IDLE_TIMEOUT`] without a key.
    pub(crate) fn expire_chord_mode(&mut self, now: Instant) -> bool {
        let Some(last) = self.chord_last_activity else {
            return false;
        };
        if self.chord_mode == ChordMode::None
            || now.saturating_duration_since(last) < CHORD_IDLE_TIMEOUT
        {
            return false;
        }
        self.exit_chord_mode();
        true
    }

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

    pub fn pane_view(&self, pane_id: PaneId) -> Option<&PaneViewState> {
        self.pane_views.get(&pane_id)
    }

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

    pub(in crate::tui) fn pane_at_or_focused(&self, column: u16, row: u16, area: Rect) -> PaneId {
        pane_at(&self.geometry(area).panes, column, row).unwrap_or(self.focused_pane)
    }

    pub fn pane_at_or_focused_for_mouse(&self, column: u16, row: u16, area: Rect) -> PaneId {
        self.pane_at_or_focused(column, row, area)
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

    pub fn set_pane_view(&mut self, pane_id: PaneId, mut state: PaneViewState) -> bool {
        if self.snapshot.panes.contains_key(&pane_id) {
            state.scrollback = self.scrollback_offset(pane_id);
            if self.pane_views.get(&pane_id) == Some(&state) {
                return false;
            }
            self.pane_views.insert(pane_id, state);
            return true;
        }
        false
    }

    /// Select a tab only when the coordinator publishes the exact ID reserved for this member.
    pub fn select_created_tab(&mut self, tab_id: TabId) {
        self.pending_created_tab = Some(tab_id);
        self.repair_selection();
    }

    /// Select a pane only once an authoritative snapshot includes the reservation's pane ID.
    pub fn select_created_pane(&mut self, pane_id: PaneId) {
        self.pending_created_pane = Some(pane_id);
        self.repair_selection();
    }

    pub fn apply_snapshot(&mut self, snapshot: LayoutSnapshot) -> Result<(), LayoutError> {
        if snapshot.revision < self.snapshot.revision {
            return Err(LayoutError::StaleRevision {
                expected: self.snapshot.revision,
                got: snapshot.revision,
            });
        }
        if snapshot.revision == self.snapshot.revision {
            return if snapshot == self.snapshot {
                Ok(())
            } else {
                Err(LayoutError::ConflictingSnapshotRevision {
                    revision: snapshot.revision,
                })
            };
        }
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let old_views = std::mem::take(&mut self.pane_views);
        self.pane_views = snapshot
            .panes
            .values()
            .map(|pane| {
                let state = old_views.get(&pane.pane_id).cloned().unwrap_or_default();
                (pane.pane_id, state)
            })
            .collect();
        self.snapshot = snapshot;
        self.prior_agent_states
            .retain(|pane_id, _| self.snapshot.panes.contains_key(pane_id));
        self.prior_agent_episodes
            .retain(|pane_id, _| self.snapshot.panes.contains_key(pane_id));
        self.notified_agent_episodes
            .retain(|pane_id, _| self.snapshot.panes.contains_key(pane_id));
        self.unread_agent_panes
            .retain(|pane_id| self.snapshot.panes.contains_key(pane_id));
        self.cancel_resize_drag();
        self.clear_selection();
        self.repair_selection();
        Ok(())
    }

    pub fn select_tab(&mut self, tab_id: TabId) -> Result<(), LayoutError> {
        let tab = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        let pane_id = first_leaf(&tab.root).expect("validated layout has a leaf");
        self.select_pane(tab_id, pane_id, "tab_switch");
        Ok(())
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

    pub fn geometry(&self, area: Rect) -> PaneGeometry {
        let tab_bar = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let footer_height = area.height.saturating_sub(tab_bar.height).min(1);
        let footer = Rect::new(
            area.x,
            area.y
                .saturating_add(area.height.saturating_sub(footer_height)),
            area.width,
            footer_height,
        );
        let tab_bar_gap = 0;
        let content = Rect::new(
            area.x,
            area.y.saturating_add(tab_bar.height + tab_bar_gap),
            area.width,
            area.height
                .saturating_sub(tab_bar.height + tab_bar_gap + footer_height),
        );
        let mut panes = BTreeMap::new();
        if let Some(tab) = self.current_tab_layout() {
            allocate_node_with_preview(
                &tab.root,
                content,
                &mut panes,
                self.resize_drag.and_then(|drag| {
                    drag.axis
                        .zip(drag.preview_first_share_bps)
                        .map(|(axis, first_share_bps)| ResizePreview {
                            pane_id: drag.pane_id,
                            axis,
                            first_share_bps,
                        })
                }),
            );
        }
        PaneGeometry {
            tab_bar,
            tab_labels: self.tab_label_rects(tab_bar),
            content,
            footer,
            panes,
        }
    }

    pub(in crate::tui) fn tab_label_rects(&self, tab_bar: Rect) -> BTreeMap<TabId, Rect> {
        let mut x = tab_bar
            .x
            .saturating_add(text_width(&self.title))
            .saturating_add(text_width(TOP_BAR_BRAND_SEPARATOR));
        let right = tab_bar.x.saturating_add(tab_bar.width);
        self.snapshot
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                if index > 0 {
                    x = x.saturating_add(text_width(TAB_BAR_SEPARATOR));
                }
                let label_width = text_width(&tab_label(
                    tab.title.as_deref(),
                    index + 1,
                    tab.tab_id == self.current_tab,
                    self.tab_has_unread_agent_pane(tab),
                ));
                let width = right.saturating_sub(x).min(label_width);
                let rect = Rect::new(x, tab_bar.y, width, tab_bar.height);
                x = x.saturating_add(label_width);
                (tab.tab_id, rect)
            })
            .collect()
    }

    pub(in crate::tui) fn tab_has_unread_agent_pane(&self, tab: &crate::layout::Tab) -> bool {
        self.unread_agent_panes
            .iter()
            .any(|pane_id| contains_leaf(&tab.root, *pane_id))
    }

    pub fn handle_key(&mut self, key: KeyEvent, area: Rect) -> KeyHandling {
        if is_quit(key) {
            self.modal = ModalState::None;
            self.pending_agent_toggle = None;
            self.exit_chord_mode();
            return KeyHandling::Quit;
        }
        if matches!(self.modal, ModalState::Rename(_)) {
            return self.handle_rename_key(key);
        }
        if matches!(self.modal, ModalState::ConfirmDeleteTab { .. }) {
            return self.handle_confirm_delete_tab_key(key);
        }
        // Ctrl+S is claimed before the pane sees it, so a pane never receives XOFF from this
        // binding — the same trade already made for Ctrl+A.
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            self.modal = if self.share_open() {
                ModalState::None
            } else {
                ModalState::Share
            };
            self.exit_chord_mode();
            return KeyHandling::Consumed(vec![]);
        }
        if self.share_open() {
            return self.handle_share_key(key);
        }
        if key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL {
            if self.overlay_open() {
                let forward = self
                    .pending_agent_toggle
                    .is_some_and(|then| then.elapsed() <= AGENT_TOGGLE_WINDOW);
                self.modal = ModalState::None;
                self.pending_agent_toggle = None;
                return if forward {
                    ui_debug_log(
                        "agents_toggle_forward",
                        format_args!("window_ms={}", AGENT_TOGGLE_WINDOW.as_millis()),
                    );
                    KeyHandling::Forward
                } else {
                    ui_debug_log("agents_overlay_close", format_args!("reason=ctrl_a"));
                    KeyHandling::Consumed(vec![])
                };
            }
            self.modal = ModalState::Agents;
            self.pending_agent_toggle = Some(Instant::now());
            self.exit_chord_mode();
            ui_debug_log(
                "agents_overlay_open",
                format_args!("window_ms={}", AGENT_TOGGLE_WINDOW.as_millis()),
            );
            return KeyHandling::Consumed(vec![]);
        }
        if self.overlay_open() {
            self.set_agent_overlay_viewport(area);
            return self.handle_agent_overlay_key(key);
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.chord_mode != ChordMode::None
        {
            self.exit_chord_mode();
            return KeyHandling::Consumed(vec![]);
        }
        if matches!(self.chord_mode, ChordMode::None | ChordMode::Pane) && is_option_arrow(key) {
            self.touch_chord_activity();
            return KeyHandling::Consumed(self.move_focus(key.code, area).into_iter().collect());
        }
        if self.chord_mode == ChordMode::None {
            if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
                self.enter_chord_mode(ChordMode::Pane);
                return KeyHandling::Consumed(vec![]);
            }
            if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
                self.enter_chord_mode(ChordMode::Tab);
                return KeyHandling::Consumed(vec![]);
            }
            return KeyHandling::Forward;
        }

        let chord = self.chord_mode;
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.enter_chord_mode(ChordMode::Pane);
            return KeyHandling::Consumed(vec![]);
        }
        if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            self.enter_chord_mode(ChordMode::Tab);
            return KeyHandling::Consumed(vec![]);
        }
        let intent = match chord {
            ChordMode::Pane => self.handle_pane_chord(key, area),
            ChordMode::Tab => self.handle_tab_chord(key, area),
            ChordMode::None => None,
        };
        if is_chord_command(chord, key) {
            if is_chord_navigation(chord, key) {
                self.touch_chord_activity();
            } else {
                self.exit_chord_mode();
            }
            KeyHandling::Consumed(intent.into_iter().collect())
        } else {
            self.exit_chord_mode();
            KeyHandling::Forward
        }
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

    pub(in crate::tui) fn focused_pane_viewport(&self, area: Rect) -> Option<Rect> {
        let geometry = self.geometry(area);
        let pane_rect = geometry.panes.get(&self.focused_pane)?;
        let pane = self.snapshot.panes.get(&self.focused_pane)?;
        Some(fixed_grid_viewport(
            pane_content_rect(*pane_rect),
            pane.grid_rows,
            pane.grid_cols,
        ))
    }

    /// Aligns a reattached client's local selection with the node-owned focus.
    pub fn set_focus(&mut self, tab_id: TabId, pane_id: PaneId) -> Result<(), LayoutError> {
        let tab = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        if !contains_leaf(&tab.root, pane_id) {
            return Err(LayoutError::UnknownPane { pane_id });
        }
        self.select_pane(tab_id, pane_id, "node_sync");
        Ok(())
    }

    pub(in crate::tui) fn handle_agent_overlay_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Esc => {
                self.modal = ModalState::None;
                self.pending_agent_toggle = None;
                ui_debug_log("agents_overlay_close", format_args!("reason=esc"));
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_agent_selection(false);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_agent_selection(true);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter => {
                let Some(pane_id) = self.agent_selected_pane else {
                    return KeyHandling::Consumed(vec![]);
                };
                KeyHandling::Consumed(self.jump_to_agent_pane(pane_id, "enter"))
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    pub(in crate::tui) fn open_rename(&mut self, target: RenameTarget) {
        let value = match target {
            RenameTarget::Pane(pane_id) => self
                .snapshot
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.title.clone()),
            RenameTarget::Tab(tab_id) => self
                .snapshot
                .tabs
                .iter()
                .find(|tab| tab.tab_id == tab_id)
                .and_then(|tab| tab.title.clone()),
        }
        .unwrap_or_default();
        self.modal = ModalState::Rename(RenamePrompt {
            target,
            value,
            error: None,
        });
        self.exit_chord_mode();
        self.clear_selection();
        self.cancel_resize_drag();
    }

    pub(in crate::tui) fn handle_rename_key(&mut self, key: KeyEvent) -> KeyHandling {
        let ModalState::Rename(prompt) = &mut self.modal else {
            unreachable!("rename handler requires an active rename prompt");
        };
        match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                prompt.value.pop();
                prompt.error = None;
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter if key.modifiers.is_empty() => match normalize_title(&prompt.value) {
                Ok(normalized) => {
                    let title = normalized.unwrap_or_default();
                    let intent = match prompt.target {
                        RenameTarget::Pane(pane_id) => UiIntent::RenamePane { pane_id, title },
                        RenameTarget::Tab(tab_id) => UiIntent::RenameTab { tab_id, title },
                    };
                    self.modal = ModalState::None;
                    KeyHandling::Consumed(vec![intent])
                }
                Err(_) => {
                    prompt.error = Some(String::from("Max 32 characters; no controls"));
                    KeyHandling::Consumed(vec![])
                }
            },
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                prompt.value.push(character);
                prompt.error = None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    /// Keys for the share modal.
    ///
    /// Enter takes the ticket because that is the only thing a peer on another machine can
    /// use; the code is the secondary key precisely because it only resolves on this Mac.
    /// Every key is consumed so no invite material leaks into the focused pane.
    pub(in crate::tui) fn handle_share_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Ticket);
            }
            KeyCode::Char('c') if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Code);
            }
            KeyCode::Esc if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
            }
            _ => {}
        }
        KeyHandling::Consumed(vec![])
    }

    pub(in crate::tui) fn confirm_delete_tab(&mut self, tab_id: TabId, pane_count: usize) {
        self.modal = ModalState::ConfirmDeleteTab { tab_id, pane_count };
        self.exit_chord_mode();
        self.clear_selection();
        self.cancel_resize_drag();
    }

    pub(in crate::tui) fn handle_confirm_delete_tab_key(&mut self, key: KeyEvent) -> KeyHandling {
        let tab_id = match &self.modal {
            ModalState::ConfirmDeleteTab { tab_id, .. } => *tab_id,
            _ => unreachable!("delete confirmation handler requires an active confirmation"),
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id }])
            }
            KeyCode::Esc | KeyCode::Char('n') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
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

    pub(in crate::tui) fn handle_pane_chord(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_rename(RenameTarget::Pane(self.focused_pane));
                None
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
                self.create_pane(
                    if rect.width > rect.height {
                        Axis::LeftRight
                    } else {
                        Axis::TopBottom
                    },
                    NewPanePosition::Second,
                    area,
                )
            }
            KeyCode::Char('r') if key.modifiers.is_empty() => {
                self.create_pane(Axis::LeftRight, NewPanePosition::Second, area)
            }
            KeyCode::Char('l') if key.modifiers.is_empty() => {
                self.create_pane(Axis::LeftRight, NewPanePosition::First, area)
            }
            KeyCode::Char('d') if key.modifiers.is_empty() => {
                self.create_pane(Axis::TopBottom, NewPanePosition::Second, area)
            }
            KeyCode::Char('u') if key.modifiers.is_empty() => {
                self.create_pane(Axis::TopBottom, NewPanePosition::First, area)
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => Some(UiIntent::DeletePane {
                pane_id: self.focused_pane,
            }),
            KeyCode::Char('k') if key.modifiers.is_empty() => self
                .snapshot
                .panes
                .get(&self.focused_pane)
                .map(|pane| UiIntent::SetPaneLock {
                    pane_id: self.focused_pane,
                    locked: !pane.locked,
                }),
            // Shift+L, deliberately a different key from the lowercase `k` pane lock:
            // locking one pane and locking the front door are very different acts to
            // perform by accident.
            KeyCode::Char('L') => Some(UiIntent::SetSessionLock {
                locked: !self.session_locked,
            }),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.is_empty() =>
            {
                self.move_focus(key.code, area)
            }
            _ => None,
        }
    }

    /// Mirror the coordinator's current session lock, for display and for the toggle.
    pub fn set_session_locked(&mut self, locked: bool) {
        self.session_locked = locked;
    }

    pub fn session_locked(&self) -> bool {
        self.session_locked
    }

    pub(in crate::tui) fn create_pane(
        &self,
        axis: Axis,
        position: NewPanePosition,
        area: Rect,
    ) -> Option<UiIntent> {
        let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
        let (grid_rows, grid_cols) = grid_for_pane(rect);
        Some(UiIntent::CreatePane {
            target_pane_id: self.focused_pane,
            axis,
            position,
            grid_rows,
            grid_cols,
        })
    }

    pub(in crate::tui) fn handle_tab_chord(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_rename(RenameTarget::Tab(self.current_tab));
                None
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let (grid_rows, grid_cols) = grid_for_pane(self.geometry(area).content);
                Some(UiIntent::CreateTab {
                    grid_rows,
                    grid_cols,
                })
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => {
                let tab = self
                    .snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == self.current_tab)?;
                let pane_count = visible_leaf_panes(&tab.root).len();
                if pane_count > 1 {
                    self.confirm_delete_tab(tab.tab_id, pane_count);
                    None
                } else {
                    Some(UiIntent::DeleteTab { tab_id: tab.tab_id })
                }
            }
            KeyCode::Left if key.modifiers.is_empty() => self.switch_tab(false),
            KeyCode::Right if key.modifiers.is_empty() => self.switch_tab(true),
            _ => None,
        }
    }

    pub(in crate::tui) fn move_focus(
        &mut self,
        direction: KeyCode,
        area: Rect,
    ) -> Option<UiIntent> {
        let geometry = self.geometry(area);
        let source = *geometry.panes.get(&self.focused_pane)?;
        let source_center = rect_center(source);
        let candidates = geometry
            .panes
            .iter()
            .filter(|(pane_id, _)| **pane_id != self.focused_pane)
            .map(|(pane_id, rect)| (*pane_id, *rect))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let pane_id = candidates
            .iter()
            .copied()
            .filter(|(_, rect)| is_in_direction(source_center, rect_center(*rect), direction))
            .min_by_key(|(pane_id, rect)| {
                direction_distance(source_center, rect_center(*rect), direction, *pane_id)
            })?
            .0;
        self.select_pane(self.current_tab, pane_id, "key");
        Some(UiIntent::FocusPane { pane_id })
    }

    pub(in crate::tui) fn switch_tab(&mut self, forward: bool) -> Option<UiIntent> {
        let index = self
            .snapshot
            .tabs
            .iter()
            .position(|tab| tab.tab_id == self.current_tab)?;
        let len = self.snapshot.tabs.len();
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        let tab_id = self.snapshot.tabs[next].tab_id;
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(UiIntent::SwitchTab { tab_id })
    }

    pub(in crate::tui) fn current_tab_layout(&self) -> Option<&crate::layout::Tab> {
        self.snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == self.current_tab)
    }

    pub(in crate::tui) fn repair_selection(&mut self) {
        if let Some(tab_id) = self.pending_created_tab
            && let Some(tab) = self.snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id)
        {
            let pane_id = first_leaf(&tab.root).expect("validated layout has a leaf");
            self.pending_created_tab = None;
            self.select_pane(tab_id, pane_id, "snapshot_repair");
            return;
        }
        let current_tab = if let Some(tab) = self.current_tab_layout() {
            tab
        } else {
            &self.snapshot.tabs[0]
        };
        let (tab_id, pane_id) = {
            let root = &current_tab.root;
            let pane_id = self
                .pending_created_pane
                .filter(|pane_id| contains_leaf(root, *pane_id))
                .or_else(|| contains_leaf(root, self.focused_pane).then_some(self.focused_pane))
                .unwrap_or_else(|| first_leaf(root).expect("validated layout has a leaf"));
            (current_tab.tab_id, pane_id)
        };
        if self.pending_created_pane == Some(pane_id) {
            self.pending_created_pane = None;
        }
        self.select_pane(tab_id, pane_id, "snapshot_repair");
    }

    pub(in crate::tui) fn select_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        reason: &str,
    ) -> bool {
        let old_tab = self.current_tab;
        let old_pane = self.focused_pane;
        self.current_tab = tab_id;
        self.focused_pane = pane_id;
        let unread_cleared = self.unread_agent_panes.remove(&pane_id);
        self.log_selection_change(reason, old_tab, old_pane, tab_id, pane_id);
        old_tab != tab_id || old_pane != pane_id || unread_cleared
    }

    pub(in crate::tui) fn log_selection_change(
        &self,
        reason: &str,
        old_tab: TabId,
        old_pane: PaneId,
        new_tab: TabId,
        new_pane: PaneId,
    ) {
        if old_tab != new_tab || old_pane != new_pane {
            ui_debug_log(
                "selection_change",
                format_args!(
                    "reason={reason} old_tab={old_tab} old_pane={old_pane} new_tab={new_tab} new_pane={new_pane}"
                ),
            );
        }
    }
}
