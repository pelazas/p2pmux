//! The blocking terminal runtime that owns a shared layout: its panes, its
//! control channel, and the loop that drives them.
//!
//! The inherent impl is split by concern across this module's files.

mod failover;
mod forward;
mod layout;
mod node;
mod run;

use std::{
    collections::BTreeMap,
    error::Error,
    io,
    time::{Duration, Instant},
};

use iroh::EndpointAddr;

use crate::{
    layout::{LayoutSnapshot, PaneId},
    protocol::{AgentRoster, AgentRosterEntry, PaneDescriptor, Presence},
    session::{
        GuestPane, PaneLayoutReconciler, PaneServer, SharedLayoutHost, SharedLayoutMember,
        layout_snapshot_from_state,
    },
    transport::Transport,
    tui::{
        Invite, MultiPaneTui,
        pane::{
            control::{PendingCreate, RemoteSubscriptionState, SharedControl},
            local::{AgentSamplingWorker, SharedLocalPane},
            remote::SharedRemotePane,
        },
    },
};

/// Blocking terminal runtime for the shared layout. Network tasks keep streams independent while
/// this loop only drains ready channels and renders the current fixed grids.
pub struct SharedLayoutRuntime {
    pub(in crate::tui) tui: MultiPaneTui,
    pub(in crate::tui) control: SharedControl,
    pub(in crate::tui) panes: PaneServer,
    pub(in crate::tui) reconciler: PaneLayoutReconciler,
    pub(in crate::tui) transport: Transport,
    pub(in crate::tui) session_id: Vec<u8>,
    pub(in crate::tui) runtime: tokio::runtime::Handle,
    pub(in crate::tui) local: BTreeMap<PaneId, SharedLocalPane>,
    pub(in crate::tui) remote: BTreeMap<PaneId, SharedRemotePane>,
    pub(in crate::tui) remote_descriptors: BTreeMap<PaneId, (EndpointAddr, PaneDescriptor)>,
    pub(in crate::tui) subscriptions: RemoteSubscriptionState,
    pub(in crate::tui) retry_tick: u64,
    pub(in crate::tui) subscription_tx:
        tokio::sync::mpsc::UnboundedSender<(PaneId, Result<GuestPane, String>)>,
    pub(in crate::tui) subscription_rx:
        tokio::sync::mpsc::UnboundedReceiver<(PaneId, Result<GuestPane, String>)>,
    pub(in crate::tui) pending_create: Option<PendingCreate>,
    pub(in crate::tui) provisional: BTreeMap<u64, PaneId>,
    pub(in crate::tui) pending_locks: BTreeMap<u64, (PaneId, bool)>,
    pub(in crate::tui) pending_exits: BTreeMap<PaneId, u64>,
    pub(in crate::tui) next_request_id: u64,
    pub(in crate::tui) status: String,
    pub(in crate::tui) copied_lines: Option<usize>,
    pub(in crate::tui) footer_notice: Option<String>,
    pub(in crate::tui) invite: Invite,
    pub(in crate::tui) share_notice: Option<String>,
    pub(in crate::tui) agent_sampler: AgentSamplingWorker,
    pub(in crate::tui) agent_rosters: BTreeMap<Vec<u8>, AgentRoster>,
    pub(in crate::tui) agent_roster_generation: u64,
    pub(in crate::tui) last_local_agent_entries: Vec<AgentRosterEntry>,
    pub(in crate::tui) next_agent_roster_heartbeat: Instant,
    pub(in crate::tui) last_agent_overlay_animation: Instant,
    pub(in crate::tui) presence_generation: u64,
    pub(in crate::tui) last_local_presence: Option<Presence>,
    pub(in crate::tui) seen_presence_epoch: u64,
    /// Highest roster generation this renderer has already applied, per host. Only the
    /// coordinator advances it: a member is handed rosters by the broadcast instead.
    pub(in crate::tui) seen_agent_generations: BTreeMap<Vec<u8>, u64>,
    pub(in crate::tui) presence: Vec<Presence>,
    /// When the coordinator stopped answering, and `None` while it is answering.
    ///
    /// A coordinator sets this too, for the mirror-image case: one whose network dropped
    /// cannot tell that from every member leaving at once, and both are the same state --
    /// this node is alone and the session may have moved on without it.
    pub(in crate::tui) coordinator_lost_at: Option<Instant>,
    pub(in crate::tui) grace: Duration,
    /// The endpoint's accept loop, which serves joins on a coordinator and pane
    /// subscriptions on a member. Held here because a promotion has to swap it.
    pub(in crate::tui) accept_task:
        Option<tokio::task::JoinHandle<Result<(), crate::session::SessionError>>>,
    pub(in crate::tui) rejoin_tx: tokio::sync::mpsc::UnboundedSender<failover::Rejoin>,
    pub(in crate::tui) rejoin_rx: tokio::sync::mpsc::UnboundedReceiver<failover::Rejoin>,
    pub(in crate::tui) rejoin_in_flight: bool,
    pub(in crate::tui) rejoin_deadline: Option<Instant>,
    pub(in crate::tui) rejoin_cursor: usize,
    /// A short code this runtime published for itself, after taking the role over.
    ///
    /// The one a session starts with belongs to whoever created it; this is the replacement,
    /// and holding it here is what lets it be retired when this node stops.
    pub(in crate::tui) published_code: Option<crate::hosted_rendezvous::PublishedCode>,
    pub(in crate::tui) code_tx:
        tokio::sync::mpsc::UnboundedSender<crate::hosted_rendezvous::PublishedCode>,
    pub(in crate::tui) code_rx:
        tokio::sync::mpsc::UnboundedReceiver<crate::hosted_rendezvous::PublishedCode>,
    /// A role change the durable session record has not caught up with yet.
    ///
    /// `p2pmux ls` and `p2pmux ticket <name>` read that record out of process, so a takeover
    /// that never reached it leaves the machine advertising a ticket for an endpoint that
    /// stopped answering and a role that is no longer true.
    pub(in crate::tui) pending_role_persist: Option<RolePersist>,
}

