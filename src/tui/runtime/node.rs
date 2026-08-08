//! What the node API exposes to a local client: screens, scrollback, focus,
//! intents, and the agent roster.

use std::{
    collections::BTreeMap,
    error::Error,
    io,
    time::{Duration, Instant},
};

use ratatui::layout::Rect;

use crate::{
    agent_detect::{AgentKind, AgentState},
    layout::{LayoutSnapshot, PaneId, TabId},
    local_ipc::AgentOverlaySnapshotRow,
    protocol::{
        AgentRoster, AgentRosterState, MAX_AGENT_CWD_BYTES, MAX_AGENT_MESSAGE_BYTES, Presence,
    },
    tui::{
        AgentOverlayRow, LocalScrollbackWindow, NodeLeaseSnapshots, NodeScreenSnapshot,
        NodeScreenSnapshots, UiIntent,
        clock::unix_ms_now,
        geometry::{contains_leaf, visible_leaf_panes},
        member_label,
        text::{sanitize_single_line, truncate_bytes},
    },
};

use super::SharedLayoutRuntime;

impl SharedLayoutRuntime {
    /// Node-facing non-terminal operations. Kept small while the old foreground adapter is
    /// retired so pane/Iroh ownership has exactly one home.
    pub fn drain_node(&mut self) -> Result<bool, Box<dyn Error>> {
        self.drain()
    }

