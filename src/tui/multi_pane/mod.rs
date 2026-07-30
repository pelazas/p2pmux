//! `MultiPaneTui`: the local view of a shared layout, and everything a
//! keystroke or click does to it.
//!
//! The inherent impl is split by concern across this module's files.

mod agents;
pub(in crate::tui) mod keys;
mod mouse;
mod scrollback;

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use ratatui::layout::Rect;

use crate::{
    config::UiTheme,
    layout::{LayoutError, LayoutSnapshot, PaneId, TabId},
    protocol::AgentRosterState,
    tui::{
        AgentOverlayRow, ChordMode, ModalState, PaneGeometry, PaneTextSelection, PaneViewState,
        ShareCopy,
        debug_log::ui_debug_log,
        geometry::{
            ResizeDrag, ResizePreview, allocate_node_with_preview, contains_leaf, first_leaf,
            fixed_grid_viewport, pane_content_rect, visible_leaf_panes,
        },
        render::panes::{TAB_BAR_SEPARATOR, TOP_BAR_BRAND, TOP_BAR_BRAND_SEPARATOR, tab_label},
        text::text_width,
    },
};

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

    pub fn pane_view(&self, pane_id: PaneId) -> Option<&PaneViewState> {
        self.pane_views.get(&pane_id)
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

    /// Mirror the coordinator's current session lock, for display and for the toggle.
    pub fn set_session_locked(&mut self, locked: bool) {
        self.session_locked = locked;
    }

    pub fn session_locked(&self) -> bool {
        self.session_locked
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
