//! Surviving the loss of the coordinator, and taking the role over when it does not come back.
//!
//! Losing the coordinator has never ended a session here: panes run on the machines that
//! host them, their control leases are settled there, and screen data travels peer to peer.
//! What stops is everything the coordinator alone serializes -- layout edits, admission,
//! presence -- and until now that stopped permanently.
//!
//! This is the part that unfreezes it. Three situations, one mechanism:
//!
//! - **A member whose coordinator went quiet** waits out the grace window, then its share of
//!   the stagger, then takes the role over ([`crate::failover`] decides whose turn it is).
//! - **A member behind it in the queue** waits, then dials the successor and follows it.
//! - **A coordinator that lost the room** -- the same event seen from the other side, since a
//!   coordinator whose network dropped cannot tell that from everyone else leaving -- waits
//!   the same window and then rejoins as an ordinary member.
//!
//! The last one is why demotion exists at all. Without it a coordinator that flapped would
//! come back believing it still ran a session that had already moved on, and two coordinators
//! would be sealing entries onto the same ledger.

use std::{error::Error, time::Instant};

use iroh::EndpointAddr;

use crate::{
    failover::{Election, Role, Verdict},
    layout::Member,
    session::{SessionError, SharedLayoutHost, SharedLayoutMember, join_layout_with_display_name},
    ticket::JoinTicket,
    tui::pane::control::SharedControl,
};

use super::SharedLayoutRuntime;

/// How long to leave between attempts to reach a coordinator that is not answering.
///
/// A dial that fails costs a round trip against a peer that is probably gone, and the
/// interesting deadlines here are measured in minutes, so there is nothing to gain by
/// hammering it.
const REJOIN_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// The outcome of one attempt to attach to a coordinator, delivered back to the loop.
pub(in crate::tui) struct Rejoin {
    pub(in crate::tui) peer_id: Vec<u8>,
    pub(in crate::tui) result: Result<Box<RejoinSuccess>, String>,
}

pub(in crate::tui) struct RejoinSuccess {
    pub(in crate::tui) member: SharedLayoutMember,
    pub(in crate::tui) state: crate::protocol::LayoutState,
}

impl SharedLayoutRuntime {
    /// The coordinator's control stream ended. Start the clock.
    ///
    /// Deliberately not treated as fatal, and deliberately not reported as an error: from
    /// here the session is still usable for everything that does not need the coordinator,
    /// and saying so is the difference between "your session broke" and "structure is frozen
    /// for a few minutes".
    pub(in crate::tui) fn note_coordinator_lost(&mut self) {
        if self.coordinator_lost_at.is_none() {
            self.coordinator_lost_at = Some(Instant::now());
        }
        self.status = String::from(match self.control {
            SharedControl::Host(_) => "no members reachable; structure frozen while reconnecting",
            SharedControl::Member(_) => {
                "coordinator unreachable; panes still live, structure frozen"
            }
        });
    }

    /// Whether layout edits should be refused right now.
    ///
    /// A request sent while the coordinator is missing would go into a channel with nothing
    /// on the other end, and the member would sit forever waiting for a commit that cannot
    /// arrive. Refusing it up front is what turns that into a footer line.
    pub(in crate::tui) fn structural_edits_frozen(&self) -> bool {
        self.coordinator_lost_at.is_some()
    }

    /// A coordinator's half of the same signal a member gets as a dropped control stream.
    ///
    /// There is no event for this. A member is told its coordinator went away because the
    /// stream it was reading ends; a coordinator is told nothing at all, because the thing
    /// that would tell it is the peers that just vanished. So it has to notice, and what it
    /// notices is that a session with other people in it has nobody attached to it.
    ///
    /// The reverse matters just as much: one peer reattaching clears the clock. A coordinator
    /// whose relay blipped for ten seconds must not spend the next five minutes preparing to
    /// hand over a session it never actually lost.
    fn note_isolation(&mut self) {
        let SharedControl::Host(host) = &self.control else {
            return;
        };
        let alone = self.tui.snapshot().members.len() > 1 && host.connected_peers() == 0;
        match (alone, self.coordinator_lost_at) {
            (true, None) => self.note_coordinator_lost(),
            (false, Some(_)) => {
                self.coordinator_lost_at = None;
                self.rejoin_deadline = None;
                self.status.clear();
            }
            _ => {}
        }
    }

    /// This member's view of the succession, or `None` when there is nothing to decide.
    fn election(&self) -> Option<Election> {
        let snapshot = self.tui.snapshot();
        let members = snapshot
            .members
            .iter()
            .map(|member| member.peer_id.clone())
            .collect::<Vec<_>>();
        if members.len() < 2 {
            // A session of one has nobody to promote and nobody to follow. Whatever happened
            // to its network, waiting is the only move.
            return None;
        }
        let (coordinator, epoch) = match &self.control {
            SharedControl::Host(host) => (self.control.peer_id(), host.coordinator_epoch()),
            SharedControl::Member(member) => {
                (member.coordinator_peer_id.clone(), member.coordinator_epoch)
            }
        };
        Some(Election::new(
            members,
            coordinator,
            epoch,
            self.control.peer_id(),
        ))
    }