    pub fn node_input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(bytes.clone())?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id) {
            pane.input(bytes);
        }
        self.tui.reset_scrollback(pane_id);
        Ok(())
    }

    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> {
        let peer_id = self.control.peer_id();
        for pane in self.local.values_mut() {
            pane.release_controller(&peer_id)?;
        }
        for pane in self.remote.values_mut() {
            pane.release_controller();
        }
        Ok(())
    }

    /// A complete node-owned view for a newly attached local renderer.
    pub(crate) fn node_snapshot(
        &self,
    ) -> (
        LayoutSnapshot,
        NodeScreenSnapshots,
        NodeLeaseSnapshots,
        Vec<AgentOverlaySnapshotRow>,
    ) {
        let mut screens = BTreeMap::new();
        let mut chrome = BTreeMap::new();
        for (pane_id, pane) in &self.local {
            screens.insert(
                *pane_id,
                NodeScreenSnapshot::Local {
                    frame: pane.screen.current_frame().clone(),
                    history_len: pane.screen.history_metadata().0,
                    history_end: pane.screen.history_metadata().1,
                },
            );
            let view = pane.view_state();
            chrome.insert(
                *pane_id,
                (view.ready, view.controller_peer_id, view.controller_active),
            );
        }
        for (pane_id, pane) in &self.remote {
            if pane.screen.screen().is_some() {
                screens.insert(
                    *pane_id,
                    NodeScreenSnapshot::Remote {
                        sequence: pane.screen.sequence().unwrap_or(1),
                        kitty_keyboard_active: pane.screen.kitty_keyboard_active(),
                    },
                );
            }
            let view = pane.view_state();
            chrome.insert(
                *pane_id,
                (view.ready, view.controller_peer_id, view.controller_active),
            );
        }
        (
            self.tui.snapshot().clone(),
            screens,
            chrome,
            self.agent_overlay_rows()
                .iter()
                .map(AgentOverlaySnapshotRow::from)
                .collect(),
        )
    }

    /// Every machine in this session, with how many agents each is running.
    ///
    /// Deliberately separate from [`Self::node_snapshot`], which clones every
    /// pane's screen: this is read on a hot loop to notice a membership change,
    /// and the whole point is that noticing has to be nearly free.
    ///
    /// This is what makes `p2pmux machines` able to answer "is that machine
    /// awake" without attaching to the session. Only the node knows, and only
    /// the node is always running.
    pub(crate) fn session_peers(&self) -> Vec<crate::session_store::SessionPeer> {
        let local = self.local_peer_id();
        let members = &self.tui.snapshot().members;
        let mut agents_by_host: BTreeMap<String, usize> = BTreeMap::new();
        for roster in self.agent_rosters.values() {
            for entry in &roster.entries {
                let Some(pane) = self.tui.snapshot().panes.get(&entry.pane_id) else {
                    continue;
                };
                if pane.exited {
                    continue;
                }
                *agents_by_host
                    .entry(sanitize_single_line(&member_label(
                        &pane.host_peer_id,
                        members,
                    )))
                    .or_default() += 1;
            }
        }
        members
            .iter()
            .map(|member| {
                let name = sanitize_single_line(&member_label(&member.peer_id, members));
                crate::session_store::SessionPeer {
                    agents: agents_by_host.get(&name).copied().unwrap_or_default(),
                    this_machine: member.peer_id == local,
                    name,
                }
            })
            .collect()
    }

    pub(crate) fn node_local_scrollback(&self, pane_id: PaneId) -> Option<LocalScrollbackWindow> {
        let pane = self.local.get(&pane_id)?;
        let (total_rows, _) = pane.screen.history_metadata();
        if total_rows == 0 || pane.screen.screen().alternate_screen() {
            return None;
        }
        Some(LocalScrollbackWindow {
            total_rows,
            screen: pane.screen.screen().clone(),
        })
    }

    pub(crate) fn node_remote_snapshot(&self, pane_id: PaneId) -> Option<Vec<u8>> {
        let screen = self.remote.get(&pane_id)?.screen.screen()?;
        crate::screen::snapshot_payload(screen)
            .ok()
            .map(|snapshot| snapshot.as_ref().to_vec())
    }

    pub fn node_resize(&mut self, cols: u16, rows: u16) -> Result<(), Box<dyn Error>> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        self.reflow_local_panes(Rect::new(0, 0, cols, rows))
    }

    pub fn node_focus(&mut self, tab_id: TabId, pane_id: PaneId) -> Result<(), Box<dyn Error>> {
        let previous = self.tui.focused_pane();
        self.tui
            .set_focus(tab_id, pane_id)
            .map_err(|error| io::Error::other(format!("invalid node focus: {error:?}")))?;
        self.maybe_publish_presence();
        self.release_blurred_pane(previous)
    }

    pub fn node_intent(&mut self, intent: UiIntent) -> Result<(), Box<dyn Error>> {
        self.handle_intent(intent)
    }

    /// Apply a status pushed by a producer running inside one of this node's
    /// own panes. Returns whether it was accepted.
    ///
    /// The pane id is a claim from an unauthenticated local process, so it is
    /// checked against the panes this node actually hosts. That is the local
    /// half of the containment `Coordinator::accept_agent_roster` enforces
    /// between peers: a producer can only ever speak for a pane on the machine
    /// it runs on, and the roster it feeds is published under this node's own
    /// peer id.
    ///
    /// A kind or status this build does not know is refused rather than
    /// coerced — a lenient parse would let a typo file status under the wrong
    /// agent, or blank the row it meant to update.
    pub fn apply_agent_status(
        &mut self,
        pane_id: PaneId,
        kind: &str,
        status: &str,
        cwd: &str,
        message: &str,
    ) -> bool {
        let (Some(kind), Some(state)) = (AgentKind::from_wire(kind), AgentState::from_wire(status))
        else {
            return false;
        };
        let Some(pane) = self.local.get_mut(&pane_id) else {
            return false;
        };
        if pane.exited {
            return false;
        }
        // Capped here rather than at publish time: an over-long cwd would fail
        // `validate_agent_roster` and silently drop this host's whole roster.
        let cwd = truncate_bytes(sanitize_single_line(cwd), MAX_AGENT_CWD_BYTES);
        // The message never reaches `validate_agent_roster` — it is stripped
        // before the roster goes to peers — but it is agent-authored text
        // heading for a terminal, so it gets the same sanitizing treatment.
        let message = truncate_bytes(sanitize_single_line(message), MAX_AGENT_MESSAGE_BYTES);
        pane.agent_tracker.record_pushed_status(
            kind,
            cwd,
            state,
            message,
            Instant::now(),
            unix_ms_now(),
        );
        true
    }

    pub fn shutdown_node(self) {
        self.shutdown();
    }

    pub(in crate::tui) fn refresh_local_views(&mut self) -> bool {
        let mut changed = false;
        for (pane_id, pane) in &self.local {
            changed |= self.tui.set_pane_view(*pane_id, pane.view_state());
        }
        for (pane_id, pane) in &self.remote {
            changed |= self.tui.set_pane_view(*pane_id, pane.view_state());
        }
        changed
    }

    pub(in crate::tui) fn publish_local_agent_roster(&mut self) -> bool {
        let now = Instant::now();
        let entries = self
            .local
            .values()
            .filter(|pane| !pane.exited)
            .filter_map(crate::tui::SharedLocalPane::agent_roster_entry)
            .collect::<Vec<_>>();
        if entries == self.last_local_agent_entries && now < self.next_agent_roster_heartbeat {
            return false;
        }
        self.agent_roster_generation = self.agent_roster_generation.saturating_add(1);
        let roster = AgentRoster {
            host_peer_id: self.control.peer_id(),
            generation: self.agent_roster_generation,
            entries: entries.clone(),
        };
        if self.control.try_agent_roster(roster.clone()).is_err() {
            return false;
        }
        self.last_local_agent_entries = entries;
        self.next_agent_roster_heartbeat = now + Duration::from_secs(5);
        self.agent_rosters
            .insert(roster.host_peer_id.clone(), roster);
        true
    }

    pub(in crate::tui) fn refresh_agent_rows(&mut self) -> bool {
        self.tui.set_agent_rows(self.agent_overlay_rows())
    }

    /// Tell the session where this member is now looking, if it moved.
    ///
    /// Called after anything that can change focus. There is no heartbeat and no timer:
    /// a human moving is the only thing that produces traffic here, so an idle session
    /// costs nothing.
    pub(in crate::tui) fn maybe_publish_presence(&mut self) -> bool {
        let presence = Presence {
            peer_id: self.control.peer_id(),
            generation: self.presence_generation.saturating_add(1),
            tab_id: self.tui.current_tab(),
            pane_id: self.tui.focused_pane(),
            attached: true,
        };
        if self
            .last_local_presence
            .as_ref()
            .is_some_and(|last| last.tab_id == presence.tab_id && last.pane_id == presence.pane_id)
        {
            return false;
        }
        match self.control.try_presence(presence.clone()) {
            // The coordinator never receives its own broadcast, so it applies the roster
            // its own update produced rather than waiting for one to come back.
            Ok(Some(roster)) => self.presence = roster.entries,
            Ok(None) => {}
            Err(_) => return false,
        }
        self.presence_generation = presence.generation;
        self.last_local_presence = Some(presence);
        true
    }

    /// Presence of every member other than this one, ready for the renderer.
    pub fn presence_rows(&self) -> Vec<crate::local_ipc::PresenceRow> {
        let local_peer_id = self.control.peer_id();
        self.presence
            .iter()
            .filter(|entry| entry.attached && entry.peer_id != local_peer_id)
            .map(|entry| crate::local_ipc::PresenceRow {
                peer_id: entry.peer_id.clone(),
                tab_id: entry.tab_id,
                pane_id: entry.pane_id,
            })
            .collect()
    }

    pub fn agent_overlay_rows(&self) -> Vec<AgentOverlayRow> {
        let pane_locations =
            self.tui
                .snapshot()
                .tabs
                .iter()
                .enumerate()
                .flat_map(|(tab_index, tab)| {
                    visible_leaf_panes(&tab.root).into_iter().enumerate().map(
                        move |(pane_index, pane_id)| (pane_id, (tab_index + 1, pane_index + 1)),
                    )
                })
                .collect::<BTreeMap<_, _>>();
        self.agent_rosters
            .values()
            .flat_map(|roster| {
                roster.entries.iter().filter_map(|entry| {
                    let pane = self.tui.snapshot().panes.get(&entry.pane_id)?;
                    if pane.exited {
                        return None;
                    }
                    let view = self.tui.pane_view(entry.pane_id)?;
                    let &(tab_ordinal, pane_ordinal) = pane_locations.get(&entry.pane_id)?;
                    let tab = self
                        .tui
                        .snapshot()
                        .tabs
                        .iter()
                        .find(|tab| contains_leaf(&tab.root, entry.pane_id))?;
                    let host = sanitize_single_line(&member_label(
                        &pane.host_peer_id,
                        &self.tui.snapshot().members,
                    ));
                    let controller = view
                        .controller_peer_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .map(|id| {
                            sanitize_single_line(&member_label(id, &self.tui.snapshot().members))
                        })
                        .unwrap_or_else(|| String::from("free"));
                    Some(AgentOverlayRow {
                        pane_id: entry.pane_id,
                        tab_ordinal,
                        pane_ordinal,
                        tab_label: tab
                            .title
                            .clone()
                            .unwrap_or_else(|| format!("Tab #{tab_ordinal}")),
                        pane_label: pane
                            .title
                            .clone()
                            .unwrap_or_else(|| format!("Pane #{pane_ordinal}")),
                        kind: sanitize_single_line(&entry.agent_kind),
                        cwd: sanitize_single_line(&entry.cwd),
                        state: AgentRosterState::from_wire(entry.state),
                        working_since_unix_ms: entry.working_since_unix_ms,
                        host,
                        controller,
                        // Read from the pane rather than the roster entry, and
                        // so only ever present for a pane this node hosts: the
                        // roster is the peer-facing shape and has no field for
                        // it. A member's agent reports its message to its own
                        // node, where it stays.
                        message: self
                            .local
                            .get(&entry.pane_id)
                            .and_then(crate::tui::SharedLocalPane::listed_agent)
                            .map(|listed| listed.message)
                            .unwrap_or_default(),
                    })
                })
            })
            .collect()
    }
}
