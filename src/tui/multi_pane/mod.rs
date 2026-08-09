//! `MultiPaneTui`: the local view of a shared layout, and everything a
//! keystroke or click does to it.
//!
//! The inherent impl is split by concern across this module's files.

mod agents;
pub(in crate::tui) mod keys;
mod modal;
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
        AgentOverlayRow, ChordMode, ModalState, PairedMachine, PaneGeometry, PaneTextSelection,
        PaneViewState, ShareCopy,
        debug_log::ui_debug_log,
        geometry::{
            ResizeDrag, ResizePreview, allocate_node_with_preview, contains_leaf, first_leaf,
            fixed_grid_viewport, pane_content_rect, visible_leaf_panes,
        },
        render::panes::{
            TAB_BAR_SEPARATOR, TOP_BAR_BRAND, TOP_BAR_BRAND_SEPARATOR, TOP_BAR_TITLE_MAX_WIDTH,
            inbox_segment, inbox_segment_width, tab_label, tab_presence_width,
        },
        text::{text_width, truncate_trailing},
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
    pub(in crate::tui) presence: Vec<crate::local_ipc::PresenceRow>,
    pub(in crate::tui) prior_agent_states: BTreeMap<PaneId, AgentRosterState>,
    /// Start of the working interval last seen for a pane. An idle row carries the `0`
    /// sentinel, so the episode a completion refers to has to be remembered while it runs.
    pub(in crate::tui) prior_agent_episodes: BTreeMap<PaneId, u64>,
    /// Working interval a pane was last announced for. Keyed separately from
    /// `unread_agent_panes` because that set is cleared by focusing the pane.
    pub(in crate::tui) notified_agent_episodes: BTreeMap<PaneId, u64>,
    pub(in crate::tui) unread_agent_panes: BTreeSet<PaneId>,
    pub(in crate::tui) modal: ModalState,
    /// A first Ctrl+A, waiting to see whether a second follows. See
    /// [`crate::tui::HOME_TOGGLE_WINDOW`].
    pub(in crate::tui) pending_home_toggle: Option<Instant>,
    /// Whether Home — the inbox — is the screen on display.
    ///
    /// Local to this client and never replicated: see [`crate::tui::home`].
    pub(in crate::tui) home_open: bool,
    pub(in crate::tui) home_selected: Option<crate::tui::HomeRowId>,
    /// Which machine the cursor is on, when it is on the fleet rather than on
    /// the agents.
    ///
    /// The fleet used to be a read-only strip. It is a picker now because
    /// "open a terminal on that machine" needs somewhere to say *which*, and
    /// the list of machines was already on screen. `None` means the cursor is
    /// back on the agents, which is where it starts and where `Esc` returns it.
    pub(in crate::tui) home_machine: Option<usize>,
    /// One line Home has to say about the last thing that was asked of it.
    ///
    /// Home's own rather than the window footer's, because the answers it gives
    /// are about the screen you are still looking at — which machine is asleep,
    /// which one is not yours — and the reader has not gone anywhere.
    pub(in crate::tui) home_notice: Option<String>,
    /// The area Home was last measured against.
    ///
    /// Kept so that a key handler which decides to open a terminal can size it
    /// without being handed the screen again through three call sites that have
    /// no other use for it.
    pub(in crate::tui) last_home_area: Rect,
    /// Which page of the agent list is drawn. Derived from the selection
    /// everywhere except the wheel, which moves both.
    pub(in crate::tui) home_page: usize,
    /// How many agents that page holds, from the last layout that was measured.
    pub(in crate::tui) home_page_size: usize,
    /// The pane Home handed the user into, drawn alone in the content area.
    ///
    /// A local view choice, so it never reaches the layout: the pane keeps the
    /// grid the shared layout gave it and is simply the only thing on screen.
    pub(in crate::tui) zoomed_pane: Option<PaneId>,
    /// Machines paired with this one. Filled by the client from the pairing
    /// record; a machine that is paired but not a session member is one you own
    /// that is not answering.
    pub(in crate::tui) paired_machines: Vec<PairedMachine>,
    /// This client's own peer id, so the machine list can mark which row is
    /// the machine you are sitting at.
    pub(in crate::tui) local_peer_id: Option<Vec<u8>>,
    /// Whether leaving and ending the session are different things here.
    /// See [`MultiPaneTui::set_detachable`].
    pub(in crate::tui) detachable: bool,
    pub(in crate::tui) resize_drag: Option<ResizeDrag>,
    /// Set while a press forwarded to a child owns the drag and release that follow.
    pub(in crate::tui) mouse_forwarding: bool,
    /// A share-modal copy request. Invite material lives in the node's rendezvous record
    /// rather than in the layout, so the attached client resolves and copies it.
    pub(in crate::tui) pending_share_copy: Option<ShareCopy>,
    /// Whether the add-machine panel has asked the client to record the
    /// session's ticket in the pairing file. The TUI owns no filesystem.
    pub(in crate::tui) pending_pair_offer: bool,
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
            presence: Vec::new(),
            prior_agent_states: BTreeMap::new(),
            prior_agent_episodes: BTreeMap::new(),
            notified_agent_episodes: BTreeMap::new(),
            unread_agent_panes: BTreeSet::new(),
            modal: ModalState::None,
            pending_home_toggle: None,
            home_open: false,
            home_selected: None,
            home_machine: None,
            home_notice: None,
            last_home_area: Rect::new(0, 0, 80, 24),
            home_page: 0,
            home_page_size: crate::tui::home::HOME_PAGE_MAX,
            zoomed_pane: None,
            paired_machines: Vec::new(),
            local_peer_id: None,
            detachable: false,
            resize_drag: None,
            session_locked: false,
            mouse_forwarding: false,
            pending_share_copy: None,
            pending_pair_offer: false,
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

    /// Whether a blocking dialog is open.
    pub fn modal_open(&self) -> bool {
        matches!(
            self.modal,
            ModalState::Rename(_)
                | ModalState::ConfirmDeleteTab { .. }
                | ModalState::Share
                | ModalState::Quit
                | ModalState::AddMachine(_)
        )
    }

    pub fn quit_open(&self) -> bool {
        matches!(self.modal, ModalState::Quit)
    }

    /// Tells this client it is attached to a node that outlives it.
    ///
    /// Only then are detaching and ending the session two different things, and
    /// only then is there a question for Ctrl+Q to ask. A foreground session
    /// owns its panes outright: leaving is ending, and a dialog offering to
    /// "leave it running" would be offering something that cannot happen.
    pub fn set_detachable(&mut self, detachable: bool) {
        self.detachable = detachable;
    }

    /// Replace where the other members are looking. Returns whether anything moved, so a
    /// presence update that changes nothing never costs a repaint.
    pub fn set_presence(&mut self, presence: Vec<crate::local_ipc::PresenceRow>) -> bool {
        if self.presence == presence {
            return false;
        }
        self.presence = presence;
        true
    }

    /// The other members watching a pane, in member-list order so chips never reshuffle.
    pub(in crate::tui) fn pane_watchers(
        &self,
        pane_id: PaneId,
    ) -> Vec<&crate::local_ipc::PresenceRow> {
        let mut watchers = self
            .presence
            .iter()
            .filter(|row| row.pane_id == pane_id)
            .collect::<Vec<_>>();
        watchers.sort_by_key(|row| self.member_slot(&row.peer_id));
        watchers
    }

    pub(in crate::tui) fn member_slot(&self, peer_id: &[u8]) -> usize {
        self.snapshot
            .members
            .iter()
            .position(|member| member.peer_id == peer_id)
            .unwrap_or(usize::MAX)
    }

    /// The other members on a tab, in member-list order so the dots never reshuffle.
    pub(in crate::tui) fn tab_watchers(
        &self,
        tab_id: TabId,
    ) -> Vec<&crate::local_ipc::PresenceRow> {
        let mut watchers = self
            .presence
            .iter()
            .filter(|row| row.tab_id == tab_id)
            .collect::<Vec<_>>();
        watchers.sort_by_key(|row| self.member_slot(&row.peer_id));
        watchers
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
        // A zoomed pane gets the whole content area to itself. This is a local
        // view choice and never reaches the layout, so the pane keeps the fixed
        // grid the shared layout gave it and simply stops sharing the screen
        // with its siblings.
        if let Some(pane_id) = self.zoomed_pane() {
            panes.insert(pane_id, content);
        } else if let Some(tab) = self.current_tab_layout() {
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

    /// Where the `inbox` badge sits in the tab bar, so a click can find it and
    /// the tab labels can start after it.
    pub(in crate::tui) fn inbox_rect(&self, tab_bar: Rect) -> Rect {
        let x = tab_bar
            .x
            .saturating_add(self.tab_bar_title_width())
            .saturating_add(text_width(TOP_BAR_BRAND_SEPARATOR));
        let width = text_width(&inbox_segment(self.home_needs_you_count()));
        Rect::new(
            x,
            tab_bar.y,
            width.min(tab_bar.right().saturating_sub(x)),
            tab_bar.height,
        )
    }

    /// Whether a tab is the one being looked at.
    ///
    /// Nothing is, while Home is open: Home is above the tabs, so lighting one
    /// up would claim the user is inside a tab they cannot see. The active-tab
    /// treatment moves to the `inbox` badge instead.
    pub(in crate::tui) fn is_active_tab(&self, tab_id: TabId) -> bool {
        tab_id == self.current_tab && !self.home_open
    }

    /// The session label's drawn width, after the bar's truncation order has
    /// been applied to it.
    pub(in crate::tui) fn tab_bar_title_width(&self) -> u16 {
        text_width(&truncate_trailing(&self.title, TOP_BAR_TITLE_MAX_WIDTH))
    }

    pub(in crate::tui) fn tab_label_rects(&self, tab_bar: Rect) -> BTreeMap<TabId, Rect> {
        let mut x = tab_bar
            .x
            .saturating_add(self.tab_bar_title_width())
            .saturating_add(text_width(TOP_BAR_BRAND_SEPARATOR))
            .saturating_add(inbox_segment_width(self.home_needs_you_count()));
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
                    self.is_active_tab(tab.tab_id),
                    self.tab_has_unread_agent_pane(tab),
                ))
                .saturating_add(tab_presence_width(self.tab_watchers(tab.tab_id).len()));
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
        if old_pane != pane_id {
            // A press was forwarded to the pane that held focus, and the child
            // there is waiting for its release. Focus moving out from under the
            // gesture -- a peer's layout change, a pane created elsewhere --
            // ends it: the release belongs to that child, not to the next one.
            self.mouse_forwarding = false;
        }
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

#[cfg(test)]
mod tests {

    use ratatui::layout::Rect;

    use crate::{
        layout::{LayoutError, Node, Tab},
        tui::{
            MultiPaneTui, PaneViewState,
            test_support::{layout, split_layout},
        },
    };

    #[test]
    fn pane_view_activity_transition_reports_a_redraw() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 1, 1)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("layout");
        let active = PaneViewState {
            ready: true,
            controller_peer_id: Some(b"controller".to_vec()),
            controller_active: true,
            scrollback: 0,
        };
        assert!(tui.set_pane_view(1, active.clone()));
        assert!(!tui.set_pane_view(1, active));
        assert!(tui.set_pane_view(
            1,
            PaneViewState {
                controller_active: false,
                ..tui.pane_view(1).expect("pane view").clone()
            },
        ));
    }

    #[test]
    fn same_revision_snapshot_preserves_resize_preview_and_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        let initial = tui.geometry(area);

        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        assert_ne!(tui.geometry(area), initial);

        tui.apply_snapshot(tui.snapshot().clone())
            .expect("valid snapshot");
        assert!(tui.resize_drag.is_some());
        assert!(tui.selection().is_some());
        assert_ne!(tui.geometry(area), initial);
    }

    #[test]
    fn same_revision_different_snapshot_is_rejected() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let mut conflicting = tui.snapshot().clone();
        conflicting.tabs[0].title = Some("conflict".into());

        assert_eq!(
            tui.apply_snapshot(conflicting),
            Err(LayoutError::ConflictingSnapshotRevision { revision: 1 })
        );
    }

    #[test]
    fn higher_revision_snapshot_cancels_resize_preview_and_selection() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        let area = Rect::new(0, 0, 80, 24);
        assert!(tui.begin_resize_drag(39, 5, area));
        assert!(tui.extend_resize_drag(49, 5));
        assert!(tui.begin_selection_at(2, 3, area));
        assert!(tui.extend_selection_at(3, 3, area));
        let mut newer = tui.snapshot().clone();
        newer.revision += 1;

        tui.apply_snapshot(newer).expect("valid snapshot");
        assert!(tui.resize_drag.is_none());
        assert!(tui.selection().is_none());
    }

    #[test]
    fn multi_pane_geometry_recursively_splits_the_content_area() {
        let tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let geometry = tui.geometry(Rect::new(0, 0, 80, 24));

        assert_eq!(geometry.tab_bar, Rect::new(0, 0, 80, 1));
        assert_eq!(geometry.footer, Rect::new(0, 23, 80, 1));
        assert_eq!(geometry.content, Rect::new(0, 1, 80, 22));
        assert_eq!(geometry.panes[&1], Rect::new(0, 1, 40, 22));
        assert_eq!(geometry.panes[&2], Rect::new(40, 1, 40, 11));
        assert_eq!(geometry.panes[&3], Rect::new(40, 12, 40, 11));
    }

    #[test]
    fn tiny_terminal_geometry_stays_in_bounds() {
        let tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let geometry = tui.geometry(Rect::new(u16::MAX, u16::MAX, 1, 1));

        assert_eq!(geometry.tab_bar, Rect::new(u16::MAX, u16::MAX, 1, 1));
        assert_eq!(geometry.footer, Rect::new(u16::MAX, u16::MAX, 1, 0));
        assert_eq!(geometry.content, Rect::new(u16::MAX, u16::MAX, 1, 0));
        assert!(
            geometry
                .panes
                .values()
                .all(|rect| rect.x == u16::MAX && rect.y == u16::MAX)
        );
    }

    #[test]
    fn snapshot_commit_repairs_removed_tab_and_pane_selection() {
        let initial = layout(
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
        );
        let mut tui = MultiPaneTui::new(initial).expect("valid layout");
        tui.select_tab(2).expect("select second tab");

        let mut committed = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        );
        committed.revision = 2;
        tui.apply_snapshot(committed).expect("valid commit");

        assert_eq!(tui.current_tab(), 1);
        assert_eq!(tui.focused_pane(), 1);
    }

    #[test]
    fn creator_selects_only_its_explicitly_reserved_tab_after_that_tab_commits() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");

        tui.select_created_tab(2);
        let mut unrelated = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (3, 2, 2)],
        );
        unrelated.revision = 2;
        tui.apply_snapshot(unrelated).expect("unrelated commit");
        assert_eq!(tui.current_tab(), 1);

        let mut targeted = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
            ],
            &[(1, 2, 2), (2, 2, 2), (3, 2, 2)],
        );
        targeted.revision = 3;
        tui.apply_snapshot(targeted).expect("targeted tab commit");

        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
    }
}