/// What the durable session record has to be updated to after a role change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolePersist {
    pub coordinating: bool,
    pub ticket: Option<String>,
    pub join_code: Option<String>,
}
impl SharedLayoutRuntime {
    pub fn host(
        host: SharedLayoutHost,
        panes: PaneServer,
        snapshot: LayoutSnapshot,
        initial: SharedLocalPane,
        ticket: String,
        code: Option<String>,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = host.transport();
        Self::new(
            SharedControl::Host(host),
            panes,
            transport,
            snapshot,
            Some(initial),
            Invite {
                ticket: Some(ticket),
                code,
            },
            runtime,
        )
    }

    pub fn member(
        member: SharedLayoutMember,
        panes: PaneServer,
        session_id: Vec<u8>,
        snapshot: LayoutSnapshot,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = member.transport();
        let mut value = Self::new(
            SharedControl::Member(member),
            panes,
            transport,
            snapshot,
            None,
            Invite::default(),
            runtime,
        )?;
        value.session_id = session_id;
        Ok(value)
    }

    /// Builds a member runtime from the first authoritative snapshot. Applying the state before
    /// entering raw-terminal mode both establishes the direct-pane admission roster and starts
    /// nonblocking subscriptions for panes hosted by other members.
    pub fn member_from_state(
        member: SharedLayoutMember,
        panes: PaneServer,
        session_id: Vec<u8>,
        state: crate::protocol::LayoutState,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let snapshot = layout_snapshot_from_state(&state)
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let mut value = Self::member(member, panes, session_id, snapshot, runtime)?;
        value.apply_layout_state(&state)?;
        Ok(value)
    }

