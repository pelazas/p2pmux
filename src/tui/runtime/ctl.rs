//! Ctl verbs, implemented on the session node rather than the attached TUI.

use std::time::Instant;

use crate::{
    ctl::{
        CtlAgent, CtlMachine, CtlSpawn, agent_id, agent_state_name, event_state_name,
        is_nested_p2pmux,
    },
    layout::PaneId,
    lease::IDLE_AFTER,
    protocol::AgentRosterState,
    tui::{AgentOverlayRow, geometry::contains_leaf, member_label, pane::remote::RemoteInput},
};

use super::SharedLayoutRuntime;

impl SharedLayoutRuntime {
    pub(crate) fn ctl_prepare(&mut self) {
        self.tui.set_local_peer_id(self.control.peer_id());
        let pairing = crate::pairing::load_or_empty();
        let _ = self.tui.set_paired_machines(pairing.machines);
        let rows = self.agent_overlay_rows();
        self.tui.set_agent_rows(rows);
    }

    pub(crate) fn ctl_machines(&mut self) -> Vec<CtlMachine> {
        self.ctl_prepare();
        super::super::home::machine_rows(&self.tui)
            .into_iter()
            .filter(|row| row.owned)
            .map(|row| CtlMachine {
                name: row.name,
                reachable: row.reachable,
                this_machine: row.this_machine,
            })
            .collect()
    }

