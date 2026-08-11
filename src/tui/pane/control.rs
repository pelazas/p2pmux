//! The layout-control side of a session: requests to the coordinator, the
//! reservations they create, and remote pane subscriptions.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use crate::{
    layout::PaneId,
    protocol::{AgentRoster, LayoutRequest, PaneFailed, PaneReady, Presence, PresenceRoster},
    session::{
        CoordinatorResponse, LayoutControlEvent, LayoutControlQueueError, SharedLayoutHost,
        SharedLayoutMember,
    },
};

pub(in crate::tui) enum SharedControl {
    Host(SharedLayoutHost),
    Member(SharedLayoutMember),
}
impl SharedControl {
    pub(in crate::tui) fn peer_id(&self) -> Vec<u8> {
        match self {
            Self::Host(host) => host.ticket().endpoint_addr().id.as_bytes().to_vec(),
            Self::Member(member) => member.peer_id.clone(),
        }
    }

    pub(in crate::tui) fn try_request(
        &self,
        request: LayoutRequest,
    ) -> Result<Option<CoordinatorResponse>, String> {
        match self {
            Self::Host(host) => host
                .handle_local_request(request)
                .map(Some)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_request(request)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    pub(in crate::tui) fn try_ready(
        &self,
        ready: PaneReady,
    ) -> Result<Option<CoordinatorResponse>, String> {
        match self {
            Self::Host(host) => host
                .handle_local_ready(ready)
                .map(Some)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_ready(ready)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    pub(in crate::tui) fn try_failed(&self, failed: PaneFailed) -> Result<(), String> {
        match self {
            Self::Host(host) => host
                .handle_local_failed(failed)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Self::Member(member) => member.try_failed(failed).map_err(layout_queue_message),
        }
    }

    pub(in crate::tui) fn try_agent_roster(&self, roster: AgentRoster) -> Result<(), String> {
        match self {
            Self::Host(host) => host
                .publish_local_agent_roster(roster)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_agent_roster(roster)
                .map_err(layout_queue_message),
        }
    }

    /// Invitations members sent this coordinator. Empty on a member, which
    /// receives them as control events instead.
    pub(in crate::tui) fn take_fleet_invites(&self) -> Vec<(Vec<u8>, String)> {
        match self {
            Self::Host(host) => host.take_fleet_invites(),
            Self::Member(_) => Vec::new(),
        }
    }

    /// Answers to this coordinator's own requests that a member refused.
    ///
    /// Empty on a member, which is sent its rejections down the control stream
    /// like any other frame. Only the coordinator has to fetch its own,
    /// because it has no stream to itself to be sent them on.
    pub(in crate::tui) fn take_own_rejects(&self) -> Vec<crate::protocol::LayoutReject> {
        match self {
            Self::Host(host) => host.take_own_rejects(),
            Self::Member(_) => Vec::new(),
        }
    }

    /// Tell the other machines in this session about a session started here.
    ///
    /// The coordinator relays it to every member and acts on it itself; a
    /// member sends it to the coordinator, which does the same. Either way the
    /// decision to follow is made by each receiver against its own pairing
    /// record — see [`crate::protocol::FleetInvite`].
    pub(in crate::tui) fn try_fleet_invite(&self, ticket: String) -> Result<usize, String> {
        match self {
            Self::Host(host) => host
                .announce_fleet_invite(ticket)
                .map_err(|error| error.to_string()),
            // A member sends one message, to the coordinator, which relays it.
            Self::Member(member) => member
                .try_fleet_invite(ticket)
                .map(|()| 1)
                .map_err(layout_queue_message),
        }
    }

    /// Publishes this member's focus. The coordinator applies it locally and gets the
    /// resulting full roster back; a member has to wait for the broadcast.
    pub(in crate::tui) fn try_presence(
        &self,
        presence: Presence,
    ) -> Result<Option<PresenceRoster>, String> {
        match self {
            Self::Host(host) => host
                .publish_local_presence(presence)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_presence(presence)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    pub(in crate::tui) fn try_event(
        &mut self,
        current_revision: u64,
        seen_presence_epoch: &mut u64,
        seen_agent_generations: &mut BTreeMap<Vec<u8>, u64>,
    ) -> Option<LayoutControlEvent> {
        match self {
            // The coordinator is the authority, so it does not receive its own broadcasts. Poll
            // its tiny in-memory snapshot to observe joins, departures, and member-originated
            // commits without adding a second internal control stream.
            Self::Host(host) => {
                // Presence has its own counter because it deliberately does not touch the
                // layout revision: focus changes are frequent, and bumping the revision
                // for one would reject every in-flight layout request as stale.
                if let Some((epoch, roster)) = host.presence_if_newer(*seen_presence_epoch) {
                    *seen_presence_epoch = epoch;
                    return Some(LayoutControlEvent::Presence(roster));
                }
                // Same reason as presence: an accepted roster is broadcast to the peers,
                // and the coordinator is not one of them, so a member's agents would never
                // reach the coordinator's own overlay. Rosters already carry a per-host
                // generation, so that is the counter rather than a second epoch. One per
                // call is enough — the caller drains this in a loop.
                if let Some(roster) = host.agent_rosters().ok().and_then(|rosters| {
                    rosters.into_iter().find(|roster| {
                        seen_agent_generations
                            .get(&roster.host_peer_id)
                            .is_none_or(|seen| roster.generation > *seen)
                    })
                }) {
                    seen_agent_generations.insert(roster.host_peer_id.clone(), roster.generation);
                    return Some(LayoutControlEvent::AgentRoster(roster));
                }
                host.session_snapshot()
                    .ok()
                    .filter(|snapshot| {
                        snapshot
                            .state
                            .as_ref()
                            .is_some_and(|state| state.revision > current_revision)
                    })
                    .map(LayoutControlEvent::Snapshot)
            }
            Self::Member(member) => member.events.try_recv().ok(),
        }
    }

    pub(in crate::tui) async fn shutdown(self) {
        match self {
            Self::Host(host) => host.close().await,
            Self::Member(member) => member.shutdown().await,
        }
    }
}
pub(in crate::tui) fn layout_queue_message(error: LayoutControlQueueError) -> String {
    match error {
        LayoutControlQueueError::Full => String::from("layout request queue is busy"),
        LayoutControlQueueError::Closed => String::from("layout coordinator disconnected"),
    }
}
pub(in crate::tui) struct PendingCreate {
    pub(in crate::tui) request_id: u64,
    pub(in crate::tui) base_revision: u64,
    pub(in crate::tui) grid_rows: u16,
    pub(in crate::tui) grid_cols: u16,
    pub(in crate::tui) cwd: Option<PathBuf>,
    /// What the pane should run instead of a login shell. Empty for a shell.
    pub(in crate::tui) command: Vec<String>,
    /// Whether the pty for this request will be spawned here.
    ///
    /// False when the pane was asked for on another machine: the request is
    /// still ours to track, so a rejection lands somewhere, but the pane
    /// arrives with a commit rather than out of a pty this process opened.
    pub(in crate::tui) hosted_here: bool,
    /// The name of the machine it was asked for on, so a refusal can say which
    /// machine refused. Empty when the pane is being opened here.
    pub(in crate::tui) target_name: String,
    /// The machine it was asked for on. Empty when that is this one.
    ///
    /// Kept because a pane opened elsewhere arrives as a layout commit rather
    /// than as something this process spawned, and the tab it landed on is
    /// still the tab the person who asked wants to be looking at.
    pub(in crate::tui) target_peer: Vec<u8>,
}
/// Pure retry state for direct remote-pane subscriptions. Ticks are supplied by the UI drain
/// loop, which keeps retry behavior deterministic in tests and prevents a failed connect from
/// either becoming permanent or spinning a busy loop.
#[derive(Default)]
pub(in crate::tui) struct RemoteSubscriptionState {
    pub(in crate::tui) in_flight: BTreeSet<PaneId>,
    pub(in crate::tui) retry_at: BTreeMap<PaneId, u64>,
    pub(in crate::tui) failures: BTreeMap<PaneId, u8>,
}
impl RemoteSubscriptionState {
    const MAX_BACKOFF_TICKS: u64 = 32;

    pub(in crate::tui) fn start(&mut self, pane_id: PaneId, tick: u64) -> bool {
        if self.in_flight.contains(&pane_id)
            || self
                .retry_at
                .get(&pane_id)
                .is_some_and(|retry_at| *retry_at > tick)
        {
            return false;
        }
        self.in_flight.insert(pane_id);
        true
    }

    pub(in crate::tui) fn succeeded(&mut self, pane_id: PaneId) {
        self.in_flight.remove(&pane_id);
        self.retry_at.remove(&pane_id);
        self.failures.remove(&pane_id);
    }

    pub(in crate::tui) fn failed(&mut self, pane_id: PaneId, tick: u64) {
        self.in_flight.remove(&pane_id);
        let failures = self.failures.entry(pane_id).or_default();
        *failures = failures.saturating_add(1);
        let delay = 1_u64
            .checked_shl(u32::from((*failures).saturating_sub(1)))
            .unwrap_or(Self::MAX_BACKOFF_TICKS)
            .min(Self::MAX_BACKOFF_TICKS);
        self.retry_at.insert(pane_id, tick.saturating_add(delay));
    }

    pub(in crate::tui) fn nudge(&mut self) {
        for retry_at in self.retry_at.values_mut() {
            *retry_at = 0;
        }
    }

    pub(in crate::tui) fn retain(&mut self, pane_ids: &BTreeSet<PaneId>) {
        self.in_flight.retain(|pane_id| pane_ids.contains(pane_id));
        self.retry_at
            .retain(|pane_id, _| pane_ids.contains(pane_id));
        self.failures
            .retain(|pane_id, _| pane_ids.contains(pane_id));
    }

    #[cfg(test)]
    pub(in crate::tui) fn next_retry_at(&self, pane_id: PaneId) -> Option<u64> {
        self.retry_at.get(&pane_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteSubscriptionState;

    #[test]
    fn failed_remote_subscription_retries_with_bounded_backoff_and_commit_nudge() {
        let mut subscriptions = RemoteSubscriptionState::default();
        assert!(subscriptions.start(7, 0));
        subscriptions.failed(7, 0);
        assert!(!subscriptions.start(7, 0));
        assert!(subscriptions.start(7, 1));
        subscriptions.failed(7, 1);
        assert!(!subscriptions.start(7, 2));
        assert!(subscriptions.start(7, 3));

        for tick in [3, 7, 15, 31, 63, 95] {
            subscriptions.failed(7, tick);
            assert!(
                subscriptions
                    .next_retry_at(7)
                    .is_some_and(|next| next <= tick + 32)
            );
        }
        subscriptions.nudge();
        assert!(subscriptions.start(7, 95));
    }
}