    pub(in crate::tui) fn new(
        control: SharedControl,
        panes: PaneServer,
        transport: Transport,
        snapshot: LayoutSnapshot,
        initial: Option<SharedLocalPane>,
        invite: Invite,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let (subscription_tx, subscription_rx) = tokio::sync::mpsc::unbounded_channel();
        let (rejoin_tx, rejoin_rx) = tokio::sync::mpsc::unbounded_channel();
        let (code_tx, code_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut local = BTreeMap::new();
        if let Some(initial) = initial {
            local.insert(initial.pane_id, initial);
        }
        let session_id = Vec::new();
        let reconciler = PaneLayoutReconciler::new(panes.clone());
        let mut value = Self {
            tui: MultiPaneTui::new(snapshot)
                .map_err(|error| io::Error::other(format!("invalid layout: {error:?}")))?,
            control,
            panes,
            reconciler,
            transport,
            session_id,
            runtime,
            local,
            remote: BTreeMap::new(),
            remote_descriptors: BTreeMap::new(),
            subscriptions: RemoteSubscriptionState::default(),
            retry_tick: 0,
            subscription_tx,
            subscription_rx,
            pending_create: None,
            provisional: BTreeMap::new(),
            pending_locks: BTreeMap::new(),
            pending_exits: BTreeMap::new(),
            next_request_id: 1,
            status: String::new(),
            copied_lines: None,
            footer_notice: None,
            invite,
            share_notice: None,
            agent_sampler: AgentSamplingWorker::spawn(),
            agent_rosters: BTreeMap::new(),
            agent_roster_generation: 0,
            last_local_agent_entries: Vec::new(),
            next_agent_roster_heartbeat: Instant::now(),
            last_agent_overlay_animation: Instant::now(),
            presence_generation: 0,
            last_local_presence: None,
            seen_presence_epoch: 0,
            seen_agent_generations: BTreeMap::new(),
            presence: Vec::new(),
            coordinator_lost_at: None,
            grace: crate::failover::DEFAULT_GRACE,
            accept_task: None,
            rejoin_tx,
            rejoin_rx,
            rejoin_in_flight: false,
            rejoin_deadline: None,
            rejoin_cursor: 0,
            published_code: None,
            code_tx,
            code_rx,
            pending_role_persist: None,
        };
        value.refresh_local_views();
        Ok(value)
    }

    pub fn set_session_id(&mut self, session_id: Vec<u8>) {
        self.session_id = session_id;
    }

    /// Hand over the endpoint's accept loop, which a promotion has to be able to replace.
    ///
    /// Owned here rather than by the caller that spawned it because the runtime is where the
    /// role changes: a member serving only pane subscriptions becomes a coordinator that must
    /// also answer joins, and the two loops cannot both be reading the same endpoint.
    pub fn set_accept_task(
        &mut self,
        task: tokio::task::JoinHandle<Result<(), crate::session::SessionError>>,
    ) {
        if let Some(previous) = self.accept_task.replace(task) {
            previous.abort();
        }
    }

    /// Shorten the wait before a missing coordinator is replaced. For tests.
    pub fn set_failover_grace(&mut self, grace: Duration) {
        self.grace = grace;
    }

    /// Whether this node currently serializes the session.
    ///
    /// Read rather than remembered: after a takeover or a step-down the role on the record
    /// this node started with is stale, and the attached client draws it every frame.
    pub fn is_coordinating(&self) -> bool {
        matches!(self.control, SharedControl::Host(_))
    }

    /// A role change the durable session record still has to be told about, once.
    pub fn take_role_persist(&mut self) -> Option<RolePersist> {
        self.pending_role_persist.take()
    }

    pub fn local_focus(&self) -> (u64, u64) {
        (self.tui.current_tab(), self.tui.focused_pane())
    }

    pub fn local_peer_id(&self) -> Vec<u8> {
        self.control.peer_id()
    }

    pub(crate) fn share_ticket(&self) -> Option<&str> {
        self.invite.ticket.as_deref()
    }

    pub(crate) fn share_code(&self) -> Option<&str> {
        self.invite.code.as_deref()
    }

    /// Current operator-facing status, empty when there is nothing to report.
    ///
    /// When the runtime drives its own terminal this is drawn directly, but under the
    /// node+client split the runtime is headless, so the node has to forward this to the
    /// attached client or the user never learns about a lost coordinator or a retrying
    /// pane.
    pub(crate) fn status(&self) -> &str {
        &self.status
    }

    /// Which network path each connected peer is actually using, and how far away it is.
    ///
    /// Travels the same headless route as `status`: the runtime holds the transport, so
    /// only it can see this, and only the client can draw it.
    pub(crate) fn peer_paths(&self) -> Vec<crate::transport::PeerPath> {
        self.transport.paths()
    }

    /// Whether this session is currently refusing new peers.
    ///
    /// Only the coordinator holds the answer, so a guest reports `false`: its own client
    /// learns the real state from the layout it is shown, not from here.
    pub(crate) fn session_locked(&self) -> bool {
        match &self.control {
            SharedControl::Host(host) => host.is_session_locked().unwrap_or(false),
            SharedControl::Member(_) => self.tui.session_locked(),
        }
    }

    pub(in crate::tui) fn exited_footer_notice(&self) -> Option<&'static str> {
        let pane = self.tui.snapshot().panes.get(&self.tui.focused_pane())?;
        if !pane.exited {
            return None;
        }
        if pane.host_peer_id == self.control.peer_id() {
            Some("exited — close with Ctrl+P, X")
        } else {
            Some("exited — input disabled; pane host can close with Ctrl+P, X")
        }
    }