    pub(crate) fn ctl_agents(&mut self) -> Vec<CtlAgent> {
        self.ctl_prepare();
        let mut rows = self.agent_overlay_rows();
        rows.sort_by(|left, right| {
            super::super::home::home_rank(right.state)
                .cmp(&super::super::home::home_rank(left.state))
                .then_with(|| left.host.cmp(&right.host))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.pane_id.cmp(&right.pane_id))
        });
        rows.into_iter().map(ctl_agent_from_row).collect()
    }

    pub(crate) fn ctl_event_snapshot(&mut self) -> Vec<crate::ctl::CtlToClient> {
        self.ctl_prepare();
        self.agent_overlay_rows()
            .into_iter()
            .filter_map(|row| {
                let state = event_state_name(row.state)?;
                Some(crate::ctl::CtlToClient::Event {
                    id: agent_id(row.pane_id, row.process_pid),
                    pane_id: row.pane_id,
                    kind: row.kind,
                    machine: row.host,
                    state: state.to_owned(),
                    message: row.message,
                })
            })
            .collect()
    }

    pub(crate) fn ctl_try_spawn(
        &mut self,
        machine: &str,
        command: Vec<String>,
        new: bool,
    ) -> Result<CtlSpawn, String> {
        if is_nested_p2pmux(&command) {
            return Err(String::from("p2pmux does not nest p2pmux in a pane"));
        }
        if self.structural_edits_frozen() {
            return Err(String::from(
                "coordinator unreachable; layout changes are paused",
            ));
        }
        self.ctl_prepare();
        let rows = super::super::home::machine_rows(&self.tui);
        let Some(row) = rows.iter().find(|row| row.name == machine) else {
            return Err(format!("no paired machine named {machine}"));
        };
        if !row.owned {
            return Err(format!(
                "{machine} is someone else's machine — p2pmux does not start terminals on it"
            ));
        }
        let Some(peer_id) = row.peer_id.clone() else {
            return Err(format!(
                "{machine} is asleep — start p2pmux on it and it rejoins on its own"
            ));
        };
        if !new && let Some(pane_id) = self.live_chat_pane(machine, &command) {
            return Ok(CtlSpawn::Reused(pane_id));
        }
        if self.pending_create.is_some() {
            return Ok(CtlSpawn::Busy);
        }
        let (grid_rows, grid_cols) = self.ctl_spawn_grid();
        // Watch before the layout call: a local coordinator answers inside
        // `begin_create_on`, clears `pending_create`, and would otherwise look
        // like a spawn that never started.
        let request_id = self.next_request_id;
        self.ctl_watch(request_id);
        if let Err(error) = self.begin_create_on(
            None,
            grid_rows,
            grid_cols,
            None,
            peer_id,
            command,
            machine.to_owned(),
        ) {
            self.ctl_waiting.remove(&request_id);
            self.ctl_results.remove(&request_id);
            return Err(error.to_string());
        }
        Ok(CtlSpawn::Started(request_id))
    }

    pub(crate) fn ctl_watch(&mut self, request_id: u64) {
        self.ctl_waiting.insert(request_id);
    }

    pub(crate) fn ctl_complete(&mut self, request_id: u64, result: Result<u64, String>) {
        if self.ctl_waiting.remove(&request_id) {
            self.ctl_results.insert(request_id, result);
        }
    }

    pub(crate) fn ctl_take_result(&mut self, request_id: u64) -> Option<Result<u64, String>> {
        self.ctl_results.remove(&request_id)
    }

    pub(crate) fn ctl_send(&mut self, pane_id: u64, keys: &str) -> Result<u64, String> {
        if keys.is_empty() {
            return Err(String::from("nothing to send"));
        }
        let Some(pane) = self.tui.snapshot().panes.get(&pane_id).cloned() else {
            return Err(String::from("no pane with that id"));
        };
        if pane.exited {
            return Err(String::from("that pane has exited"));
        }
        if !self.input_allowed(pane_id) {
            return Err(String::from("input is not allowed on that pane"));
        }
        if let Some(local) = self.local.get(&pane_id) {
            let state = local.lease.state();
            if !state.controller_peer_id.is_empty()
                && state.controller_peer_id != local.host_peer_id
                && !state.is_idle_at(Instant::now())
            {
                return Err(String::from("someone else is controlling that pane"));
            }
        }
        if let Some(remote) = self.remote.get(&pane_id)
            && let Some(lease) = remote.lease.as_ref()
        {
            let idle = remote.last_lease.elapsed() >= IDLE_AFTER;
            if crate::tui::pane::remote::remote_input_decision(
                &lease.controller_peer_id,
                remote.pane.controls.peer_id(),
                remote.pending_control,
                remote.held_input.is_empty(),
                idle,
            ) == RemoteInput::Ignore
            {
                return Err(String::from("someone else is controlling that pane"));
            }
        }
        self.node_input(Some(pane_id), keys.as_bytes().to_vec())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| String::from("input is not allowed on that pane"))
    }

    pub(crate) fn ctl_focus(&mut self, agent: &str) -> Result<u64, String> {
        self.ctl_prepare();
        let pane_id = self.resolve_ctl_agent(agent)?;
        let tab_id = self
            .tui
            .snapshot()
            .tabs
            .iter()
            .find(|tab| contains_leaf(&tab.root, pane_id))
            .map(|tab| tab.tab_id)
            .ok_or_else(|| String::from("that pane is not on any tab"))?;
        self.node_focus(tab_id, pane_id)
            .map_err(|error| error.to_string())?;
        Ok(pane_id)
    }

    fn resolve_ctl_agent(&self, agent: &str) -> Result<PaneId, String> {
        if let Some(pid) = agent.strip_prefix("pid:") {
            let pid: u32 = pid
                .parse()
                .map_err(|_| String::from("that is not an agent id"))?;
            let row = self
                .agent_overlay_rows()
                .into_iter()
                .find(|row| row.pane_id == 0 && row.process_pid == pid);
            return match row {
                Some(row) if row.pane_id != 0 => Ok(row.pane_id),
                Some(_) => Err(String::from(
                    "that agent has no pane; `p2pmux ctl spawn` starts one",
                )),
                None => Err(String::from("no agent with that id")),
            };
        }
        let pane_id: PaneId = agent
            .parse()
            .map_err(|_| String::from("that is not an agent id"))?;
        if self.tui.snapshot().panes.contains_key(&pane_id) {
            return Ok(pane_id);
        }
        Err(String::from("no pane with that id"))
    }

    fn live_chat_pane(&self, host: &str, command: &[String]) -> Option<PaneId> {
        if command.is_empty() {
            return None;
        }
        let title = super::super::home::chat_pane_title(command);
        let pane = self.tui.snapshot().panes.values().find(|pane| {
            pane.title.as_deref() == Some(title.as_str())
                && !pane.exited
                && member_label(&pane.host_peer_id, &self.tui.snapshot().members) == host
        })?;
        let state = self
            .agent_overlay_rows()
            .into_iter()
            .find(|row| row.pane_id == pane.pane_id)
            .map(|row| row.state);
        if state.is_some_and(AgentRosterState::is_completion) {
            return None;
        }
        Some(pane.pane_id)
    }

    fn ctl_spawn_grid(&self) -> (u16, u16) {
        self.tui
            .snapshot()
            .panes
            .values()
            .find(|pane| pane.grid_rows > 0 && pane.grid_cols > 0)
            .map(|pane| (pane.grid_rows, pane.grid_cols))
            .unwrap_or((24, 80))
    }
}

fn ctl_agent_from_row(row: AgentOverlayRow) -> CtlAgent {
    CtlAgent {
        id: agent_id(row.pane_id, row.process_pid),
        pane_id: row.pane_id,
        kind: row.kind,
        host: row.host,
        state: agent_state_name(row.state).to_owned(),
        needs_you: row.state.needs_you(),
        cwd: row.cwd,
        message: row.message,
        session: row.session,
        in_another_session: row.in_another_session,
    }
}
