//! What the node API exposes to a local client: screens, scrollback, focus,
//! intents, and the agent roster.

use std::{
    collections::{BTreeMap, BTreeSet},
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
        AgentOverlayRow, LocalScrollback, LocalScrollbackWindow, NodeLeaseSnapshots,
        NodeScreenSnapshot, NodeScreenSnapshots, UiIntent,
        clock::unix_ms_now,
        geometry::{contains_leaf, visible_leaf_panes},
        member_label,
        render::vt::viewed_screen,
        selection::selection_text,
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

    /// Deliver a client's bytes to the pane it addressed them to, and report
    /// which pane that turned out to be.
    ///
    /// `pane_id` is `None` for a client too old to name one; that one still gets
    /// this node's focus, which is what every client used to get. The pane comes
    /// back so the caller can prioritise that pane's next screen without asking
    /// focus a second question and getting a different answer.
    pub fn node_input(
        &mut self,
        pane_id: Option<PaneId>,
        bytes: Vec<u8>,
    ) -> Result<Option<PaneId>, Box<dyn Error>> {
        let pane_id = pane_id.unwrap_or_else(|| self.tui.focused_pane());
        if !self.input_allowed(pane_id) {
            return Ok(None);
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(bytes.clone())?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id) {
            pane.input(bytes);
        }
        self.tui.reset_scrollback(pane_id);
        Ok(Some(pane_id))
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
                    machine_id: crate::machine_id::to_hex(&member.machine_id),
                    kind: member.kind,
                    name,
                }
            })
            .collect()
    }

    /// Tell the fleet about every other session this machine is coordinating,
    /// and act on the invitations it has been sent.
    ///
    /// This is what makes your machines follow you: `p2pmux create` on a laptop
    /// starts a node that knows nothing about any fleet, but the laptop's *home
    /// session* node — the one pairing left running — sees the new session in
    /// the local store and offers its ticket to the machines already with it.
    ///
    /// Re-announced every time rather than once, because a machine that was
    /// asleep when a session started has to hear about it when it wakes. The
    /// Say, at most once per process, that this box belongs to a fleet.
    ///
    /// The member kind is decided in `node::run_background` before anything
    /// joins, from a pairing record that on this path did not exist yet. A box
    /// paired afterwards announced `Unspecified` for the whole life of that
    /// node, and `pairing::pin_into` refuses to write a peer into a fleet
    /// unless it has said outright that it is a machine — so the machine you
    /// just paired never made it into anybody's record, and its row lasted
    /// exactly as long as the session did.
    ///
    /// Latched rather than repeated: the declaration is idempotent at both
    /// ends, but a claim resent every membership tick is queue traffic for a
    /// fact that cannot change back.
    fn announce_fleet_membership(&mut self) {
        if self.announced_fleet_membership {
            return;
        }
        crate::session::set_local_member_kind(crate::layout::MemberKind::Machine);
        if self
            .control
            .try_declare_kind(crate::layout::MemberKind::Machine)
        {
            self.announced_fleet_membership = true;
        }
    }

    /// receiving side makes that cheap: it already knows which sessions it is
    /// in, and an invitation to one of them does nothing.
    pub(crate) fn exchange_fleet_invites(&mut self) {
        // Only a machine that has been paired has a fleet to talk to, and only
        // it has a record to judge invitations against.
        if !crate::pairing::load_or_empty().can_rejoin() {
            return;
        }
        // Paired, so this box belongs to a fleet — which it may well have said
        // the opposite of, because the claim is made once at node start and
        // `p2pmux pair` while a session is open is the ordinary way to add a
        // machine. Saying it again here is free once it is already recorded,
        // and until it is said nobody may write this machine into a fleet.
        self.announce_fleet_membership();
        for (from_peer_id, ticket) in self.control.take_fleet_invites() {
            self.consider_fleet_invite(&from_peer_id, &ticket);
        }
        let Ok(store) = crate::session_store::SessionStore::for_current_user() else {
            return;
        };
        // Recorded, not probed: this is the node's own drain loop, the one a
        // keystroke is echoed by, and `list_live` blocks on a socket per
        // session. A session whose node has died since is announced one last
        // time and refused by whoever follows it, which is cheaper than every
        // other tick of this loop paying for the certainty.
        let Ok(live) = store.list_recorded() else {
            return;
        };
        for session in live {
            let Some(ticket) = session.ticket else {
                continue;
            };
            // Not this session: announcing the one everybody is already in is
            // the one invitation guaranteed to be useless.
            if self.invite.ticket.as_deref() == Some(ticket.as_str()) {
                continue;
            }
            let sent = self.control.try_fleet_invite(ticket.clone());
            crate::tui::debug_log::ui_debug_log(
                "fleet_invite_sent",
                format_args!(
                    "session={} sent_to={:?} ticket={}",
                    session.name,
                    sent,
                    &ticket[..ticket.len().min(16)]
                ),
            );
        }
    }

    /// What another machine of yours is waiting to be allowed to start here.
    ///
    /// The node holds the request; the attached client is what can ask a human
    /// about it. `None` when nothing is held, which also retracts a question
    /// that has since been answered or expired.
    pub(crate) fn pending_remote_work(&self) -> Option<Vec<String>> {
        self.pending_remote
            .as_ref()
            .map(|(_, pending)| pending.command.clone())
    }

    /// What this node can offer for a pane the client wants to scroll back in.
    ///
    /// Three answers rather than one `Option`, because the caller has to say
    /// something to the user and "no window" was being reported as a single
    /// failure that named every cause it might have had — including two that
    /// are impossible for the pane in front of them. A brand-new local shell
    /// has no history for the ordinary reason that nothing has scrolled off it
    /// yet, and telling that user about remote panes and stale sessions is
    /// three guesses where the node knows the answer.
    pub(crate) fn node_local_scrollback(&self, pane_id: PaneId) -> LocalScrollback {
        let Some(pane) = self.local.get(&pane_id) else {
            return LocalScrollback::NotOurs;
        };
        if pane.screen.screen().alternate_screen() {
            return LocalScrollback::AlternateScreen;
        }
        let (total_rows, _) = pane.screen.history_metadata();
        if total_rows == 0 {
            return LocalScrollback::Empty;
        }
        LocalScrollback::Window(Box::new(LocalScrollbackWindow {
            total_rows,
            screen: pane.screen.screen().clone(),
        }))
    }

    /// Read a copy from the pane owner, whose normal-screen buffer retains all
    /// of the rows an attached client's sparse viewport cache may have evicted.
    pub(crate) fn node_selection_text(
        &self,
        selection: crate::tui::PaneTextSelection,
    ) -> Result<Option<String>, ()> {
        if let Some(pane) = self.local.get(&selection.pane_id) {
            return Ok(selection_text(selection, |offset| {
                Some(viewed_screen(pane.screen.screen(), offset))
            }));
        }
        let Some(pane) = self.remote.get(&selection.pane_id) else {
            return Err(());
        };
        // A remote screen has no retained history on this node. The live edge
        // is complete, but filling an older selection from it would fabricate
        // blank-looking lines that were never available here.
        if selection.anchor.scrollback != 0 || selection.cursor.scrollback != 0 {
            return Err(());
        }
        Ok(pane
            .screen
            .screen()
            .and_then(|screen| selection_text(selection, |_| Some(viewed_screen(screen, 0)))))
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

    /// Take the client's zoom and resize the panes this node hosts to match.
    ///
    /// A zoomed pane is alone on the screen, so it is entitled to the whole
    /// content area — and the PTY behind it has to be told, or it keeps drawing
    /// into the corner of a box that grew around it. Unzooming runs the same
    /// path in reverse: the geometry goes back to the split and every pane in it
    /// is resized back.
    pub fn node_zoom(
        &mut self,
        pane_id: Option<PaneId>,
        cols: u16,
        rows: u16,
    ) -> Result<(), Box<dyn Error>> {
        self.tui.set_zoomed_pane(pane_id);
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
        let mut entries = self
            .local
            .values()
            .filter(|pane| !pane.exited)
            .filter_map(crate::tui::SharedLocalPane::agent_roster_entry)
            .collect::<Vec<_>>();
        // The agents nobody started in a pane go out on the same roster, keyed
        // by their own process because they have no pane to be keyed by. Their
        // state is whatever their hooks left in `agent_status` — the same hooks
        // a pane's agent reports over the node socket, which have nowhere to
        // send from outside one. An agent that has never reported is `Unknown`
        // and the row says "running, and I cannot tell you more" rather than
        // inventing an answer from how quiet the process is.
        //
        // The agent's own words are not here, for the same reason they are not
        // on a pane's entry: this roster goes to every member of the session.
        entries.extend(
            self.loose_agents
                .iter()
                .map(|agent| crate::protocol::AgentRosterEntry {
                    pane_id: 0,
                    process_pid: agent.pid,
                    agent_kind: String::from(agent.kind.wire_value()),
                    cwd: truncate_bytes(
                        sanitize_single_line(&agent.cwd),
                        crate::protocol::MAX_AGENT_CWD_BYTES,
                    ),
                    state: crate::tui::pane::local::roster_state(agent.state) as i32,
                    working_since_unix_ms: agent.working_since_unix_ms,
                    // Unlike the message, this does travel. Which session an
                    // agent is in is a fact about the machine, not something
                    // the agent said, and the peer looking at the row is the
                    // one that has to be stopped from calling it "outside
                    // p2pmux".
                    session_name: truncate_bytes(
                        sanitize_single_line(&agent.session),
                        crate::protocol::MAX_SESSION_NAME_BYTES,
                    ),
                }),
        );
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

    /// What this node last told the session about where it is looking.
    ///
    /// `None` before it has said anything at all, which is a node that has
    /// neither been attached to nor moved.
    #[cfg(test)]
    pub(crate) fn local_presence(&self) -> Option<&Presence> {
        self.last_local_presence.as_ref()
    }

    /// Say whether a terminal is attached to this node, and tell the session.
    ///
    /// The node calls this on both edges. Attaching is what gives this member a
    /// location at all; detaching takes it away again, and the roster has to
    /// hear about the second as reliably as the first or the dot stays where
    /// somebody left it.
    pub fn set_client_attached(&mut self, attached: bool) -> bool {
        if self.client_attached == attached {
            return false;
        }
        self.client_attached = attached;
        self.maybe_publish_presence()
    }

    /// Tell the session where this member is now looking, if it moved.
    ///
    /// Called after anything that can change focus. There is no heartbeat and no timer:
    /// a human moving is the only thing that produces traffic here, so an idle session
    /// costs nothing.
    ///
    /// A node with no terminal on it is looking at nothing, and says so. That is
    /// not a corner case: a machine that followed a fleet invitation runs a node
    /// nobody has ever attached to, and every one of them used to claim the
    /// focus its layout happened to start with — putting a member's dot on a
    /// pane on somebody else's laptop, and painting that pane as watched, for a
    /// machine where nobody had opened the session at all.
    pub(in crate::tui) fn maybe_publish_presence(&mut self) -> bool {
        let attached = self.client_attached;
        let presence = Presence {
            peer_id: self.control.peer_id(),
            generation: self.presence_generation.saturating_add(1),
            // Normalized here as well as at the coordinator, so what this node
            // remembers sending matches what every other node will hold.
            tab_id: if attached { self.tui.current_tab() } else { 0 },
            pane_id: if attached { self.tui.focused_pane() } else { 0 },
            attached,
        };
        if self.last_local_presence.as_ref().is_some_and(|last| {
            last.tab_id == presence.tab_id
                && last.pane_id == presence.pane_id
                && last.attached == presence.attached
        }) {
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

    /// Which machine a member is on, and what that machine is called here.
    ///
    /// A machine can be several members: a second window that rejoined is a
    /// second node with its own peer id, and both announce the machine they are
    /// on. The machines rail already collapses those into one row and labels it
    /// after the first of them — so anything else that names a machine has to
    /// agree, or the inbox attributes an agent to a member with no row in the
    /// rail beside it.
    fn machine_identity(&self, peer_id: &[u8]) -> Vec<u8> {
        self.tui
            .snapshot()
            .members
            .iter()
            .find(|member| member.peer_id == peer_id)
            .map(|member| {
                if member.machine_id.is_empty() {
                    member.peer_id.clone()
                } else {
                    member.machine_id.clone()
                }
            })
            .unwrap_or_else(|| peer_id.to_vec())
    }

    /// The label the machines rail uses for whichever machine this peer is on.
    fn machine_label(&self, peer_id: &[u8]) -> String {
        let identity = self.machine_identity(peer_id);
        let members = &self.tui.snapshot().members;
        let canonical = members
            .iter()
            .find(|member| self.machine_identity(&member.peer_id) == identity)
            .map(|member| member.peer_id.clone())
            .unwrap_or_else(|| peer_id.to_vec());
        sanitize_single_line(&member_label(&canonical, members))
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
        let mut rows = self
            .agent_rosters
            .iter()
            .flat_map(|(host_peer_id, roster)| {
                let loose = roster
                    .entries
                    .iter()
                    .filter(|entry| entry.pane_id == 0)
                    .map(|entry| {
                        // An agent nobody started in a pane. There is no tab,
                        // no pane and nobody holding it, and the row says so
                        // with dashes rather than inventing a location.
                        let session = sanitize_single_line(&entry.session_name);
                        AgentOverlayRow {
                            pane_id: 0,
                            process_pid: entry.process_pid,
                            tab_ordinal: 0,
                            pane_ordinal: 0,
                            tab_label: String::from("—"),
                            // A pane of another session is still a pane, and
                            // this column used to swear otherwise.
                            pane_label: if session.is_empty() {
                                String::from("not in p2pmux")
                            } else {
                                format!("session {session}")
                            },
                            kind: sanitize_single_line(&entry.agent_kind),
                            cwd: sanitize_single_line(&entry.cwd),
                            state: AgentRosterState::from_wire(entry.state),
                            working_since_unix_ms: entry.working_since_unix_ms,
                            // The machine, not the node that reported it. An
                            // agent outside p2pmux belongs to the box, and the
                            // box may have several nodes in this session.
                            host: self.machine_label(host_peer_id),
                            controller: String::from("—"),
                            // Read from this node's own scan rather than the
                            // roster entry, so it is only ever present for an
                            // agent on this machine — the same rule a pane's
                            // message follows, for the same reason.
                            message: self
                                .loose_agents
                                .iter()
                                .find(|agent| {
                                    *host_peer_id == self.control.peer_id()
                                        && agent.pid == entry.process_pid
                                })
                                .map(|agent| agent.message.clone())
                                .unwrap_or_default(),
                            session,
                        }
                    })
                    .collect::<Vec<_>>();
                roster
                    .entries
                    .iter()
                    .filter_map(|entry| {
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
                                sanitize_single_line(&member_label(
                                    id,
                                    &self.tui.snapshot().members,
                                ))
                            })
                            .unwrap_or_else(|| String::from("free"));
                        Some(AgentOverlayRow {
                            pane_id: entry.pane_id,
                            process_pid: 0,
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
                            // A pane of this session. There is no other
                            // session to send the user to.
                            session: String::new(),
                        })
                    })
                    .chain(loose)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        // One process on one machine is one row, however many of that machine's
        // nodes are in this session to report it.
        //
        // An agent outside p2pmux is found by scanning the machine, and every
        // node on that machine scans the same machine. Two of them -- a session
        // you have open and a second window that rejoined it, which is the
        // ordinary way to end up with two -- therefore listed every bot on that
        // box twice, under two member names, only one of which the machines
        // rail had a row for.
        let mut seen = BTreeSet::new();
        rows.retain(|row| row.pane_id != 0 || seen.insert((row.host.clone(), row.process_pid)));
        rows
    }
}