    pub(in crate::tui) fn input_allowed(&self, pane_id: PaneId) -> bool {
        let peer_id = self.control.peer_id();
        self.tui
            .snapshot()
            .panes
            .get(&pane_id)
            .is_none_or(|pane| !pane.exited && (!pane.locked || pane.host_peer_id == peer_id))
    }

    pub(in crate::tui) fn shutdown(mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
        if let Some(published) = self.published_code.take() {
            self.runtime.block_on(published.retire());
        }
        self.agent_sampler.shutdown();
        for (_, mut pane) in std::mem::take(&mut self.local) {
            let _ = self.panes.remove_local_pane(pane.pane_id);
            let _ = pane.shutdown();
        }
        for (_, pane) in std::mem::take(&mut self.remote) {
            self.runtime.block_on(pane.pane.shutdown());
        }
        self.runtime.block_on(self.control.shutdown());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        net::Ipv4Addr,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use ratatui::layout::Rect;
    use tokio::sync::{mpsc, watch};

    use crate::{
        agent_detect::cwd_for_pid,
        layout::{Axis, NewPanePosition},
        lease::{LeaseManager, LeaseState},
        protocol::PaneDescriptor,
        screen::HostScreen,
        session::{
            HostPaneChannels, HostSession, LayoutControlEvent, SharedLayoutHost,
            layout_snapshot_from_state, pane_wire_id,
        },
        transport::{ALPN, Transport},
        tui::{UiIntent, pane::local::SharedLocalPane},
    };

    use super::SharedLayoutRuntime;

    async fn loopback_transport() -> Transport {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("localhost address")
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .expect("loopback endpoint");
        Transport::from_endpoint(endpoint)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pushed_agent_status_is_confined_to_panes_this_node_hosts() {
        use crate::agent_detect::{AgentKind, AgentState};
        use crate::protocol::MAX_AGENT_CWD_BYTES;

        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            2,
            8,
        )
        .expect("shared host");
        let pane_server = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let initial = SharedLocalPane::spawn(1, 2, 8, host_id.clone()).expect("initial pty");
        pane_server
            .register_local_pane(
                PaneDescriptor {
                    pane_id: 1,
                    host_peer_id: host_id,
                    grid_rows: 2,
                    grid_cols: 8,
                    title: None,
                    locked: false,
                    exited: false,
                },
                initial.channels(),
            )
            .expect("initial pane registered");
        let state = host
            .session_snapshot()
            .expect("snapshot")
            .state
            .expect("layout state");
        let snapshot = layout_snapshot_from_state(&state).expect("render layout");
        let mut runtime = SharedLayoutRuntime::host(
            host,
            pane_server,
            snapshot,
            initial,
            String::from("TESTCODE"),
            None,
            tokio::runtime::Handle::current(),
        )
        .expect("runtime");

        assert!(
            runtime.apply_agent_status(1, "claude", "pending", "/repo", ""),
            "a producer may report for a pane this node hosts"
        );
        assert_eq!(
            runtime
                .local
                .get_mut(&1)
                .expect("local pane")
                .agent_tracker
                .listed_agent()
                .map(|listed| (listed.agent.kind, listed.state)),
            Some((AgentKind::Claude, AgentState::Pending))
        );

        // And the overlay actually gets a row for it. The tracker holding the
        // status is not the same thing as the overlay showing it: the roster
        // has to be published and read back before anything reaches the screen.
        runtime.publish_local_agent_roster();
        let rows = runtime.agent_overlay_rows();
        assert_eq!(rows.len(), 1, "the pushed status reached the overlay");
        assert_eq!(rows[0].pane_id, 1);
        assert_eq!(rows[0].state, crate::protocol::AgentRosterState::Pending);

        // A pane id this node does not host is refused outright. This is the
        // local half of `Coordinator::accept_agent_roster`: without it, any
        // process on this machine could publish status for a peer's pane under
        // this node's authenticated peer id.
        assert!(
            !runtime.apply_agent_status(99, "claude", "pending", "/repo", ""),
            "a producer may not report for a pane hosted elsewhere"
        );

        // Unparseable kinds and statuses are refused rather than coerced.
        assert!(!runtime.apply_agent_status(1, "gemini", "pending", "/repo", ""));
        assert!(!runtime.apply_agent_status(1, "claude", "pendign", "/repo", ""));

        // An over-long cwd is cut at intake: letting it through would fail
        // `validate_agent_roster` and drop this host's entire roster.
        assert!(runtime.apply_agent_status(
            1,
            "claude",
            "working",
            &"/x".repeat(MAX_AGENT_CWD_BYTES),
            ""
        ));
        let cwd = runtime
            .local
            .get(&1)
            .and_then(|pane| pane.agent_tracker.pushed.as_ref())
            .map(|pushed| pushed.cwd.clone())
            .expect("pushed status");
        assert!(cwd.len() <= MAX_AGENT_CWD_BYTES);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_create_then_committed_delete_updates_its_local_pane_lifecycle() {
        let directory = std::env::temp_dir().join(format!(
            "p2pmux-runtime-create-cwd-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        fs::create_dir(&directory).expect("create temporary directory");
        let expected_cwd = fs::canonicalize(&directory).expect("canonicalize temporary directory");
        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            2,
            8,
        )
        .expect("shared host");
        let pane_server = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let initial = SharedLocalPane::spawn(1, 2, 8, host_id.clone()).expect("initial pty");
        pane_server
            .register_local_pane(
                PaneDescriptor {
                    pane_id: 1,
                    host_peer_id: host_id,
                    grid_rows: 2,
                    grid_cols: 8,
                    title: None,
                    locked: false,
                    exited: false,
                },
                initial.channels(),
            )
            .expect("initial pane registered");
        let state = host
            .session_snapshot()
            .expect("snapshot")
            .state
            .expect("layout state");
        let snapshot = layout_snapshot_from_state(&state).expect("render layout");
        let mut runtime = SharedLayoutRuntime::host(
            host,
            pane_server,
            snapshot,
            initial,
            String::from("TESTCODE"),
            None,
            tokio::runtime::Handle::current(),
        )
        .expect("runtime");
        runtime.set_session_id(b"session".to_vec());

        thread::sleep(Duration::from_secs(2));
        let source_pid = runtime
            .local
            .get(&1)
            .and_then(|pane| pane.host.process_id())
            .expect("source PTY child PID");
        runtime
            .local
            .get_mut(&1)
            .expect("source local pane")
            .host
            .write_input(format!("cd -- {}\n", directory.display()).as_bytes())
            .expect("change source PTY directory");
        // A real shell has to boot and process the `cd` before its cwd moves. Half a
        // second is enough on an idle Mac and not enough on a loaded CI runner, which
        // made this fail for reasons that had nothing to do with pane lifecycle.
        let source_cwd = (0..200).find_map(|_| {
            let cwd = cwd_for_pid(source_pid);
            if cwd
                .as_ref()
                .and_then(|cwd| fs::canonicalize(cwd).ok())
                .as_ref()
                == Some(&expected_cwd)
            {
                cwd
            } else {
                thread::sleep(Duration::from_millis(25));
                None
            }
        });
        assert!(source_cwd.is_some(), "source PTY changed directory");

        runtime
            .handle_intent(UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::LeftRight,
                position: NewPanePosition::Second,
                grid_rows: 2,
                grid_cols: 8,
            })
            .expect("create intent commits after registering a local pane");
        assert!(runtime.local.contains_key(&2));
        let created_pid = runtime
            .local
            .get(&2)
            .and_then(|pane| pane.host.process_id())
            .expect("created PTY child PID");
        assert_eq!(
            cwd_for_pid(created_pid)
                .as_ref()
                .and_then(|cwd| fs::canonicalize(cwd).ok()),
            Some(expected_cwd.clone())
        );
        assert!(runtime.panes.has_registered_pane(2).expect("pane registry"));
        assert!(
            runtime.provisional.is_empty(),
            "committed panes clear provisional state"
        );
        assert_eq!(runtime.tui.snapshot().panes.len(), 2);
        assert_eq!(runtime.tui.focused_pane(), 2);

        runtime.footer_notice = Some(String::from("layout request 5 rejected"));
        let peer_id = runtime.control.peer_id();
        let pane = runtime.local.get_mut(&2).expect("created local pane");
        pane.lease = LeaseManager::new(peer_id, Instant::now());
        let mut lease_rx = pane.lease_tx.subscribe();
        assert!(
            !runtime
                .handle_key(
                    KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
                    Rect::new(0, 0, 80, 24),
                )
                .expect("focus change")
        );
        assert_eq!(runtime.footer_notice, None);
        assert!(lease_rx.has_changed().expect("lease published immediately"));
        assert!(lease_rx.borrow_and_update().controller_peer_id.is_empty());

        runtime
            .handle_intent(UiIntent::DeletePane { pane_id: 2 })
            .expect("host-owned deletion commits");
        assert!(!runtime.local.contains_key(&2));
        assert!(
            !runtime.panes.has_registered_pane(2).expect("pane registry"),
            "committed removal revokes the direct-pane service before the PTY is shut down"
        );
        assert_eq!(runtime.tui.snapshot().panes.len(), 1);
        drop(runtime);
        fs::remove_dir(&directory).expect("remove temporary directory");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn member_runtime_reconciles_snapshot_into_a_direct_remote_pane_attachment() {
        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            1,
            1,
        )
        .expect("shared host");
        let host_panes = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let screen = HostScreen::new(1, 1).expect("screen");
        let (_screen_tx, screen_rx) = watch::channel(screen.current_frame().clone());
        let (_lease_tx, lease_rx) = watch::channel(LeaseState {
            controller_peer_id: host_id.clone(),
            epoch: 1,
            last_activity: Instant::now(),
        });
        let (control_tx, _control_rx) = mpsc::channel(8);
        let descriptor = PaneDescriptor {
            pane_id: 1,
            host_peer_id: host_id.clone(),
            grid_rows: 1,
            grid_cols: 1,
            title: None,
            locked: false,
            exited: false,
        };
        host_panes
            .register_local_pane(
                descriptor.clone(),
                HostPaneChannels {
                    pane_id: pane_wire_id(1),
                    host_peer_id: host_id,
                    screen_rx: screen_rx.clone(),
                    lease_rx: lease_rx.clone(),
                    control_tx: control_tx.clone(),
                },
            )
            .expect("host pane");
        let dispatcher = host
            .incoming_dispatcher(host_panes.clone())
            .expect("single dispatcher");
        let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });

        let mut member =
            crate::session::join_layout(loopback_transport().await, host.ticket().clone())
                .await
                .expect("member joins");
        let LayoutControlEvent::Snapshot(snapshot) = member.events.recv().await.expect("snapshot")
        else {
            panic!("member must receive snapshot first");
        };
        let state = snapshot.state.expect("state");
        let member_panes = member
            .pane_server(host.ticket().session_id().to_vec())
            .expect("member pane server");
        let mut runtime = SharedLayoutRuntime::member_from_state(
            member,
            member_panes,
            host.ticket().session_id().to_vec(),
            state.clone(),
            tokio::runtime::Handle::current(),
        )
        .expect("member runtime");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain");
            if runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            runtime.remote.contains_key(&1),
            "remote pane attached from snapshot"
        );

        host_panes
            .remove_local_pane(1)
            .expect("remove direct pane")
            .expect("registered pane");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain after direct close");
            if !runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !runtime.remote.contains_key(&1),
            "a direct stream close removes the stale remote pane"
        );
        host_panes
            .register_local_pane(
                descriptor,
                HostPaneChannels {
                    pane_id: pane_wire_id(1),
                    host_peer_id: host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                    screen_rx,
                    lease_rx,
                    control_tx,
                },
            )
            .expect("restore direct pane");
        runtime
            .apply_layout_state(&state)
            .expect("authoritative snapshot nudges reconnect");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain after restore");
            if runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            runtime.remote.contains_key(&1),
            "remote pane reconnects after a transient direct close"
        );
        dispatcher_task.abort();
    }
}
