//! Who serializes a session once its coordinator stops answering.
//!
//! The coordinator is the only member that orders layout changes, admits joiners, and seals
//! the ledger. Losing it does not stop the session -- panes run on their own hosts and their
//! control leases are settled there -- but it does freeze everything structural until
//! somebody takes the role over. This module is the part that decides who.
//!
//! Two properties shape the whole design:
//!
//! **Nobody hands the role over.** A departed coordinator is, by definition, not available
//! to nominate a successor or to sign anything attesting to one. So the decision has to be
//! reachable independently by every member from state they already hold, and it has to come
//! out the same. That state is the last committed layout: its member list is in join order,
//! which gives every member the same ranking without a message being exchanged.
//!
//! **A claim proves nothing.** Because the decision is derivable, a member never has to
//! believe a peer that says it is in charge now -- it checks the claim against the answer it
//! computed itself. That is what makes a takeover safe without the old coordinator's
//! cooperation, and it is why [`Election::verdict_on`] exists separately from
//! [`Election::role_after`]: one is what this member would do, the other is what it will
//! accept from somebody else.
//!
//! Everything here is pure. No clock is read, no peer is contacted; elapsed time arrives as
//! an argument so the whole policy is testable in a few microseconds.

use std::time::Duration;

/// How long a session waits for a missing coordinator before replacing it.
///
/// The MVP's number. Long enough that closing a laptop lid, changing networks, or a relay
/// hiccup does not cost the session its coordinator, short enough that a room whose
/// coordinator has genuinely left is not stuck watching frozen structure for an afternoon.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(300);

/// Extra wait per position down the join order before a member promotes itself.
///
/// Members do not have a shared view of who else is still reachable -- a member hosting no
/// panes may hold no connections to its peers at all -- so the ranking is turned into time
/// instead of into a vote. The earliest survivor acts first; everyone behind it is still
/// waiting when its takeover arrives, and follows that instead of starting its own. Sized
/// well above the round trip a takeover needs to propagate, and it only ever costs the
/// session anything in the rare case where the earliest survivor is gone too.
pub const PROMOTION_STAGGER: Duration = Duration::from_secs(3);

/// What this member should do about the coordinator right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Role {
    /// The coordinator is answering. Nothing to decide.
    Following,
    /// The coordinator is missing, and it is not this member's turn to act.
    ///
    /// Either the grace window is still open, or it has closed and somebody ranked earlier
    /// is expected to take over first. Both are the same instruction: keep the session
    /// running, keep structural edits frozen, wait.
    Waiting,
    /// This member should take the role over, opening the epoch named here.
    Promote { epoch: u64 },
}

/// Whether to believe a peer that says it is the coordinator now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The claim matches what this member would have decided. Rotate to it.
    Accept,
    /// An epoch this member has already left behind, or is already in.
    ///
    /// The ordinary cause is a coordinator that dropped, was replaced, and has come back
    /// without knowing it -- not an attack.
    Stale { current: u64, claimed: u64 },
    /// The claimant is not in the last committed member list.
    NotAMember,
    /// The claimant is the coordinator that just left, trying to reclaim the role.
    ///
    /// It rejoins as an ordinary member instead. Auto-reclaim would make a flapping network
    /// hand the session back and forth.
    Deposed,
}

/// One member's view of the succession, built from the last committed layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Election {
    members: Vec<Vec<u8>>,
    coordinator: Vec<u8>,
    epoch: u64,
    me: Vec<u8>,
}

impl Election {
    /// `members` must be in join order, which is the order the committed layout stores them
    /// in. `coordinator` is whoever currently holds the role, and `epoch` how many times it
    /// has changed hands.
    pub fn new(members: Vec<Vec<u8>>, coordinator: Vec<u8>, epoch: u64, me: Vec<u8>) -> Self {
        Self {
            members,
            coordinator,
            epoch,
            me,
        }
    }

    /// The epoch currently in force.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The coordinator currently in force.
    pub fn coordinator(&self) -> &[u8] {
        &self.coordinator
    }

    /// Who should take over, in the order they should try.
    ///
    /// The departed coordinator is not a candidate for its own succession, which is what
    /// stops a coordinator that flaps from being handed the role straight back.
    pub fn candidates(&self) -> impl Iterator<Item = &[u8]> {
        self.members
            .iter()
            .filter(|peer| *peer != &self.coordinator)
            .map(Vec::as_slice)
    }

    /// How far down the succession this member sits, or `None` if it is not a candidate.
    pub fn rank(&self, peer: &[u8]) -> Option<usize> {
        self.candidates().position(|candidate| candidate == peer)
    }

    /// How long this member waits, from the coordinator going quiet, before acting.
    ///
    /// `None` when it is not a candidate at all, which means it always waits.
    pub fn delay_for(&self, peer: &[u8], grace: Duration) -> Option<Duration> {
        let rank = self.rank(peer)?;
        Some(grace + PROMOTION_STAGGER.saturating_mul(u32::try_from(rank).unwrap_or(u32::MAX)))
    }

    /// What to do, given how long the coordinator has been unreachable.
    ///
    /// `None` means it is answering.
    pub fn role_after(&self, unreachable_for: Option<Duration>, grace: Duration) -> Role {
        let Some(elapsed) = unreachable_for else {
            return Role::Following;
        };
        let Some(delay) = self.delay_for(&self.me, grace) else {
            return Role::Waiting;
        };
        if elapsed < delay {
            return Role::Waiting;
        }
        Role::Promote {
            epoch: self.epoch.saturating_add(1),
        }
    }