    /// One pass of the failover state machine. Called from the drain loop.
    pub(in crate::tui) fn tick_failover(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut changed = self.drain_rejoins()?;
        self.note_isolation();
        let Some(lost_at) = self.coordinator_lost_at else {
            return Ok(changed);
        };
        let Some(election) = self.election() else {
            return Ok(changed);
        };
        let elapsed = Instant::now().duration_since(lost_at);
        match election.role_after(Some(elapsed), self.grace) {
            Role::Following => {}
            Role::Waiting => {
                // Dial while waiting rather than after it. The coordinator may simply have
                // changed networks, and a member that reattaches inside the grace window
                // never learns there was one.
                changed |= self.attempt_rejoin(&election);
            }
            Role::Promote { epoch } => {
                self.promote(&election, epoch)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Take the coordinator role over.
    ///
    /// Everything the session is made of stays where it is. The PTYs this machine hosts keep
    /// running, the subscriptions other members hold onto them keep streaming, and the pane
    /// service that serves those subscriptions is handed to the new coordinator rather than
    /// rebuilt -- rebuilding it would empty the registry and drop every pane this machine
    /// runs out of the session at the exact moment it took charge of it.
    fn promote(&mut self, election: &Election, epoch: u64) -> Result<(), Box<dyn Error>> {
        let SharedControl::Member(member) = &self.control else {
            return Ok(());
        };
        let ledger_head = member.ledger_head();
        let session_name = member.session_name.clone();
        let replaced = election.coordinator().to_vec();
        let host = crate::session::HostSession::resume_with_session_name(
            self.transport.clone(),
            self.session_id.clone(),
            session_name,
            epoch,
        )?;
        let ticket = host.ticket().to_string();
        let layout_host = SharedLayoutHost::take_over(
            host,
            self.panes.clone(),
            self.tui.snapshot().clone(),
            ledger_head,
            crate::session::DEFAULT_RESERVATION_TIMEOUT,
        )?;
        let previous = std::mem::replace(&mut self.control, SharedControl::Host(layout_host));
        if let SharedControl::Member(member) = previous {
            member.disconnect_control();
        }
        // The invite has to be reissued: the old one names an endpoint nobody is listening
        // on. The short code is republished separately, so until that lands the share panel
        // offers the ticket alone rather than a code that resolves to a dead address.
        self.invite.ticket = Some(ticket);
        self.invite.code = None;
        self.swap_accept_loop(true);
        if let SharedControl::Host(host) = &self.control
            && let Err(error) = host.announce_takeover(replaced)
        {
            self.status = format!("took over the session, but could not record it: {error}");
        } else {
            self.status = String::from("coordinator left; this session is now coordinated here");
        }
        self.coordinator_lost_at = None;
        self.rejoin_deadline = None;
        Ok(())
    }

    /// Step down to an ordinary member of a session somebody else now coordinates.
    ///
    /// Reached by a coordinator that was isolated long enough for the room to replace it. It
    /// keeps its panes -- they are the same processes on the same machine, and the layout it
    /// is rejoining still lists them -- and gives up only the authority it no longer has.
    fn demote(&mut self, member: SharedLayoutMember) {
        let previous = std::mem::replace(&mut self.control, SharedControl::Member(member));
        if let SharedControl::Host(host) = previous {
            host.disconnect_peers();
        }
        self.invite.ticket = None;
        self.invite.code = None;
        self.swap_accept_loop(false);
    }

    /// Point this endpoint's accept loop at the right service for the role it now holds.
    ///
    /// A coordinator has to answer joins as well as pane subscriptions; a member answers only
    /// subscriptions. Both loops call `accept_incoming` on the same endpoint, so this is a
    /// swap and not an addition -- two of them running would race for every connection.
    fn swap_accept_loop(&mut self, coordinating: bool) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
        let panes = self.panes.clone();
        let task = match (&self.control, coordinating) {
            (SharedControl::Host(host), true) => match host.incoming_dispatcher(panes) {
                Ok(dispatcher) => self
                    .runtime
                    .spawn(async move { dispatcher.accept_loop().await }),
                Err(error) => {
                    self.status = format!("could not start serving joins: {error}");
                    return;
                }
            },
            _ => self.runtime.spawn(async move { panes.accept_loop().await }),
        };
        self.accept_task = Some(task);
    }

    /// Try to attach to whoever is coordinating now, if a previous attempt is not still open.
    ///
    /// Candidates are tried in succession order, one per round, so a member that is itself
    /// about to be promoted does not spend its wait blocked on a peer that is gone.
    fn attempt_rejoin(&mut self, election: &Election) -> bool {
        if self.rejoin_in_flight {
            return false;
        }
        let now = Instant::now();
        if self.rejoin_deadline.is_some_and(|deadline| now < deadline) {
            return false;
        }
        self.rejoin_deadline = Some(now + REJOIN_RETRY);
        let me = self.control.peer_id();
        // The coordinator first: the ordinary case is that it is simply still there and this
        // member's connection to it is what broke.
        let targets = std::iter::once(election.coordinator().to_vec())
            .chain(election.candidates().map(<[u8]>::to_vec))
            .filter(|peer| *peer != me)
            .collect::<Vec<_>>();
        let index = self.rejoin_cursor % targets.len().max(1);
        self.rejoin_cursor = self.rejoin_cursor.wrapping_add(1);
        let Some(target) = targets.get(index).cloned() else {
            return false;
        };
        let Some(endpoint) = self.member_endpoint(&target) else {
            return false;
        };
        let Ok(ticket) = JoinTicket::from_parts(self.session_id.clone(), endpoint) else {
            return false;
        };
        let display_name = self.local_display_name();
        let transport = self.transport.clone();
        let sender = self.rejoin_tx.clone();
        self.rejoin_in_flight = true;
        self.runtime.spawn(async move {
            let result = attach(transport, ticket, display_name).await;
            let _ = sender.send(Rejoin {
                peer_id: target,
                result: result.map(Box::new).map_err(|error| error.to_string()),
            });
        });
        true
    }

    /// Apply whatever the last attempt produced.
    fn drain_rejoins(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut changed = false;
        while let Ok(rejoin) = self.rejoin_rx.try_recv() {
            self.rejoin_in_flight = false;
            changed = true;
            let success = match rejoin.result {
                Ok(success) => success,
                Err(error) => {
                    self.status = format!("coordinator unreachable: {error}");
                    continue;
                }
            };
            let epoch = success.member.coordinator_epoch;
            // A connection is not an argument. The peer answered, which says it is alive and
            // holds the session secret -- it does not say the room agreed it is in charge,
            // and that is the question being asked here.
            let acceptable = match self.election() {
                None => true,
                // Reattaching to the same coordinator at the same epoch is the ordinary
                // case, and nothing has changed hands, so there is no claim to weigh.
                Some(election) => {
                    (rejoin.peer_id == election.coordinator() && epoch == election.epoch())
                        || matches!(election.verdict_on(&rejoin.peer_id, epoch), Verdict::Accept)
                }
            };
            if !acceptable {
                self.status = String::from("refused a coordinator this session did not elect");
                success.member.disconnect_control();
                continue;
            }
            let RejoinSuccess { member, state } = *success;
            self.panes.replace_roster_from_layout(&state)?;
            if matches!(self.control, SharedControl::Host(_)) {
                self.demote(member);
            } else {
                let previous = std::mem::replace(&mut self.control, SharedControl::Member(member));
                if let SharedControl::Member(previous) = previous {
                    previous.disconnect_control();
                }
            }
            self.coordinator_lost_at = None;
            self.rejoin_deadline = None;
            // Presence and agent rosters are relayed by the coordinator and cached there, so
            // a new one starts out knowing nothing about anybody. Clearing what this member
            // believes it has already published is what makes it publish again.
            self.seen_presence_epoch = 0;
            self.last_local_presence = None;
            self.last_local_agent_entries.clear();
            self.apply_layout_state(&state)?;
            self.status = String::from("reattached to the session's coordinator");
        }
        Ok(changed)
    }

    /// This member's own display name, as the session records it.
    fn local_display_name(&self) -> String {
        let me = self.control.peer_id();
        self.tui
            .snapshot()
            .members
            .iter()
            .find(|member| member.peer_id == me)
            .map(|member| member.display_name.clone())
            .unwrap_or_default()
    }

    fn member_endpoint(&self, peer_id: &[u8]) -> Option<EndpointAddr> {
        self.tui
            .snapshot()
            .members
            .iter()
            .find(|member: &&Member| member.peer_id == peer_id)
            .and_then(|member| serde_json::from_slice(&member.endpoint_addr).ok())
    }
}

/// Dial a coordinator and wait for the layout it answers with.
///
/// The first snapshot is part of attaching, not something that arrives later: without it
/// there is nothing to draw, nothing to check the succession against, and no roster to admit
/// pane subscriptions with.
async fn attach(
    transport: crate::transport::Transport,
    ticket: JoinTicket,
    display_name: String,
) -> Result<RejoinSuccess, SessionError> {
    let mut member = join_layout_with_display_name(transport, ticket, display_name).await?;
    match member.events.recv().await {
        Some(crate::session::LayoutControlEvent::Snapshot(snapshot)) => {
            let state = snapshot.state.ok_or(SessionError::InvalidPostWelcome)?;
            Ok(RejoinSuccess { member, state })
        }
        _ => Err(SessionError::InvalidPostWelcome),
    }
}
