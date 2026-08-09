//! Applying layout state and turning UI intents into coordinator requests.

use std::{collections::BTreeSet, error::Error, io, path::PathBuf};

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
            LayoutControlEvent::Disconnected => self.note_coordinator_lost(),
        }
        Ok(())
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
                snapshot
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
            } => {
                self.begin_create_on(None, grid_rows, grid_cols, None, peer_id, command, name)?;
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
            },
        };
        if pending.request_id == 0 || pending.grid_rows == 0 || pending.grid_cols == 0 {
            self.status = String::from("unexpected pane reservation");
            return Ok(());
        }
        // A pane this machine asked for on itself needs no permission from
        // itself. Everything else is a machine of yours asking, and the answer
        // is given here, by whoever owns this box.
        if !asked_here {
            match crate::pairing::load_or_empty().work_decision(&pending.command) {
                crate::pairing::WorkDecision::Refuse => {
                    let _ = self.control.try_failed(PaneFailed {
                        reservation_id: reservation.reservation_id,
                        request_id: pending.request_id,
                        base_revision: pending.base_revision,
                        refused: true,
                    });
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
        Ok(())
    }

    /// What to tell the user when the coordinator refused a request.
    ///
    /// The three that can happen to a terminal asked for on another machine get
    /// sentences, because each one has a different thing to do about it: wake
    /// the machine, ask its owner, or wait. Everything else keeps the old line,
    /// which is fine for failures the user did not cause and cannot act on.
    fn rejection_notice(&self, reason: i32, request_id: u64) -> String {
        let machine = self
            .pending_create
            .as_ref()
            .filter(|pending| pending.request_id == request_id)
            .map(|pending| pending.target_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| String::from("that machine"));
        match LayoutRejectReason::try_from(reason) {
            Ok(LayoutRejectReason::UnknownTarget) => {
                format!("{machine} is not in this session — start p2pmux on it and it rejoins")
            }
            Ok(LayoutRejectReason::TargetRefused) => {
                format!("{machine} refused: it does not accept work from your machines")
            }
            Ok(LayoutRejectReason::ReservationFailure) => {
                format!("{machine} did not answer in time")
            }
            _ => format!("layout request {request_id} rejected"),
        }
    }

    pub(in crate::tui) fn reject_request_with_reason(&mut self, request_id: u64, reason: i32) {
        let notice = self.rejection_notice(reason, request_id);
        self.reject_request(request_id);
        self.footer_notice = Some(notice);
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
        self.footer_notice = Some(format!("layout request {request_id} rejected"));
    }

    pub(in crate::tui) fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1).max(1);
        id
    }
}