    /// Whether to accept `claimant` as the coordinator for `epoch`.
    ///
    /// Deliberately not a check that the claimant is the *best* candidate. A member cannot
    /// tell the difference between "the top-ranked survivor is gone as well" and "somebody
    /// jumped the queue", and refusing the second would strand the session in the first --
    /// which is the failure this whole module exists to prevent. The stagger is what keeps
    /// the queue orderly; membership, non-reclaim and a forward epoch are what keep it safe.
    pub fn verdict_on(&self, claimant: &[u8], epoch: u64) -> Verdict {
        if epoch <= self.epoch {
            return Verdict::Stale {
                current: self.epoch,
                claimed: epoch,
            };
        }
        if claimant == self.coordinator {
            return Verdict::Deposed;
        }
        if !self.members.iter().any(|member| member == claimant) {
            return Verdict::NotAMember;
        }
        Verdict::Accept
    }

    /// Move to a new coordinator, after [`Self::verdict_on`] agreed to it.
    pub fn rotated(&self, coordinator: Vec<u8>, epoch: u64) -> Self {
        Self {
            members: self.members.clone(),
            coordinator,
            epoch,
            me: self.me.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: u8) -> Vec<u8> {
        vec![id; 32]
    }

    /// Three members in join order, the first of them coordinating, seen by member `me`.
    fn election(me: u8) -> Election {
        Election::new(vec![peer(1), peer(2), peer(3)], peer(1), 0, peer(me))
    }

    #[test]
    fn a_coordinator_that_is_answering_settles_nothing() {
        assert_eq!(election(2).role_after(None, DEFAULT_GRACE), Role::Following);
    }

    #[test]
    fn nobody_moves_while_the_grace_window_is_open() {
        // The whole point of grace: a coordinator that closed its laptop for four minutes
        // comes back to the session it left, not to one that replaced it.
        let earliest = election(2);
        assert_eq!(
            earliest.role_after(Some(DEFAULT_GRACE - Duration::from_secs(1)), DEFAULT_GRACE),
            Role::Waiting
        );
    }

    #[test]
    fn the_earliest_survivor_goes_first_and_the_next_one_waits_behind_it() {
        // Same input, same grace, different answers -- which is what keeps two members from
        // promoting themselves at the same instant without exchanging a single message.
        let at_grace = Some(DEFAULT_GRACE);
        assert_eq!(
            election(2).role_after(at_grace, DEFAULT_GRACE),
            Role::Promote { epoch: 1 }
        );
        assert_eq!(
            election(3).role_after(at_grace, DEFAULT_GRACE),
            Role::Waiting
        );
    }

    #[test]
    fn a_lower_ranked_member_takes_over_when_the_one_ahead_never_does() {
        // The stagger is a queue, not a veto. If member 2 is gone too, member 3's turn
        // arrives on its own.
        assert_eq!(
            election(3).role_after(Some(DEFAULT_GRACE + PROMOTION_STAGGER), DEFAULT_GRACE),
            Role::Promote { epoch: 1 }
        );
    }

    #[test]
    fn the_coordinator_does_not_succeed_itself() {
        // Its own view of the election has to agree with everyone else's, or a coordinator
        // whose connections dropped would promote itself and fork the session.
        let itself = election(1);
        assert_eq!(itself.rank(&peer(1)), None);
        assert_eq!(
            itself.role_after(Some(DEFAULT_GRACE * 10), DEFAULT_GRACE),
            Role::Waiting
        );
    }

    #[test]
    fn the_epoch_counts_up_from_wherever_the_session_already_was() {
        // Already one takeover in, with member 2 coordinating. Member 1 -- the coordinator
        // that was replaced last time, back as an ordinary member -- is now first in the
        // succession again, because rank is join order and nothing about being deposed
        // removes it from that list.
        let second_takeover = Election::new(vec![peer(1), peer(2), peer(3)], peer(2), 1, peer(1));

        assert_eq!(second_takeover.rank(&peer(1)), Some(0));
        assert_eq!(
            second_takeover.role_after(Some(DEFAULT_GRACE), DEFAULT_GRACE),
            Role::Promote { epoch: 2 }
        );
    }

    #[test]
    fn a_claim_from_a_member_at_the_next_epoch_is_believed() {
        assert_eq!(election(3).verdict_on(&peer(2), 1), Verdict::Accept);
    }

    #[test]
    fn a_claim_at_an_epoch_already_in_force_is_stale() {
        // Where a second claimant lands after one takeover has already been accepted, and
        // where a replayed takeover message lands.
        let after = election(3).rotated(peer(2), 1);

        assert_eq!(
            after.verdict_on(&peer(3), 1),
            Verdict::Stale {
                current: 1,
                claimed: 1
            }
        );
        assert_eq!(
            after.verdict_on(&peer(3), 0),
            Verdict::Stale {
                current: 1,
                claimed: 0
            }
        );
    }

    #[test]
    fn the_coordinator_that_left_cannot_claim_its_way_back_in() {
        // A returning coordinator rejoins as an ordinary member. Letting it reclaim the role
        // would make a flapping network trade the session back and forth.
        assert_eq!(election(3).verdict_on(&peer(1), 1), Verdict::Deposed);
    }

    #[test]
    fn a_stranger_cannot_claim_the_role() {
        assert_eq!(election(3).verdict_on(&peer(9), 1), Verdict::NotAMember);
    }

    #[test]
    fn a_member_that_jumped_the_queue_is_still_accepted() {
        // Member 3 claiming before member 2 means member 2 is most likely gone as well.
        // Refusing would leave the session with no coordinator at all, which is worse than
        // the ordering being imperfect -- and every member reaches the same conclusion, so
        // they do not split.
        assert_eq!(election(2).verdict_on(&peer(3), 1), Verdict::Accept);
    }
}
