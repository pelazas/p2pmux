//! Applying layout state and turning UI intents into coordinator requests.

use std::{
    collections::BTreeSet,
    error::Error,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::terminal;
use ratatui::layout::Rect;

use crate::{
    agent_detect::cwd_for_pid,
    layout::{Axis, NewPanePosition, PaneId},
    protocol::{
        CreatePane, CreateTab, DeletePane, DeleteTab, LayoutRejectReason, LayoutRequest,
        MarkPaneExited, NewPanePosition as ProtocolNewPanePosition, PaneDescriptor, PaneFailed,
        PaneReady, RenamePane, RenameTab, SetPaneLock, SplitAxis,
    },
    session::{
        CoordinatorResponse, LayoutControlEvent, layout_snapshot_from_state, subscribe_pane,
    },
    tui::{
        UiIntent,
        geometry::{area_from_terminal_size, grid_for_pane},
        pane::{
            control::{PendingCreate, SharedControl},
            local::SharedLocalPane,
        },
    },
};

use super::SharedLayoutRuntime;

/// How long a machine waits before acting on the same invitation twice.
///
/// Long enough that a repeated announcement costs nothing, short enough that a
/// session which becomes joinable — a coordinator that was still starting, a
/// network that came back — is joined while the person who started it is still
/// waiting for it.
const FLEET_INVITE_RETRY: Duration = Duration::from_secs(60);

impl SharedLayoutRuntime {
    pub(in crate::tui) fn handle_control_event(
        &mut self,
        event: LayoutControlEvent,
    ) -> Result<(), Box<dyn Error>> {
        self.reconciler.apply(&event)?;
        match event {
            LayoutControlEvent::Snapshot(snapshot) => {
                self.apply_layout_state(snapshot.state.as_ref().ok_or("missing layout state")?)?;
            }
            LayoutControlEvent::AgentRoster(roster) => {
                self.agent_rosters
                    .insert(roster.host_peer_id.clone(), roster);
            }
            LayoutControlEvent::Presence(roster) => self.presence = roster.entries,
            LayoutControlEvent::Commit(commit) => {
                self.apply_layout_state(commit.state.as_ref().ok_or("missing layout state")?)?;
            }
            LayoutControlEvent::Reservation(reservation) => self.accept_reservation(reservation)?,
            LayoutControlEvent::Reject(reject) => {
                self.reject_request_with_reason(reject.request_id, reject.reason)
            }
            LayoutControlEvent::FleetInvite {
                from_peer_id,
                ticket,
            } => self.consider_fleet_invite(&from_peer_id, &ticket),
            LayoutControlEvent::Disconnected => self.note_coordinator_lost(),
        }
        Ok(())
    }

    /// Decide whether to follow a machine into a session it has started.
    ///
    /// This is what makes the fleet a property of *you* rather than of the one
    /// session pairing recorded: your other machines are already here, in the
    /// home session, and this is how they are told where you went.
    ///
    /// Two questions, both answered locally. Is the inviter one of my machines
    /// — asked of this machine's own pairing record, against a peer id the
    /// transport authenticated, so a guest in the session cannot summon your
    /// droplet anywhere. And am I already there — because a session announces
    /// itself on a timer, and joining twice would leave two nodes of the same
    /// machine in one member list.
    ///
    /// Asked at most once a minute per invitation, and that rate limit is the
    /// difference between a re-announcement and a decision. The sender repeats
    /// itself every couple of seconds on purpose; this end used to answer every
    /// repetition from scratch — a pairing file read, a walk of the session
    /// store, and for anything it could not join, a node spawned to fail again.
    /// A machine left coordinating a session nobody wants, which is what a
    /// forgotten `p2pmux` on a droplet is, therefore cost its whole fleet a
    /// stall every two seconds for as long as it stayed up.
    pub(in crate::tui) fn consider_fleet_invite(&mut self, from_peer_id: &[u8], ticket: &str) {
        if self
            .considered_fleet_invites
            .get(ticket)
            .is_some_and(|last| last.elapsed() < FLEET_INVITE_RETRY)
        {
            return;
        }
        // Bounded by the invitations actually seen, and those are bounded by the
        // sessions your own machines are running. Dropped once an invitation has
        // gone quiet for long enough that acting on it again would be free.
        self.considered_fleet_invites
            .retain(|_, last| last.elapsed() < FLEET_INVITE_RETRY * 10);
        self.considered_fleet_invites
            .insert(ticket.to_owned(), Instant::now());
        let name = crate::tui::member_label(from_peer_id, &self.tui.snapshot().members);
        let member = self
            .tui
            .snapshot()
            .members
            .iter()
            .find(|member| member.peer_id == from_peer_id)
            .cloned();
        let kind = member
            .as_ref()
            .map(|member| member.kind)
            .unwrap_or_default();
        // The machine, not the node: the peer that sent this is one process on
        // it, and the fleet record is about the box.
        let machine = member
            .as_ref()
            .map(|member| crate::machine_id::to_hex(&member.machine_id))
            .unwrap_or_default();
        let owned = crate::pairing::load_or_empty().owns(&machine, &name, kind);
        crate::tui::debug_log::ui_debug_log(
            "fleet_invite_considered",
            format_args!("from={name} owned={owned} kind={kind:?}"),
        );
        if !owned {
            self.status = format!("ignored an invitation from {name}, which is not my machine");
            return;
        }
        // Detached: this node is following an invitation on behalf of the
        // machine, and the session it starts belongs to the fleet rather than to
        // the node that noticed the invitation. Tethering it would make one
        // node's restart take another node's session down with it.
        let followed = crate::node::follow_fleet_invite(ticket, crate::node::Tether::Detached);
        crate::tui::debug_log::ui_debug_log(
            "fleet_invite_received",
            format_args!(
                "from={name} followed={:?}",
                followed.as_ref().map_err(|error| error.to_string())
            ),
        );
        match followed {
            Ok(true) => self.status = format!("joined the session {name} started"),
            Ok(false) => {}
            Err(error) => {
                self.status = format!("could not follow {name}: {error}");
                // And in the session log, which outlives the status line and is
                // readable on a machine nobody is sitting at. A fleet that
                // cannot follow one of its own left no trace anywhere: the
                // status line is drawn for whoever is attached, and the machine
                // this happens on is by definition often nobody.
                eprintln!("p2pmux node: could not follow {name} into a session: {error}");
            }
        }
    }

    pub(in crate::tui) fn apply_layout_state(
        &mut self,
        state: &crate::protocol::LayoutState,
    ) -> Result<(), Box<dyn Error>> {
        let snapshot = layout_snapshot_from_state(state)
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let current_ids = snapshot.panes.keys().copied().collect::<BTreeSet<_>>();
        let prior_revision = self.tui.snapshot().revision;
        self.agent_rosters.retain(|host, roster| {
            roster.entries.retain(|entry| {
                // An agent in no pane outlives every pane: it is a process on
                // that machine, and it stops being listed when that machine
                // stops reporting it or leaves.
                entry.pane_id == 0
                    || snapshot
                        .panes
                        .get(&entry.pane_id)
                        .is_some_and(|pane| pane.host_peer_id == *host)
            });
            snapshot
                .members
                .iter()
                .any(|member| member.peer_id == *host)
        });
        // A successful authoritative commit is the only point at which a provisional local PTY
        // becomes a real pane. Forget the request bookkeeping then; rejection handles the other
        // path and tears the provisional PTY down.
        self.provisional
            .retain(|_, pane_id| !current_ids.contains(pane_id));
        let local_ids = self.local.keys().copied().collect::<Vec<_>>();
        for pane_id in local_ids {
            if !current_ids.contains(&pane_id) {
                let _ = self.panes.remove_local_pane(pane_id)?;
                if let Some(mut pane) = self.local.remove(&pane_id) {
                    pane.shutdown()?;
                }
            }
        }
        for (pane_id, pane) in &snapshot.panes {
            if let Some(local) = self.local.get_mut(pane_id) {
                local.set_locked(pane.locked)?;
                if pane.exited {
                    local.mark_exited()?;
                }
            }
            if pane.exited
                && let Some(remote) = self.remote.get_mut(pane_id)
            {
                remote.mark_exited();
            }
        }
        self.pending_locks.retain(|_, (pane_id, locked)| {
            snapshot
                .panes
                .get(pane_id)
                .is_some_and(|pane| pane.locked != *locked)
        });
        self.pending_exits
            .retain(|pane_id, _| snapshot.panes.get(pane_id).is_some_and(|pane| !pane.exited));
        let remote_ids = self.remote.keys().copied().collect::<Vec<_>>();
        for pane_id in remote_ids {
            if !current_ids.contains(&pane_id)
                && let Some(pane) = self.remote.remove(&pane_id)
            {
                self.spawn_remote_shutdown(pane.pane);
            }
        }
        // A terminal asked for on another machine arrives as a commit rather
        // than as something this process spawned, so this is where the person
        // who asked for it gets taken to it. Without this, pressing enter on a
        // machine created a tab somewhere off screen and looked like nothing
        // happened at all.
        if let Some(pending) = self
            .pending_create
            .as_ref()
            .filter(|pending| !pending.hosted_here && !pending.target_peer.is_empty())
        {
            let target = pending.target_peer.clone();
            let arrived = snapshot
                .panes
                .values()
                .filter(|pane| pane.host_peer_id == target && !pane.exited)
                .map(|pane| pane.pane_id)
                .find(|pane_id| !self.tui.snapshot().panes.contains_key(pane_id));
            if let Some(pane_id) = arrived {
                self.pending_create = None;
                if let Some(tab) = snapshot
                    .tabs
                    .iter()
                    .find(|tab| crate::tui::geometry::contains_leaf(&tab.root, pane_id))
                {
                    self.tui.select_created_tab(tab.tab_id);
                }
            }
        }
        let previously_focused = self.tui.focused_pane();
        self.tui
            .apply_snapshot(snapshot.clone())
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        self.release_blurred_pane(previously_focused)?;
        let me = self.control.peer_id();
        self.remote_descriptors.clear();
        for pane in state
            .panes
            .iter()
            .filter(|pane| pane.host_peer_id != me && !pane.exited)
        {
            let endpoint = state
                .members
                .iter()
                .find(|member| member.peer_id == pane.host_peer_id)
                .and_then(|member| serde_json::from_slice(&member.endpoint_addr).ok());
            let Some(endpoint) = endpoint else {
                self.status = format!("pane {} has no usable host address", pane.pane_id);
                continue;
            };
            self.remote_descriptors
                .insert(pane.pane_id, (endpoint, pane.clone()));
        }
        let remote_ids = self
            .remote_descriptors
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        self.subscriptions.retain(&remote_ids);
        self.subscriptions.nudge();
        self.start_eligible_subscriptions();
        self.refresh_local_views();
        if let Some(area) = area_from_terminal_size(terminal::size()) {
            self.reflow_local_panes(area)?;
        }
        if snapshot.revision > prior_revision {
            self.send_pending_exit_marks()?;
        }
        Ok(())
    }

    pub(in crate::tui) fn reflow_local_panes(&mut self, area: Rect) -> Result<(), Box<dyn Error>> {
        let geometry = self.tui.geometry(area);
        let mut updates = Vec::new();
        for (pane_id, rect) in geometry.panes {
            let Some(pane) = self.local.get_mut(&pane_id) else {
                continue;
            };
            if pane.exited
                || self
                    .tui
                    .snapshot()
                    .panes
                    .get(&pane_id)
                    .is_some_and(|descriptor| descriptor.exited)
            {
                continue;
            }
            let (rows, cols) = grid_for_pane(rect);
            if pane.screen.screen().size() == (rows, cols) {
                continue;
            }
            pane.resize(rows, cols)?;
            if self
                .tui
                .snapshot()
                .panes
                .get(&pane_id)
                .is_some_and(|descriptor| {
                    (descriptor.grid_rows, descriptor.grid_cols) != (rows, cols)
                })
            {
                updates.push(crate::protocol::PaneGrid {
                    pane_id,
                    grid_rows: u32::from(rows),
                    grid_cols: u32::from(cols),
                });
            }
        }
        if !updates.is_empty() {
            let request_id = self.next_id();
            self.send_request(LayoutRequest {
                request_id,
                base_revision: self.tui.snapshot().revision,
                create_pane: None,
                delete_pane: None,
                create_tab: None,
                delete_tab: None,
                set_split_ratio: None,
                update_pane_grids: Some(crate::protocol::UpdatePaneGrids { panes: updates }),

                rename_pane: None,
                rename_tab: None,
                set_pane_lock: None,
                mark_pane_exited: None,
                author_signature: Vec::new(),
            })?;
        }
        Ok(())
    }

    pub(in crate::tui) fn start_eligible_subscriptions(&mut self) {
        for (pane_id, (endpoint, descriptor)) in self.remote_descriptors.clone() {
            if self.remote.contains_key(&pane_id)
                || !self.subscriptions.start(pane_id, self.retry_tick)
            {
                continue;
            }
            let tx = self.subscription_tx.clone();
            let transport = self.transport.clone();
            let session_id = self.session_id.clone();
            self.runtime.spawn(async move {
                let result = subscribe_pane(transport, session_id, endpoint, descriptor)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send((pane_id, result));
            });
        }
    }

    pub(in crate::tui) fn send_pending_exit_marks(&mut self) -> Result<(), Box<dyn Error>> {
        let revision = self.tui.snapshot().revision;
        let pane_ids = self
            .pending_exits
            .iter()
            .filter_map(|(pane_id, attempted_revision)| {
                (*attempted_revision < revision).then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            let request_id = self.next_id();
            self.pending_exits.insert(pane_id, revision);
            self.send_request(LayoutRequest {
                request_id,
                base_revision: revision,
                create_pane: None,
                delete_pane: None,
                create_tab: None,
                delete_tab: None,
                set_split_ratio: None,
                update_pane_grids: None,
                rename_pane: None,
                rename_tab: None,
                set_pane_lock: None,
                mark_pane_exited: Some(MarkPaneExited { pane_id }),
                author_signature: Vec::new(),
            })?;
        }
        Ok(())
    }

    pub(in crate::tui) fn handle_intent(&mut self, intent: UiIntent) -> Result<(), Box<dyn Error>> {
        // Refused here rather than sent and forgotten. Only the coordinator commits any of
        // these, so with it missing the request would go into a channel with nothing on the
        // other end -- and a split, worse, would spawn a local PTY first and leave it
        // orphaned when the commit that was meant to adopt it never came. Moving focus and
        // switching tabs are this member's own business and stay available.
        if self.structural_edits_frozen()
            && !matches!(
                intent,
                UiIntent::FocusPane { .. } | UiIntent::SwitchTab { .. }
            )
        {
            let notice = String::from("coordinator unreachable; layout changes are paused");
            self.footer_notice = Some(notice.clone());
            // Also as status, which is the only one of the two that survives the trip to an
            // attached client: the node is headless and forwards `status`, so a refusal that
            // lived in `footer_notice` alone would be invisible to everybody not running the
            // old foreground path -- which is to say, to everybody.
            self.status = notice;
            return Ok(());
        }
        match intent {
            UiIntent::CreatePane {
                target_pane_id,
                axis,
                position,
                grid_rows,
                grid_cols,
            } => {
                let cwd = self
                    .local
                    .get(&target_pane_id)
                    .and_then(|pane| pane.host.process_id())
                    .and_then(cwd_for_pid);
                self.begin_create(
                    Some((target_pane_id, axis, position)),
                    grid_rows,
                    grid_cols,
                    cwd,
                )?;
            }
            UiIntent::CreateTab {
                grid_rows,
                grid_cols,
            } => {
                self.begin_create(None, grid_rows, grid_cols, None)?;
            }
            UiIntent::AnswerRemoteWork { approved } => self.answer_remote_work(approved)?,
            UiIntent::CreateTabOn {
                peer_id,
                command,
                name,
                grid_rows,
                grid_cols,
                title,
            } => {
                self.begin_create_on(
                    None, grid_rows, grid_cols, None, peer_id, command, name, title,
                )?;
            }
            UiIntent::DeletePane { pane_id } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: Some(DeletePane { pane_id }),
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: None,
                    update_pane_grids: None,

                    rename_pane: None,
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                    author_signature: Vec::new(),
                })?
            }
            UiIntent::DeleteTab { tab_id } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: Some(DeleteTab { tab_id }),
                    set_split_ratio: None,
                    update_pane_grids: None,

                    rename_pane: None,
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                    author_signature: Vec::new(),
                })?
            }
            UiIntent::SetSplitRatio {
                pane_id,
                axis,
                first_share_bps,
                base_revision,
            } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: Some(crate::protocol::SetSplitRatio {
                        pane_id,
                        axis: Some(match axis {
                            Axis::LeftRight => SplitAxis::LeftRight as i32,
                            Axis::TopBottom => SplitAxis::TopBottom as i32,
                        }),
                        first_share_bps: u32::from(first_share_bps),
                    }),
                    update_pane_grids: None,
                    rename_pane: None,
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                    author_signature: Vec::new(),
                })?;
            }
            UiIntent::RenamePane { pane_id, title } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: None,
                    update_pane_grids: None,
                    rename_pane: Some(RenamePane { pane_id, title }),
                    rename_tab: None,
                    set_pane_lock: None,
                    mark_pane_exited: None,
                    author_signature: Vec::new(),
                })?;
            }
            UiIntent::RenameTab { tab_id, title } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: None,
                    set_split_ratio: None,
                    update_pane_grids: None,
                    rename_pane: None,
                    rename_tab: Some(RenameTab { tab_id, title }),
                    set_pane_lock: None,
                    mark_pane_exited: None,
                    author_signature: Vec::new(),
                })?;
            }
            UiIntent::SetPaneLock { pane_id, locked } => {
                self.set_pane_lock(pane_id, locked)?;
            }
            UiIntent::SetSessionLock { locked } => {
                self.set_session_lock(locked)?;
            }
            UiIntent::FocusPane { .. } | UiIntent::SwitchTab { .. } => {}
        }
        Ok(())
    }

    /// Close or reopen the session to newcomers.
    ///
    /// Refused for a guest rather than forwarded: the coordinator is the only peer that
    /// answers joins, so a guest "locking" would change nothing while looking like it had.
    pub(in crate::tui) fn set_session_lock(&mut self, locked: bool) -> Result<(), Box<dyn Error>> {
        let SharedControl::Host(host) = &self.control else {
            self.status = String::from("only the session host can lock this session");
            return Ok(());
        };
        let locked = host.set_session_lock(locked)?;
        self.tui.set_session_locked(locked);
        self.status = String::from(if locked {
            "session locked — new peers are refused"
        } else {
            "session unlocked — anyone with the ticket can join"
        });
        Ok(())
    }

    pub(in crate::tui) fn begin_create(
        &mut self,
        pane: Option<(PaneId, Axis, NewPanePosition)>,
        grid_rows: u16,
        grid_cols: u16,
        cwd: Option<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        self.begin_create_on(
            pane,
            grid_rows,
            grid_cols,
            cwd,
            Vec::new(),
            Vec::new(),
            String::new(),
            None,
        )
    }

    /// Ask for a pane, optionally on another machine and optionally running
    /// something other than a shell.
    ///
    /// `target` empty means here, which is what every split and every new tab
    /// means and what this did before machines could ask each other.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::tui) fn begin_create_on(
        &mut self,
        pane: Option<(PaneId, Axis, NewPanePosition)>,
        grid_rows: u16,
        grid_cols: u16,
        cwd: Option<PathBuf>,
        target: Vec<u8>,
        command: Vec<String>,
        target_name: String,
        title: Option<String>,
    ) -> Result<(), Box<dyn Error>> {
        if self.pending_create.is_some() {
            self.status = String::from("waiting for current pane reservation");
            return Ok(());
        }
        let request_id = self.next_id();
        let base_revision = self.tui.snapshot().revision;
        let hosted_here = target.is_empty() || target == self.control.peer_id();
        self.pending_create = Some(PendingCreate {
            request_id,
            base_revision,
            grid_rows,
            grid_cols,
            cwd,
            command: command.clone(),
            hosted_here,
            target_name,
            target_peer: target.clone(),
            title,
        });
        self.send_request(LayoutRequest {
            request_id,
            base_revision,
            create_pane: pane.map(|(target_pane_id, axis, position)| CreatePane {
                target_pane_id,
                axis: Some(match axis {
                    Axis::LeftRight => SplitAxis::LeftRight as i32,
                    Axis::TopBottom => SplitAxis::TopBottom as i32,
                }),
                grid_rows: u32::from(grid_rows),
                grid_cols: u32::from(grid_cols),
                position: Some(match position {
                    NewPanePosition::First => ProtocolNewPanePosition::First as i32,
                    NewPanePosition::Second => ProtocolNewPanePosition::Second as i32,
                }),
                target_peer_id: target.clone(),
                command: command.clone(),
            }),
            delete_pane: None,
            create_tab: pane.is_none().then_some(CreateTab {
                grid_rows: u32::from(grid_rows),
                grid_cols: u32::from(grid_cols),
                target_peer_id: target,
                command,
            }),
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,
            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        })
    }

    pub(in crate::tui) fn set_pane_lock(
        &mut self,
        pane_id: PaneId,
        locked: bool,
    ) -> Result<(), Box<dyn Error>> {
        let peer_id = self.control.peer_id();
        if self
            .tui
            .snapshot()
            .panes
            .get(&pane_id)
            .is_none_or(|pane| pane.host_peer_id != peer_id)
        {
            self.footer_notice = Some(String::from("only the pane host can lock it"));
            return Ok(());
        }
        let Some(pane) = self.local.get_mut(&pane_id) else {
            self.footer_notice = Some(String::from("pane host is unavailable"));
            return Ok(());
        };
        let previous = pane.locked;
        pane.set_locked(locked)?;
        let request_id = self.next_id();
        self.pending_locks.insert(request_id, (pane_id, previous));
        let request = LayoutRequest {
            request_id,
            base_revision: self.tui.snapshot().revision,
            create_pane: None,
            delete_pane: None,
            create_tab: None,
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,
            rename_pane: None,
            rename_tab: None,
            set_pane_lock: Some(SetPaneLock { pane_id, locked }),
            mark_pane_exited: None,
            author_signature: Vec::new(),
        };
        if let Err(error) = self.send_request(request) {
            self.pending_locks.remove(&request_id);
            if let Some(pane) = self.local.get_mut(&pane_id) {
                pane.set_locked(previous)?;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::tui) fn send_request(
        &mut self,
        request: LayoutRequest,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(response) = self.control.try_request(request)? {
            match response {
                CoordinatorResponse::Reservation(reservation) => {
                    self.accept_reservation(reservation)?
                }
                CoordinatorResponse::Commit(commit) => {
                    self.handle_control_event(LayoutControlEvent::Commit(commit))?
                }
                CoordinatorResponse::Reject(reject) => {
                    self.reject_request_with_reason(reject.request_id, reject.reason)
                }
            }
        }
        Ok(())
    }

    pub(in crate::tui) fn accept_reservation(
        &mut self,
        reservation: crate::protocol::PaneReservation,
    ) -> Result<(), Box<dyn Error>> {
        let host_peer_id = self.control.peer_id();
        if !reservation.host_peer_id.is_empty() && reservation.host_peer_id != host_peer_id {
            // Ours to have asked for, another machine's to serve. The pane
            // arrives with the commit that machine's `PaneReady` produces.
            return Ok(());
        }
        // Either this process asked for the pane, or a machine of yours asked
        // for one here. In the second case there is no local record of the
        // request, which is why the reservation carries everything needed to
        // serve it.
        let asked_here = self.pending_create.as_ref().is_some_and(|pending| {
            pending.hosted_here
                && (reservation.request_id == 0 || pending.request_id == reservation.request_id)
        });
        let pending = match asked_here {
            true => self.pending_create.take().unwrap_or_else(|| unreachable!()),
            false => PendingCreate {
                request_id: reservation.request_id,
                base_revision: reservation.base_revision,
                grid_rows: u16::try_from(reservation.grid_rows).unwrap_or_default(),
                grid_cols: u16::try_from(reservation.grid_cols).unwrap_or_default(),
                cwd: None,
                command: reservation.command.clone(),
                hosted_here: true,
                target_name: String::new(),
                target_peer: Vec::new(),
                title: None,
            },
        };
        if pending.request_id == 0 || pending.grid_rows == 0 || pending.grid_cols == 0 {
            self.status = String::from("unexpected pane reservation");
            return Ok(());
        }
        // A pane this machine asked for on itself needs no permission from
        // itself. Everything else is a machine of yours asking, and the answer
        // is given here, by whoever owns this box.
        crate::tui::debug_log::ui_debug_log(
            "reservation_accepted",
            format_args!(
                "reservation={} request={} asked_here={asked_here} command={:?}",
                reservation.reservation_id, pending.request_id, pending.command
            ),
        );
        if !asked_here {
            let decision = crate::pairing::load_or_empty().work_decision(&pending.command);
            crate::tui::debug_log::ui_debug_log(
                "remote_work_decision",
                format_args!("decision={decision:?} command={:?}", pending.command),
            );
            match decision {
                crate::pairing::WorkDecision::Refuse => {
                    let sent = self.control.try_failed(PaneFailed {
                        reservation_id: reservation.reservation_id,
                        request_id: pending.request_id,
                        base_revision: pending.base_revision,
                        refused: true,
                    });
                    crate::tui::debug_log::ui_debug_log(
                        "remote_work_refused",
                        format_args!("sent={:?}", sent.as_ref().err()),
                    );
                    self.status =
                        String::from("refused a remote terminal: not on this machine's allowlist");
                    return Ok(());
                }
                crate::pairing::WorkDecision::Ask => {
                    // Held rather than answered. Nobody may be at this machine,
                    // and that is the point: an unanswered request expires with
                    // the coordinator's reservation rather than being granted.
                    self.tui.ask_remote_work(&pending.command);
                    self.pending_remote = Some((reservation, pending));
                    return Ok(());
                }
                crate::pairing::WorkDecision::Allow => {}
            }
        }
        self.spawn_reserved_pane(reservation, pending, asked_here)
    }

    /// Answer a held request. `approved` is the owner's own keystroke on this
    /// machine, and a `false` here and an expiry are the same outcome by
    /// design: the only way to get a pane is somebody saying yes.
    pub(in crate::tui) fn answer_remote_work(
        &mut self,
        approved: bool,
    ) -> Result<(), Box<dyn Error>> {
        let Some((reservation, pending)) = self.pending_remote.take() else {
            return Ok(());
        };
        if !approved {
            let _ = self.control.try_failed(PaneFailed {
                reservation_id: reservation.reservation_id,
                request_id: pending.request_id,
                base_revision: pending.base_revision,
                refused: true,
            });
            return Ok(());
        }
        // Never `asked_here`: a held request is by construction one another
        // machine made, so the cursor of whoever is sitting here does not move.
        self.spawn_reserved_pane(reservation, pending, false)
    }

    fn spawn_reserved_pane(
        &mut self,
        reservation: crate::protocol::PaneReservation,
        pending: PendingCreate,
        asked_here: bool,
    ) -> Result<(), Box<dyn Error>> {
        let host_peer_id = self.control.peer_id();
        let pane = match SharedLocalPane::spawn_program(
            reservation.pane_id,
            pending.grid_rows,
            pending.grid_cols,
            host_peer_id.clone(),
            pending.cwd.as_deref(),
            &pending.command,
        ) {
            Ok(pane) => pane,
            Err(error) => {
                let _ = self.control.try_failed(PaneFailed {
                    reservation_id: reservation.reservation_id,
                    request_id: pending.request_id,
                    base_revision: pending.base_revision,
                    refused: false,
                });
                self.status = format!("pane spawn failed: {error}");
                return Ok(());
            }
        };
        let descriptor = PaneDescriptor {
            pane_id: reservation.pane_id,
            host_peer_id,
            grid_rows: u32::from(pending.grid_rows),
            grid_cols: u32::from(pending.grid_cols),
            title: None,
            locked: false,
            exited: false,
        };
        if let Err(error) = self.panes.register_local_pane(descriptor, pane.channels()) {
            let _ = self.control.try_failed(PaneFailed {
                reservation_id: reservation.reservation_id,
                request_id: pending.request_id,
                base_revision: pending.base_revision,
                refused: false,
            });
            self.status = format!("pane registration failed: {error}");
            return Ok(());
        }
        self.provisional
            .insert(pending.request_id, reservation.pane_id);
        self.local.insert(reservation.pane_id, pane);
        // Only when this machine is the one that asked. A terminal somebody
        // else opened here must not move the cursor of whoever is sitting at
        // this keyboard.
        if asked_here {
            if let Some(tab_id) = reservation.tab_id {
                self.tui.select_created_tab(tab_id);
            } else {
                self.tui.select_created_pane(reservation.pane_id);
            }
        }
        match self.control.try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            request_id: pending.request_id,
            base_revision: pending.base_revision,
            author_signature: Vec::new(),
        })? {
            Some(CoordinatorResponse::Commit(commit)) => {
                self.handle_control_event(LayoutControlEvent::Commit(commit))?
            }
            Some(CoordinatorResponse::Reject(reject)) => {
                self.reject_request_with_reason(reject.request_id, reject.reason)
            }
            Some(CoordinatorResponse::Reservation(_)) | None => {}
        }
        // A pane started to run something specific says so in its title, and
        // this machine is the only peer allowed to name it — the layout lets a
        // pane's host rename it and nobody else. That title is what makes
        // pressing Enter on the same agent twice find the pane instead of
        // opening a second one.
        if !pending.command.is_empty() {
            let title = pending
                .title
                .clone()
                .unwrap_or_else(|| crate::tui::home::chat_pane_title(&pending.command));
            self.handle_intent(UiIntent::RenamePane {
                pane_id: reservation.pane_id,
                title,
            })?;
        }
        Ok(())
    }

    /// What to tell the user when the coordinator refused a request.
    fn rejection_notice(&self, reason: i32, request_id: u64) -> String {
        let pending = self
            .pending_create
            .as_ref()
            .filter(|pending| pending.request_id == request_id);
        rejection_sentence(
            reason,
            request_id,
            pending
                .map(|pending| pending.target_name.as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("that machine"),
            pending
                .map(|pending| pending.command.as_slice())
                .unwrap_or_default(),
        )
    }

    pub(in crate::tui) fn reject_request_with_reason(&mut self, request_id: u64, reason: i32) {
        let notice = self.rejection_notice(reason, request_id);
        crate::tui::debug_log::ui_debug_log(
            "layout_request_rejected",
            format_args!("request={request_id} reason={reason} notice={notice:?}"),
        );
        self.reject_request(request_id);
        self.footer_notice = Some(notice.clone());
        // …and as status, which is the only one of the two that survives the
        // trip to an attached client. The node is headless and forwards
        // `status`; a notice that lived in `footer_notice` alone reached nobody
        // running the client/node split, which is to say nobody at all.
        //
        // That is not a cosmetic loss. Ask for a terminal on a machine that
        // refuses and the screen left Home, opened nothing, and said nothing —
        // three sentences written for exactly this moment, none of them ever
        // seen by a user.
        self.status = notice;
    }

    pub(in crate::tui) fn reject_request(&mut self, request_id: u64) {
        self.tui.cancel_resize_drag();
        self.pending_create = self
            .pending_create
            .take()
            .filter(|pending| pending.request_id != request_id);
        if let Some(pane_id) = self.provisional.remove(&request_id) {
            let _ = self.panes.remove_local_pane(pane_id);
            if let Some(mut pane) = self.local.remove(&pane_id) {
                let _ = pane.shutdown();
            }
        }
        if let Some((pane_id, previous)) = self.pending_locks.remove(&request_id)
            && let Some(pane) = self.local.get_mut(&pane_id)
        {
            let _ = pane.set_locked(previous);
        }
        let notice = format!("layout request {request_id} rejected");
        self.footer_notice = Some(notice.clone());
        // Overwritten a moment later by `reject_request_with_reason` when there
        // is a sentence to say instead. Set here too so that a rejection with
        // no reason still reaches an attached client rather than vanishing.
        self.status = notice;
    }

    pub(in crate::tui) fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1).max(1);
        id
    }
}

