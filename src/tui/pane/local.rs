//! A PTY this process owns: its screen, its lease, and the agent sampler that
//! watches what is running inside it.

use std::{
    error::Error,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self as sync_mpsc, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use portable_pty::PtySize;
use tokio::sync::{mpsc, watch};

use crate::{
    agent_detect::{
        AgentScan, PaneAgentTracker, ProcessSnapshot, SysinfoSampler, sample_global_snapshot,
    },
    layout::PaneId,
    lease::{LeaseDecision, LeaseManager, LeaseState},
    protocol::{AgentRosterEntry, AgentRosterState},
    pty_host::PtyHost,
    screen::{HostScreen, ScreenFrame, SyncGate},
    session::{HostControlEvent, HostPaneChannels, pane_wire_id},
    tui::{PaneViewState, clock::unix_ms_now},
};

/// Scan cadence while any pane's agent state is being inferred from output
/// timing. Inference has to notice silence promptly, so this is the floor.
pub(in crate::tui) const AGENT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Scan cadence when nothing needs inference — no agents at all, or every agent
/// reporting through a hook. The scan is then only watching for a process to
/// appear or exit, and a full `sysinfo` refresh of every process on the machine
/// once a second to do that is most of this process's idle cost.
pub(in crate::tui) const AGENT_WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// One fixed-grid PTY owned by this process. Its watch channels are registered with the pane
/// service before the layout coordinator is told that the pane is ready.
pub struct SharedLocalPane {
    pub(in crate::tui) pane_id: PaneId,
    pub(in crate::tui) host: PtyHost,
    pub(in crate::tui) screen: HostScreen,
    pub(in crate::tui) lease: LeaseManager,
    pub(in crate::tui) host_peer_id: Vec<u8>,
    pub(in crate::tui) locked: bool,
    pub(in crate::tui) exited: bool,
    pub(in crate::tui) exit_report_pending: bool,
    pub(in crate::tui) screen_tx: watch::Sender<ScreenFrame>,
    pub(in crate::tui) lease_tx: watch::Sender<LeaseState>,
    pub(in crate::tui) control_tx: mpsc::Sender<HostControlEvent>,
    pub(in crate::tui) control_rx: mpsc::Receiver<HostControlEvent>,
    pub(in crate::tui) agent_tracker: PaneAgentTracker,
    pub(in crate::tui) sync_gate: SyncGate,
}
impl SharedLocalPane {
    pub fn spawn(
        pane_id: PaneId,
        grid_rows: u16,
        grid_cols: u16,
        host_peer_id: Vec<u8>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_cwd(pane_id, grid_rows, grid_cols, host_peer_id, None)
    }

    pub(crate) fn spawn_with_cwd(
        pane_id: PaneId,
        grid_rows: u16,
        grid_cols: u16,
        host_peer_id: Vec<u8>,
        cwd: Option<&Path>,
    ) -> Result<Self, Box<dyn Error>> {
        let size = PtySize {
            rows: grid_rows,
            cols: grid_cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cwd = cwd.filter(|path| path.is_dir());
        let host = match PtyHost::spawn_default_shell_with_cwd(size, cwd, Some(pane_id)) {
            Ok(host) => host,
            Err(_) if cwd.is_some_and(|path| !path.is_dir()) => {
                PtyHost::spawn_default_shell_with_cwd(size, None, Some(pane_id))?
            }
            Err(error) => return Err(error),
        };
        let screen = HostScreen::new(grid_rows, grid_cols)?;
        let (screen_tx, _) = watch::channel(screen.current_frame().clone());
        let lease = LeaseManager::new(Vec::new(), Instant::now());
        let (lease_tx, _) = watch::channel(lease.state().clone());
        let (control_tx, control_rx) = mpsc::channel(256);
        Ok(Self {
            pane_id,
            host,
            screen,
            lease,
            host_peer_id,
            locked: false,
            exited: false,
            exit_report_pending: false,
            screen_tx,
            lease_tx,
            control_tx,
            control_rx,
            agent_tracker: PaneAgentTracker::default(),
            sync_gate: SyncGate::default(),
        })
    }

    pub fn channels(&self) -> HostPaneChannels {
        HostPaneChannels {
            pane_id: pane_wire_id(self.pane_id),
            host_peer_id: self.host_peer_id.clone(),
            screen_rx: self.screen_tx.subscribe(),
            lease_rx: self.lease_tx.subscribe(),
            control_tx: self.control_tx.clone(),
        }
    }

    pub(in crate::tui) fn view_state(&self) -> PaneViewState {
        PaneViewState {
            ready: true,
            controller_peer_id: Some(self.lease.state().controller_peer_id.clone()),
            controller_active: !self.lease.state().is_idle_at(Instant::now()),
            scrollback: 0,
        }
    }

    pub(in crate::tui) fn drain(&mut self) -> Result<LocalPaneDrain, Box<dyn Error>> {
        let mut changed = false;
        if !self.exited && self.host.try_wait()? {
            changed |= self.transition_exited()?;
        }
        if let Some(state) = self.lease.clear_if_idle(Instant::now())? {
            self.lease_tx.send_replace(state);
            changed = true;
        }
        while let Ok(event) = self.control_rx.try_recv() {
            if self.exited {
                continue;
            }
            match event {
                HostControlEvent::Input { peer_id, input } => {
                    if self.locked && peer_id != self.host_peer_id {
                        continue;
                    }
                    if let LeaseDecision::AcceptInput(bytes) =
                        self.lease
                            .input(&peer_id, input.lease_epoch, input.data, Instant::now())
                    {
                        if self.host.write_input(&bytes).is_err() {
                            changed |= self.transition_exited()?;
                            continue;
                        }
                        self.lease_tx.send_replace(self.lease.state().clone());
                        changed = true;
                    }
                }
                HostControlEvent::TakeControl { peer_id, request } => {
                    if self.locked && peer_id != self.host_peer_id {
                        continue;
                    }
                    let decision = self.lease.take_control(
                        peer_id,
                        request.known_lease_epoch,
                        Instant::now(),
                    )?;
                    match decision {
                        LeaseDecision::Publish(state) => {
                            self.lease_tx.send_replace(state);
                            changed = true;
                        }
                        // A normal request while the holder is active does not change the lease,
                        // but the requester needs an authoritative re-publication to clear its
                        // pending claim and try again after the idle timeout.
                        LeaseDecision::RejectActiveController => {
                            self.lease_tx.send_replace(self.lease.state().clone());
                            changed = true;
                        }
                        LeaseDecision::AcceptInput(_)
                        | LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest => {}
                    }
                }
                HostControlEvent::ReleaseControl { peer_id } => {
                    if self.lease.state().controller_peer_id == peer_id
                        && let Some(state) = self.lease.clear_controller(Instant::now())?
                    {
                        self.lease_tx.send_replace(state);
                        changed = true;
                    }
                }
            }
        }
        let started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = self.host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        if !pending.is_empty() {
            self.agent_tracker
                .record_output(Instant::now(), unix_ms_now());
        }
        let ready = if pending.is_empty() {
            self.sync_gate.flush_stale(Instant::now())
        } else {
            self.sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            // One parse/snapshot/diff for the whole batch: process_pty clones the
            // screen per call, so per-chunk calls dominated CPU under output floods.
            let frame = self.screen.process_pty(&ready)?;
            // Read after parsing: the bell is a parser callback, so the count only reflects
            // this batch once the batch has been fed through.
            if self.screen.take_bell_count() > 0 {
                self.agent_tracker.record_completion_signal(Instant::now());
            }
            if let Some(reply) = self.screen.take_kitty_keyboard_query_reply()
                && self.host.write_input(&reply).is_err()
            {
                changed |= self.transition_exited()?;
            } else {
                self.screen_tx.send_replace(frame);
                changed = true;
            }
        }
        if !self.exited && self.host.output_closed() {
            changed |= self.transition_exited()?;
        }
        Ok(LocalPaneDrain {
            changed,
            newly_exited: std::mem::take(&mut self.exit_report_pending),
        })
    }

    pub(in crate::tui) fn apply_agent_snapshot(
        &mut self,
        scan: &AgentScan<'_>,
        now: Instant,
    ) -> bool {
        let unix_ms_now = unix_ms_now();
        let before = self.agent_tracker.listed_agent(now, unix_ms_now);
        let session_child = self.host.process_id();
        let detected = session_child.and_then(|pid| scan.classify(pid));
        self.agent_tracker.update(detected, now, unix_ms_now);
        self.agent_tracker
            .observe_pane_liveness(session_child.is_some_and(|pid| scan.has_children(pid)), now);
        self.agent_tracker.listed_agent(now, unix_ms_now) != before
    }

    /// Whether this pane's reported state comes from output-timing inference
    /// rather than a producer inside it — the only case that needs the fast
    /// sampler cadence, since only inference has to notice silence promptly.
    pub(in crate::tui) fn agent_state_is_inferred(&self) -> bool {
        self.agent_tracker.active_agent.is_some() && !self.agent_tracker.has_owning_push()
    }

    pub(in crate::tui) fn agent_roster_entry(&mut self, now: Instant) -> Option<AgentRosterEntry> {
        let (agent, state) = self.agent_tracker.listed_agent(now, unix_ms_now())?;
        Some(AgentRosterEntry {
            pane_id: self.pane_id,
            agent_kind: agent.kind.wire_value().into(),
            cwd: agent.cwd,
            state: match state {
                crate::agent_detect::AgentState::Idle => AgentRosterState::Idle as i32,
                crate::agent_detect::AgentState::Working => AgentRosterState::Working as i32,
                crate::agent_detect::AgentState::Done => AgentRosterState::Done as i32,
                crate::agent_detect::AgentState::Pending => AgentRosterState::Pending as i32,
                crate::agent_detect::AgentState::Error => AgentRosterState::Error as i32,
            },
            working_since_unix_ms: if state == crate::agent_detect::AgentState::Working {
                self.agent_tracker.reported_working_since_unix_ms()
            } else {
                0
            },
        })
    }

    pub(in crate::tui) fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        if self.exited {
            return Ok(());
        }
        let epoch = self.lease.state().epoch;
        let decision = self
            .lease
            .input(&self.host_peer_id, epoch, bytes, Instant::now());
        match decision {
            LeaseDecision::AcceptInput(bytes) => {
                if self.host.write_input(&bytes).is_err() {
                    let _ = self.transition_exited()?;
                }
                self.lease_tx.send_replace(self.lease.state().clone());
            }
            LeaseDecision::Publish(_) => {}
            LeaseDecision::RejectStaleInput
            | LeaseDecision::RejectStaleRequest
            | LeaseDecision::RejectActiveController => {}
        }
        Ok(())
    }

    pub(in crate::tui) fn release_controller(
        &mut self,
        peer_id: &[u8],
    ) -> Result<bool, Box<dyn Error>> {
        if self.lease.state().controller_peer_id != peer_id {
            return Ok(false);
        }
        let Some(state) = self.lease.clear_controller(Instant::now())? else {
            return Ok(false);
        };
        self.lease_tx.send_replace(state);
        Ok(true)
    }

    pub(in crate::tui) fn set_locked(&mut self, locked: bool) -> Result<bool, Box<dyn Error>> {
        if self.locked == locked {
            return Ok(false);
        }
        self.locked = locked;
        if locked
            && self.lease.state().controller_peer_id != self.host_peer_id
            && let Some(state) = self.lease.clear_controller(Instant::now())?
        {
            self.lease_tx.send_replace(state);
        }
        Ok(true)
    }

    pub(in crate::tui) fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Box<dyn Error>> {
        if self.exited {
            return Ok(());
        }
        if self.screen.screen().size() == (rows, cols) {
            return Ok(());
        }
        self.host.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let frame = self.screen.resize(rows, cols)?;
        self.screen_tx.send_replace(frame);
        Ok(())
    }

    pub(in crate::tui) fn shutdown(&mut self) -> Result<(), Box<dyn Error>> {
        self.host.shutdown()
    }

    pub(in crate::tui) fn transition_exited(&mut self) -> Result<bool, Box<dyn Error>> {
        if self.exited {
            return Ok(false);
        }
        self.exited = true;
        self.exit_report_pending = true;
        while self.control_rx.try_recv().is_ok() {}
        if let Some(state) = self.lease.clear_controller(Instant::now())? {
            self.lease_tx.send_replace(state);
        }
        Ok(true)
    }

    pub(in crate::tui) fn mark_exited(&mut self) -> Result<(), Box<dyn Error>> {
        self.transition_exited()?;
        self.exit_report_pending = false;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::tui) struct LocalPaneDrain {
    pub(in crate::tui) changed: bool,
    pub(in crate::tui) newly_exited: bool,
}
pub(in crate::tui) struct AgentSamplingWorker {
    pub(in crate::tui) snapshots: Receiver<Vec<ProcessSnapshot>>,
    pub(in crate::tui) stop: Arc<AtomicBool>,
    pub(in crate::tui) interval_ms: Arc<AtomicU64>,
    pub(in crate::tui) join: Option<JoinHandle<()>>,
}
impl AgentSamplingWorker {
    pub(in crate::tui) fn spawn() -> Self {
        let (snapshot_tx, snapshots) = sync_mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let interval_ms = Arc::new(AtomicU64::new(AGENT_SAMPLE_INTERVAL.as_millis() as u64));
        let worker_stop = stop.clone();
        let worker_interval = interval_ms.clone();
        let join = thread::spawn(move || {
            let mut sampler = SysinfoSampler::default();
            while !worker_stop.load(Ordering::Relaxed) {
                if snapshot_tx
                    .send(sample_global_snapshot(&mut sampler))
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(
                    worker_interval.load(Ordering::Relaxed),
                ));
            }
        });
        Self {
            snapshots,
            stop,
            interval_ms,
            join: Some(join),
        }
    }

    /// Set how often the global process scan runs.
    ///
    /// The scan is the single most expensive thing this process does when
    /// nothing is happening — a full `sysinfo` refresh of every process on the
    /// machine, with exe, cmdline and cwd. It only needs the fast cadence when
    /// a pane's state is being *inferred* from output timing, which is the one
    /// case that depends on noticing silence promptly. Panes whose agent
    /// reports through a hook, and panes with no agent at all, only need the
    /// scan for liveness and for spotting a new launch.
    pub(in crate::tui) fn set_interval(&self, interval: Duration) {
        self.interval_ms
            .store(interval.as_millis() as u64, Ordering::Relaxed);
    }

    pub(in crate::tui) fn latest_snapshot(&self) -> Option<Vec<ProcessSnapshot>> {
        let mut latest = None;
        while let Ok(snapshot) = self.snapshots.try_recv() {
            latest = Some(snapshot);
        }
        latest
    }

    pub(in crate::tui) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use crate::{
        lease::{IDLE_AFTER, LeaseManager},
        session::{HostControlEvent, pane_wire_id},
    };

    use super::SharedLocalPane;

    #[test]
    fn rejected_busy_claim_republishes_lease_and_idle_claim_succeeds() {
        let owner = b"owner".to_vec();
        let requester = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, owner.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(owner.clone(), Instant::now());
        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: requester.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: requester.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("busy takeover event");
        pane.drain().expect("drain busy takeover");
        assert!(lease_rx.has_changed().expect("lease watch"));
        assert_eq!(lease_rx.borrow_and_update().controller_peer_id, owner);

        pane.lease = LeaseManager::with_epoch_for_test(
            pane.host_peer_id.clone(),
            1,
            Instant::now() - IDLE_AFTER,
        );
        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: requester.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: requester.clone(),
                    known_lease_epoch: 2,
                },
            })
            .expect("idle takeover event");
        pane.drain().expect("drain idle takeover");
        assert_eq!(pane.lease.state().controller_peer_id, requester);
        assert_eq!(pane.lease.state().epoch, 3);
    }

    #[test]
    fn idle_controller_is_cleared_and_published_to_watchers() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::with_epoch_for_test(host_id, 1, Instant::now() - IDLE_AFTER);

        pane.drain().expect("drain idle controller");

        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert!(lease.controller_peer_id.is_empty());
        assert_eq!(lease.epoch, 2);
    }

    #[test]
    fn exited_local_pane_clears_lease_and_rejects_all_controls() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(host_id.clone(), Instant::now());
        assert!(pane.transition_exited().expect("exit transition"));
        assert!(pane.exited);
        assert!(pane.lease.state().controller_peer_id.is_empty());
        assert!(lease_rx.has_changed().expect("lease publication"));

        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: host_id,
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: b"host".to_vec(),
                    known_lease_epoch: pane.lease.state().epoch,
                },
            })
            .expect("queued control");
        let drained = pane.drain().expect("drain exited pane");
        assert!(drained.newly_exited);
        assert!(pane.lease.state().controller_peer_id.is_empty());
        pane.shutdown().expect("shutdown exited pane");
    }

    #[test]
    fn locked_local_pane_rejects_guest_control_and_accepts_host_control() {
        let host_id = b"host".to_vec();
        let guest_id = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        pane.set_locked(true).expect("lock pane");

        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: guest_id.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: guest_id.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("guest takeover event");
        pane.drain().expect("drain guest takeover");
        assert!(pane.lease.state().controller_peer_id.is_empty());

        pane.control_tx
            .try_send(HostControlEvent::Input {
                peer_id: guest_id.clone(),
                input: crate::protocol::Input {
                    pane_id: pane_wire_id(99),
                    lease_epoch: 1,
                    data: b"blocked".to_vec(),
                },
            })
            .expect("guest input event");
        pane.drain().expect("drain guest input");
        assert!(pane.lease.state().controller_peer_id.is_empty());

        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: host_id.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: host_id.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("host takeover event");
        pane.drain().expect("drain host takeover");
        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn locking_local_pane_clears_guest_lease_but_keeps_host_lease() {
        let host_id = b"host".to_vec();
        let guest_id = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(guest_id, Instant::now());

        pane.set_locked(true).expect("lock pane");
        assert!(lease_rx.has_changed().expect("lease watch"));
        assert!(lease_rx.borrow_and_update().controller_peer_id.is_empty());

        pane.lease = LeaseManager::new(host_id.clone(), Instant::now());
        pane.set_locked(false).expect("unlock pane");
        pane.set_locked(true).expect("relock pane");
        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn remote_release_clears_the_host_lease_immediately() {
        let host_id = b"host".to_vec();
        let guest_id = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(guest_id.clone(), Instant::now());
        pane.control_tx
            .try_send(HostControlEvent::ReleaseControl { peer_id: guest_id })
            .expect("remote release event");

        pane.drain().expect("drain remote release");

        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert!(lease.controller_peer_id.is_empty());
        assert_eq!(lease.epoch, 2);

        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn local_input_crossing_the_idle_timeout_reclaims_and_delivers_it() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease =
            LeaseManager::with_epoch_for_test(b"remote".to_vec(), 1, Instant::now() - IDLE_AFTER);

        pane.input(b"printf boundary-input-delivered\\n".to_vec())
            .expect("first input at idle boundary");

        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        assert_eq!(pane.lease.state().epoch, 3);
        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert_eq!(lease.controller_peer_id, b"host");
        assert_eq!(lease.epoch, 3);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            while let Some(bytes) = pane.host.try_read_output().expect("PTY reader") {
                output.extend(bytes);
            }
            if String::from_utf8_lossy(&output).contains("boundary-input-delivered") {
                pane.shutdown().expect("shutdown local pane");
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pane.shutdown().expect("shutdown local pane");
        panic!(
            "first input was not delivered to the PTY: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn new_local_pane_starts_free_while_the_host_retains_pty_ownership() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");

        assert!(pane.lease.state().controller_peer_id.is_empty());
        assert_eq!(pane.host_peer_id, host_id);

        pane.shutdown().expect("shutdown local pane");
    }

    #[test]
    fn local_pane_resize_publishes_a_replacement_screen_frame() {
        let mut pane = SharedLocalPane::spawn(99, 1, 1, b"host".to_vec()).expect("pane");
        let before = pane.screen.current_frame().sequence;
        pane.resize(3, 4).expect("resize");
        assert_eq!(pane.screen.screen().size(), (3, 4));
        assert!(pane.screen.current_frame().sequence > before);
        assert_eq!(pane.screen.current_frame().base_sequence, 0);
        pane.shutdown().expect("shutdown");
    }
}