/// The sentence a refused layout request gets.
///
/// The three that can happen to a terminal asked for on another machine get
/// one each, because each has a different thing to do about it: wake the
/// machine, open its gate, or wait. Everything else keeps the bare line, which
/// is fine for failures the user did not cause and cannot act on.
///
/// Free of the runtime so the words can be tested without a session behind
/// them — they are the whole value of the branch.
fn rejection_sentence(reason: i32, request_id: u64, machine: &str, command: &[String]) -> String {
    match LayoutRejectReason::try_from(reason) {
        Ok(LayoutRejectReason::UnknownTarget) => {
            format!("{machine} is not in this session — start p2pmux on it and it rejoins")
        }
        Ok(LayoutRejectReason::TargetRefused) => {
            // The line the user needs is not that they were refused, which they
            // can see, but the one command that stops it happening again — run
            // *there*, because that is the machine whose answer it is.
            let entry = crate::pairing::work_entry(command);
            let argument = if entry == crate::pairing::SHELL_ENTRY {
                String::new()
            } else {
                format!(" {entry}")
            };
            format!("{machine} refused — run `p2pmux work allow{argument}` on {machine}")
        }
        Ok(LayoutRejectReason::ReservationFailure) => {
            format!("{machine} did not answer in time")
        }
        _ => format!("layout request {request_id} rejected"),
    }
}

#[cfg(test)]
mod tests {
    use super::rejection_sentence;
    use crate::protocol::LayoutRejectReason;

    /// A refusal the user cannot act on is a refusal that gets reported as a
    /// bug. Both shapes name the command, and name the machine to run it on.
    #[test]
    fn a_refusal_names_the_command_that_lifts_it() {
        assert_eq!(
            rejection_sentence(LayoutRejectReason::TargetRefused as i32, 7, "droplet", &[]),
            "droplet refused — run `p2pmux work allow` on droplet",
            "a bare terminal is the shell entry, which is spelled by leaving it off"
        );
        assert_eq!(
            rejection_sentence(
                LayoutRejectReason::TargetRefused as i32,
                7,
                "droplet",
                &[String::from("claude")],
            ),
            "droplet refused — run `p2pmux work allow claude` on droplet"
        );
        // The other two are about the machine, not about its policy.
        assert!(
            rejection_sentence(LayoutRejectReason::UnknownTarget as i32, 7, "droplet", &[])
                .contains("not in this session")
        );
        assert_eq!(
            rejection_sentence(99, 7, "droplet", &[]),
            "layout request 7 rejected"
        );
    }
}
