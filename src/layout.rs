use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

mod pane_map {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Pane, PaneId};

    pub fn serialize<S>(panes: &BTreeMap<PaneId, Pane>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        panes.values().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<PaneId, Pane>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Vec::<Pane>::deserialize(deserializer)?
            .into_iter()
            .map(|pane| (pane.pane_id, pane))
            .collect())
    }
}

pub const MAX_MEMBERS: usize = 8;
pub const MAX_TABS: usize = 9;
pub const MAX_PANES_PER_TAB: usize = 8;
pub const MAX_SPLIT_DEPTH: usize = 4;
pub const DEFAULT_FIRST_SHARE_BPS: u16 = 5_000;
pub const MIN_FIRST_SHARE_BPS: u16 = 1;
pub const MAX_FIRST_SHARE_BPS: u16 = 9_999;
/// Maximum peer-identifier size, aligned with the protocol boundary.
pub const MAX_PEER_ID_BYTES: usize = 64;
/// Maximum serialized endpoint-address size accepted by this pure model.
pub const MAX_ENDPOINT_ADDR_BYTES: usize = 4096;

pub type PaneId = u64;
pub type TabId = u64;
pub type ReservationId = u64;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    LeftRight,
    TopBottom,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewPanePosition {
    First,
    #[default]
    Second,
}

/// What is at the other end of a member: a machine, a person, or no answer.
///
/// The layout's copy of [`crate::protocol::MemberKind`], kept here so this
/// model stays free of the wire types the way [`Axis`] is. `Unspecified` is the
/// default for the same reason it is `0` on the wire: a member list written
/// before this field existed said nothing, and reading silence as an answer
/// would put words in an older peer's mouth.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    #[default]
    Unspecified,
    Machine,
    Person,
}

impl MemberKind {
    /// Whether this member may be recognized as one of your machines.
    ///
    /// Silence counts. A machine that was in your fleet before this field
    /// existed, or whose p2pmux started before the box was paired, is still
    /// your machine — the marker exists to let a peer disclaim being one, not
    /// to make every peer prove it.
    pub const fn could_be_machine(self) -> bool {
        !matches!(self, Self::Person)
    }

    /// Whether this member said, in as many words, that it is a machine.
    ///
    /// The stricter of the two questions, and the one asked before *writing* to
    /// the fleet record. Joining a fleet is a change; being recognized in one
    /// is not.
    pub const fn declared_machine(self) -> bool {
        matches!(self, Self::Machine)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Leaf {
        pane_id: PaneId,
    },
    Split {
        axis: Axis,
        first_share_bps: u16,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Member {
    pub peer_id: Vec<u8>,
    pub endpoint_addr: Vec<u8>,
    pub display_name: String,
    /// What this member said it is. Never what the coordinator decided it is:
    /// the claim is carried, and every client answers "is that one of mine"
    /// against its own pairing record.
    #[serde(default)]
    pub kind: MemberKind,
    /// The machine behind this member, when it proved which one it is.
    ///
    /// Empty when it offered no proof or offered one that did not check out —
    /// so this is verified or absent, never merely claimed. A node's peer id
    /// belongs to one process; this is what survives the process, and it is
    /// what a fleet record is keyed on.
    #[serde(default)]
    pub machine_id: Vec<u8>,
    /// The signature `machine_id` was accepted on, carried so that every member
    /// can check it for itself rather than taking the coordinator's word.
    #[serde(default)]
    pub machine_proof: Vec<u8>,
}

/// Everything a member says about itself when it joins.
///
/// Bundled rather than passed as four more arguments, because they travel
/// together everywhere and because the interesting invariant is about the pair
/// at the end: a machine id is kept only with a proof that checks out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemberIdentity {
    pub display_name: String,
    pub kind: MemberKind,
    pub machine_id: Vec<u8>,
    pub machine_proof: Vec<u8>,
}

impl MemberIdentity {
    /// Drop a machine id whose proof does not check out against `peer_id`.
    ///
    /// Called wherever a member list is built, so that everything downstream
    /// can read a machine id as verified or absent and never ask again.
    pub fn verified_for(mut self, peer_id: &[u8]) -> Self {
        if !crate::machine_id::verify(&self.machine_id, peer_id, &self.machine_proof) {
            self.machine_id.clear();
            self.machine_proof.clear();
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Pane {
    pub pane_id: PaneId,
    pub host_peer_id: Vec<u8>,
    pub locked: bool,
    pub exited: bool,
    pub grid_rows: u16,
    pub grid_cols: u16,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Tab {
    pub tab_id: TabId,
    pub root: Node,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LayoutSnapshot {
    pub revision: u64,
    pub members: Vec<Member>,
    pub tabs: Vec<Tab>,
    #[serde(with = "pane_map")]
    pub panes: BTreeMap<PaneId, Pane>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneReservation {
    pub reservation_id: ReservationId,
    pub pane_id: PaneId,
    pub tab_id: Option<TabId>,
    /// The peer that will spawn the pty and host the pane. Equal to the creator
    /// for every pane created before machines could ask each other for one.
    pub host_peer_id: Vec<u8>,
    pub grid_rows: u16,
    pub grid_cols: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidatedReservation {
    pub reservation_id: ReservationId,
    pub creator_peer_id: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReservationCommit {
    Pane { pane_id: PaneId },
    Tab { tab_id: TabId, pane_id: PaneId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingReservationKind {
    Pane {
        pane_id: PaneId,
        target_pane_id: PaneId,
        axis: Axis,
        position: NewPanePosition,
        grid_rows: u16,
        grid_cols: u16,
    },
    Tab {
        tab_id: TabId,
        pane_id: PaneId,
        grid_rows: u16,
        grid_cols: u16,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingReservation {
    reservation_id: ReservationId,
    /// Who asked for the pane, and therefore who is told when it fails.
    creator_peer_id: Vec<u8>,
    /// Who was asked to spawn it, and therefore who may report it ready.
    ///
    /// The same peer as the creator in every case but one: a pane opened on
    /// another machine. Keeping them apart is what makes that case possible
    /// without letting a peer report a pane ready that it was never asked for.
    host_peer_id: Vec<u8>,
    base_revision: u64,
    kind: PendingReservationKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    revision: u64,
    members: Vec<Member>,
    tabs: Vec<Tab>,
    panes: BTreeMap<PaneId, Pane>,
    next_tab_id: TabId,
    next_pane_id: PaneId,
    next_reservation_id: ReservationId,
    pending_reservation: Option<PendingReservation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    StaleRevision {
        expected: u64,
        got: u64,
    },
    ConflictingSnapshotRevision {
        revision: u64,
    },
    RevisionExhausted,
    MemberLimit,
    AlreadyMember,
    NotMember,
    InvalidPeerId,
    InvalidEndpointAddress,
    InvalidDisplayName,
    InvalidTitle,
    TabLimit,
    PaneLimit,
    SplitDepthLimit,
    InvalidGrid,
    InvalidSplitRatio,
    UnknownPane {
        pane_id: PaneId,
    },
    NoMatchingSplit {
        pane_id: PaneId,
        axis: Axis,
    },
    UnknownTab {
        tab_id: TabId,
    },
    NotPaneHost {
        pane_id: PaneId,
    },
    NotTabHost {
        tab_id: TabId,
    },
    LastPaneInTab {
        tab_id: TabId,
    },
    LastTab,
    ReservationPending,
    /// A pane was asked for on a machine that is not in this session.
    ///
    /// Its own error rather than `NotMember`, which is about the peer doing the
    /// asking. This one is about the machine being asked, and it is the failure
    /// a user can actually act on: wake the machine.
    UnknownTarget,
    UnknownReservation {
        reservation_id: ReservationId,
    },
    ReservationCreatorMismatch,
    ReservationInvalid,
    InvalidSnapshot,
    IdExhausted,
}

impl SessionState {
    pub fn new(
        initial_host: Vec<u8>,
        endpoint_addr: Vec<u8>,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<Self, LayoutError> {
        Self::new_with_display_name(
            initial_host,
            endpoint_addr,
            String::new(),
            grid_rows,
            grid_cols,
        )
    }

    pub fn new_with_display_name(
        initial_host: Vec<u8>,
        endpoint_addr: Vec<u8>,
        display_name: String,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<Self, LayoutError> {
        Self::new_with_identity(
            initial_host,
            endpoint_addr,
            MemberIdentity {
                display_name,
                ..MemberIdentity::default()
            },
            grid_rows,
            grid_cols,
        )
    }

    pub fn new_with_identity(
        initial_host: Vec<u8>,
        endpoint_addr: Vec<u8>,
        identity: MemberIdentity,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<Self, LayoutError> {
        let identity = identity.verified_for(&initial_host);
        let MemberIdentity {
            display_name,
            kind,
            machine_id,
            machine_proof,
        } = identity;
        validate_grid(grid_rows, grid_cols)?;
        validate_peer_id(&initial_host)?;
        validate_endpoint_addr(&endpoint_addr)?;
        validate_display_name(&display_name)?;
        let initial_pane = Pane {
            pane_id: 1,
            host_peer_id: initial_host.clone(),
            locked: false,
            exited: false,
            grid_rows,
            grid_cols,
            title: None,
        };
        let mut panes = BTreeMap::new();
        panes.insert(1, initial_pane);
        Ok(Self {
            revision: 1,
            members: vec![Member {
                peer_id: initial_host,
                endpoint_addr,
                display_name,
                kind,
                machine_id,
                machine_proof,
            }],
            tabs: vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            panes,
            next_tab_id: 2,
            next_pane_id: 2,
            next_reservation_id: 1,
            pending_reservation: None,
        })
    }

    /// Rebuild authority over a session that already exists, from its last committed layout.
    ///
    /// Every other constructor here fabricates a session: one member, one tab, one pane. A
    /// member promoted after its coordinator left needs the opposite -- the tabs, panes and
    /// join order the room is already looking at, with itself now the one allowed to change
    /// them. It does not get to invent any of that, which is why the snapshot goes through
    /// the same validation an untrusted one would.
    ///
    /// The id counters resume past what the snapshot already uses, so a pane created after
    /// the takeover cannot collide with one created before it. Reservations do not resume:
    /// they live on the control streams a takeover tears down, and the requester is told its
    /// pane never landed rather than being left holding a provisional PTY forever.
    ///
    /// The departed coordinator stays in the member list. Its panes are dead, and every
    /// member already draws a pane whose host is gone as unavailable, so evicting it here
    /// would trade a placeholder for a layout that silently rearranged itself under people
    /// mid-sentence -- and in a two-person session where only the coordinator hosted
    /// anything, for no layout at all. Eviction is a decision for whoever calls
    /// [`Self::remove_member`], once the grace window has actually expired.
    pub fn restore(snapshot: LayoutSnapshot) -> Result<Self, LayoutError> {
        Self::validate_snapshot(&snapshot)?;
        let next_pane_id = snapshot
            .panes
            .keys()
            .copied()
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(LayoutError::RevisionExhausted)?;
        let next_tab_id = snapshot
            .tabs
            .iter()
            .map(|tab| tab.tab_id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(LayoutError::RevisionExhausted)?;
        Ok(Self {
            revision: snapshot.revision,
            members: snapshot.members,
            tabs: snapshot.tabs,
            panes: snapshot.panes,
            next_tab_id,
            next_pane_id,
            next_reservation_id: 1,
            pending_reservation: None,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn snapshot(&self) -> LayoutSnapshot {
        LayoutSnapshot {
            revision: self.revision,
            members: self.members.clone(),
            tabs: self.tabs.clone(),
            panes: self.panes.clone(),
        }
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn pane(&self, pane_id: PaneId) -> Option<&Pane> {
        self.panes.get(&pane_id)
    }

    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.panes.values()
    }

    pub fn validate_snapshot(snapshot: &LayoutSnapshot) -> Result<(), LayoutError> {
        if snapshot.revision == 0
            || snapshot.members.is_empty()
            || snapshot.members.len() > MAX_MEMBERS
            || snapshot.tabs.is_empty()
            || snapshot.tabs.len() > MAX_TABS
        {
            return Err(LayoutError::InvalidSnapshot);
        }
        let mut peer_ids = BTreeSet::new();
        for member in &snapshot.members {
            if validate_peer_id(&member.peer_id).is_err()
                || !peer_ids.insert(&member.peer_id)
                || validate_endpoint_addr(&member.endpoint_addr).is_err()
                || validate_display_name(&member.display_name).is_err()
            {
                return Err(LayoutError::InvalidSnapshot);
            }
        }
        let mut tab_ids = BTreeSet::new();
        let mut pane_ids = BTreeSet::new();
        for tab in &snapshot.tabs {
            if tab.tab_id == 0 || !tab_ids.insert(tab.tab_id) || !is_normalized_title(&tab.title) {
                return Err(LayoutError::InvalidSnapshot);
            }
            let before = pane_ids.len();
            if !tab.root.collect_pane_ids(0, &mut pane_ids)
                || pane_ids.len() - before > MAX_PANES_PER_TAB
            {
                return Err(LayoutError::InvalidSnapshot);
            }
            if !tab.root.has_valid_ratios() {
                return Err(LayoutError::InvalidSnapshot);
            }
        }
        if pane_ids.len() != snapshot.panes.len()
            || snapshot.panes.len() > MAX_TABS * MAX_PANES_PER_TAB
        {
            return Err(LayoutError::InvalidSnapshot);
        }
        for (pane_id, pane) in &snapshot.panes {
            if *pane_id != pane.pane_id
                || !pane_ids.contains(pane_id)
                || validate_grid(pane.grid_rows, pane.grid_cols).is_err()
                || !peer_ids.contains(&pane.host_peer_id)
                || !is_normalized_title(&pane.title)
            {
                return Err(LayoutError::InvalidSnapshot);
            }
        }
        Ok(())
    }

    pub fn add_member(
        &mut self,
        base_revision: u64,
        peer_id: Vec<u8>,
        endpoint_addr: Vec<u8>,
    ) -> Result<Option<InvalidatedReservation>, LayoutError> {
        self.add_member_with_display_name(base_revision, peer_id, endpoint_addr, String::new())
    }

    pub fn add_member_with_display_name(
        &mut self,
        base_revision: u64,
        peer_id: Vec<u8>,
        endpoint_addr: Vec<u8>,
        display_name: String,
    ) -> Result<Option<InvalidatedReservation>, LayoutError> {
        self.add_member_with_identity(
            base_revision,
            peer_id,
            endpoint_addr,
            MemberIdentity {
                display_name,
                ..MemberIdentity::default()
            },
        )
    }

    pub fn add_member_with_identity(
        &mut self,
        base_revision: u64,
        peer_id: Vec<u8>,
        endpoint_addr: Vec<u8>,
        identity: MemberIdentity,
    ) -> Result<Option<InvalidatedReservation>, LayoutError> {
        let MemberIdentity {
            display_name,
            kind,
            machine_id,
            machine_proof,
        } = identity.verified_for(&peer_id);
        self.check_mutation(base_revision)?;
        validate_peer_id(&peer_id)?;
        validate_endpoint_addr(&endpoint_addr)?;
        validate_display_name(&display_name)?;
        if self.members.iter().any(|member| member.peer_id == peer_id) {
            return Err(LayoutError::AlreadyMember);
        }
        if self.members.len() >= MAX_MEMBERS {
            return Err(LayoutError::MemberLimit);
        }
        self.members.push(Member {
            peer_id,
            endpoint_addr,
            display_name,
            kind,
            machine_id,
            machine_proof,
        });
        self.advance_revision();
        Ok(self.invalidate_reservation())
    }

    /// Adopt a member's newly declared kind, and say whether anything moved.
    ///
    /// Only ever a *strengthening*: `Unspecified` may become `Machine` or
    /// `Person`, and neither of those may become anything else. A member that
    /// has said what it is has said it, and letting the claim be taken back
    /// would make "is this one of my machines" a question whose answer depends
    /// on when you asked.
    ///
    /// This exists because the claim is made once, at node start, from a
    /// pairing record that is very often written a minute later — `p2pmux pair`
    /// while a session is already open is the ordinary way to add a machine.
    /// Without a way to say so afterwards, a box that has just joined a fleet
    /// goes on announcing that it belongs to none until it is restarted, and
    /// `pin_peers` refuses to write it into anybody's fleet the whole time.
    /// `Ok(None)` means the claim was refused or was already there and nothing
    /// moved, which the caller must not turn into a commit: a revision bump per
    /// re-announcement would be a burst of layout traffic caused by nothing.
    pub fn update_member_kind(
        &mut self,
        base_revision: u64,
        peer_id: &[u8],
        kind: MemberKind,
    ) -> Result<Option<Option<InvalidatedReservation>>, LayoutError> {
        self.check_mutation(base_revision)?;
        let member = self
            .members
            .iter_mut()
            .find(|member| member.peer_id == peer_id)
            .ok_or(LayoutError::NotMember)?;
        if kind == MemberKind::Unspecified || member.kind != MemberKind::Unspecified {
            return Ok(None);
        }
        member.kind = kind;
        self.advance_revision();
        Ok(Some(self.invalidate_reservation()))
    }

    pub fn update_member_endpoint(
        &mut self,
        base_revision: u64,
        peer_id: &[u8],
        endpoint_addr: Vec<u8>,
    ) -> Result<Option<InvalidatedReservation>, LayoutError> {
        self.check_mutation(base_revision)?;
        validate_endpoint_addr(&endpoint_addr)?;
        let member = self
            .members
            .iter_mut()
            .find(|member| member.peer_id == peer_id)
            .ok_or(LayoutError::NotMember)?;
        member.endpoint_addr = endpoint_addr;
        self.advance_revision();
        Ok(self.invalidate_reservation())
    }

    /// Removes a departed non-final member and every pane it hosted. Tabs with no remaining
    /// leaves disappear; mixed tabs collapse around the departed leaves in one revision.
    pub fn remove_member(
        &mut self,
        peer_id: &[u8],
    ) -> Result<Option<InvalidatedReservation>, LayoutError> {
        self.require_member(peer_id)?;
        if self.members.len() == 1 {
            return Err(LayoutError::InvalidSnapshot);
        }
        self.revision
            .checked_add(1)
            .ok_or(LayoutError::RevisionExhausted)?;
        let removed_panes = self
            .panes
            .values()
            .filter(|pane| pane.host_peer_id == peer_id)
            .map(|pane| pane.pane_id)
            .collect::<BTreeSet<_>>();
        let mut tabs = Vec::with_capacity(self.tabs.len());
        for tab in self.tabs.drain(..) {
            let mut root = Some(tab.root);
            for pane_id in &removed_panes {
                root = root.and_then(|root| root.remove_leaf(*pane_id));
            }
            if let Some(root) = root {
                tabs.push(Tab {
                    tab_id: tab.tab_id,
                    root,
                    title: tab.title,
                });
            }
        }
        if tabs.is_empty() {
            return Err(LayoutError::InvalidSnapshot);
        }
        self.members.retain(|member| member.peer_id != peer_id);
        self.panes
            .retain(|pane_id, _| !removed_panes.contains(pane_id));
        self.tabs = tabs;
        self.advance_revision();
        Ok(self.invalidate_reservation())
    }

    pub fn reserve_pane(
        &mut self,
        creator: &[u8],
        base_revision: u64,
        target_pane_id: PaneId,
        axis: Axis,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneReservation, LayoutError> {
        self.reserve_pane_at(
            creator,
            base_revision,
            target_pane_id,
            axis,
            NewPanePosition::Second,
            grid_rows,
            grid_cols,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve_pane_at(
        &mut self,
        creator: &[u8],
        base_revision: u64,
        target_pane_id: PaneId,
        axis: Axis,
        position: NewPanePosition,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneReservation, LayoutError> {
        self.reserve_pane_on(
            creator,
            creator,
            base_revision,
            target_pane_id,
            axis,
            position,
            grid_rows,
            grid_cols,
        )
    }

    /// Reserve a pane that another member will host.
    ///
    /// `host` is the machine whose shell the pane runs; `creator` is the one
    /// that asked. They are the same peer for every split anyone types, and
    /// differ only when a machine is asked to open a terminal on another.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_pane_on(
        &mut self,
        creator: &[u8],
        host: &[u8],
        base_revision: u64,
        target_pane_id: PaneId,
        axis: Axis,
        position: NewPanePosition,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneReservation, LayoutError> {
        self.check_reservation(base_revision)?;
        self.require_member(creator)?;
        // The machine being asked has to be in the session. A pane reserved for
        // a peer that is not here would sit unfulfilled until it timed out,
        // which reads to the user as nothing happening.
        self.require_member(host)
            .map_err(|_| LayoutError::UnknownTarget)?;
        validate_grid(grid_rows, grid_cols)?;
        self.ensure_no_reservation()?;
        self.validate_pane_create(target_pane_id)?;
        let pane_id = self.next_pane_id;
        let reservation_id = self.next_reservation_id;
        let next_pane_id = self.next_id(pane_id)?;
        let next_reservation_id = self.next_id(reservation_id)?;
        self.next_pane_id = next_pane_id;
        self.next_reservation_id = next_reservation_id;
        self.pending_reservation = Some(PendingReservation {
            reservation_id,
            creator_peer_id: creator.to_vec(),
            host_peer_id: host.to_vec(),
            base_revision,
            kind: PendingReservationKind::Pane {
                pane_id,
                target_pane_id,
                axis,
                position,
                grid_rows,
                grid_cols,
            },
        });
        Ok(PaneReservation {
            reservation_id,
            pane_id,
            tab_id: None,
            host_peer_id: host.to_vec(),
            grid_rows,
            grid_cols,
        })
    }

    pub fn reserve_tab(
        &mut self,
        creator: &[u8],
        base_revision: u64,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneReservation, LayoutError> {
        self.reserve_tab_on(creator, creator, base_revision, grid_rows, grid_cols)
    }

    /// Reserve a tab whose first pane another member will host. See
    /// [`Self::reserve_pane_on`].
    pub fn reserve_tab_on(
        &mut self,
        creator: &[u8],
        host: &[u8],
        base_revision: u64,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneReservation, LayoutError> {
        self.check_reservation(base_revision)?;
        self.require_member(creator)?;
        self.require_member(host)
            .map_err(|_| LayoutError::UnknownTarget)?;
        validate_grid(grid_rows, grid_cols)?;
        self.ensure_no_reservation()?;
        if self.tabs.len() >= MAX_TABS {
            return Err(LayoutError::TabLimit);
        }
        let tab_id = self.next_tab_id;
        let pane_id = self.next_pane_id;
        let reservation_id = self.next_reservation_id;
        let next_tab_id = self.next_id(tab_id)?;
        let next_pane_id = self.next_id(pane_id)?;
        let next_reservation_id = self.next_id(reservation_id)?;
        self.next_tab_id = next_tab_id;
        self.next_pane_id = next_pane_id;
        self.next_reservation_id = next_reservation_id;
        self.pending_reservation = Some(PendingReservation {
            reservation_id,
            creator_peer_id: creator.to_vec(),
            host_peer_id: host.to_vec(),
            base_revision,
            kind: PendingReservationKind::Tab {
                tab_id,
                pane_id,
                grid_rows,
                grid_cols,
            },
        });
        Ok(PaneReservation {
            reservation_id,
            pane_id,
            tab_id: Some(tab_id),
            host_peer_id: host.to_vec(),
            grid_rows,
            grid_cols,
        })
    }

    /// Commit a reservation the host has fulfilled.
    ///
    /// `host` is authenticated by the caller and must be the peer the
    /// reservation named, not the one that asked for it: the pane is going to
    /// be served by whoever says it is ready, so that claim has to come from
    /// the machine the coordinator actually asked.
    pub fn pane_ready(
        &mut self,
        host: &[u8],
        base_revision: u64,
        reservation_id: ReservationId,
    ) -> Result<ReservationCommit, LayoutError> {
        let reservation = self.match_reservation(host, reservation_id)?;
        if reservation.base_revision != base_revision {
            return Err(LayoutError::StaleRevision {
                expected: reservation.base_revision,
                got: base_revision,
            });
        }
        self.check_mutation(base_revision)?;
        match reservation.kind {
            PendingReservationKind::Pane {
                pane_id,
                target_pane_id,
                axis,
                position,
                grid_rows,
                grid_cols,
            } => {
                self.validate_pane_create(target_pane_id)?;
                let tab_index = self
                    .tab_index_for_pane(target_pane_id)
                    .ok_or(LayoutError::ReservationInvalid)?;
                let target = Box::new(Node::Leaf {
                    pane_id: target_pane_id,
                });
                let new_pane = Box::new(Node::Leaf { pane_id });
                let (first, second) = match position {
                    NewPanePosition::First => (new_pane, target),
                    NewPanePosition::Second => (target, new_pane),
                };
                let split = Node::Split {
                    axis,
                    first_share_bps: DEFAULT_FIRST_SHARE_BPS,
                    first,
                    second,
                };
                if !self.tabs[tab_index]
                    .root
                    .replace_leaf(target_pane_id, split)
                {
                    return Err(LayoutError::ReservationInvalid);
                }
                self.panes.insert(
                    pane_id,
                    Pane {
                        pane_id,
                        host_peer_id: reservation.host_peer_id.clone(),
                        locked: false,
                        exited: false,
                        grid_rows,
                        grid_cols,
                        title: None,
                    },
                );
                self.pending_reservation = None;
                self.advance_revision();
                Ok(ReservationCommit::Pane { pane_id })
            }
            PendingReservationKind::Tab {
                tab_id,
                pane_id,
                grid_rows,
                grid_cols,
            } => {
                if self.tabs.len() >= MAX_TABS {
                    return Err(LayoutError::ReservationInvalid);
                }
                self.tabs.push(Tab {
                    tab_id,
                    root: Node::Leaf { pane_id },
                    title: None,
                });
                self.panes.insert(
                    pane_id,
                    Pane {
                        pane_id,
                        host_peer_id: reservation.host_peer_id.clone(),
                        locked: false,
                        exited: false,
                        grid_rows,
                        grid_cols,
                        title: None,
                    },
                );
                self.pending_reservation = None;
                self.advance_revision();
                Ok(ReservationCommit::Tab { tab_id, pane_id })
            }
        }
    }

    pub fn cancel_reservation(
        &mut self,
        creator: &[u8],
        reservation_id: ReservationId,
    ) -> Result<(), LayoutError> {
        self.match_reservation(creator, reservation_id)?;
        self.pending_reservation = None;
        Ok(())
    }

    pub fn fail_reservation(
        &mut self,
        creator: &[u8],
        base_revision: u64,
        reservation_id: ReservationId,
    ) -> Result<(), LayoutError> {
        let reservation = self.match_reservation(creator, reservation_id)?;
        if reservation.base_revision != base_revision {
            return Err(LayoutError::StaleRevision {
                expected: reservation.base_revision,
                got: base_revision,
            });
        }
        self.pending_reservation = None;
        Ok(())
    }

    pub fn expire_reservation(&mut self, reservation_id: ReservationId) -> Result<(), LayoutError> {
        let pending = self
            .pending_reservation
            .as_ref()
            .ok_or(LayoutError::UnknownReservation { reservation_id })?;
        if pending.reservation_id != reservation_id {
            return Err(LayoutError::UnknownReservation { reservation_id });
        }
        self.pending_reservation = None;
        Ok(())
    }

    pub fn create_pane(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        target_pane_id: PaneId,
        axis: Axis,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneId, LayoutError> {
        self.create_pane_at(
            requester,
            base_revision,
            target_pane_id,
            axis,
            NewPanePosition::Second,
            grid_rows,
            grid_cols,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_pane_at(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        target_pane_id: PaneId,
        axis: Axis,
        position: NewPanePosition,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneId, LayoutError> {
        let reservation = self.reserve_pane_at(
            requester,
            base_revision,
            target_pane_id,
            axis,
            position,
            grid_rows,
            grid_cols,
        )?;
        match self.pane_ready(requester, base_revision, reservation.reservation_id)? {
            ReservationCommit::Pane { pane_id } => Ok(pane_id),
            ReservationCommit::Tab { .. } => unreachable!("pane reservation commits a pane"),
        }
    }

    pub fn create_tab(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<TabId, LayoutError> {
        let reservation = self.reserve_tab(requester, base_revision, grid_rows, grid_cols)?;
        match self.pane_ready(requester, base_revision, reservation.reservation_id)? {
            ReservationCommit::Tab { tab_id, .. } => Ok(tab_id),
            ReservationCommit::Pane { .. } => unreachable!("tab reservation commits a tab"),
        }
    }

    pub fn delete_pane(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        pane_id: PaneId,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if pane.host_peer_id != requester {
            return Err(LayoutError::NotPaneHost { pane_id });
        }
        let tab_index = self
            .tab_index_for_pane(pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if self.pane_ids_in_tab_at(tab_index).len() == 1 {
            return Err(LayoutError::LastPaneInTab {
                tab_id: self.tabs[tab_index].tab_id,
            });
        }
        let root = self.tabs[tab_index].root.clone();
        self.tabs[tab_index].root = root
            .remove_leaf(pane_id)
            .expect("non-singleton tab retains a root");
        self.panes.remove(&pane_id);
        self.advance_revision();
        Ok(())
    }

    pub fn delete_tab(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        tab_id: TabId,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        if self.tabs.len() == 1 {
            return Err(LayoutError::LastTab);
        }
        let pane_ids = self.pane_ids_in_tab_at(tab_index);
        if pane_ids.iter().any(|pane_id| {
            self.panes
                .get(pane_id)
                .is_none_or(|pane| pane.host_peer_id != requester)
        }) {
            return Err(LayoutError::NotTabHost { tab_id });
        }
        self.tabs.remove(tab_index);
        for pane_id in pane_ids {
            self.panes.remove(&pane_id);
        }
        self.advance_revision();
        Ok(())
    }

    pub fn set_split_ratio(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        pane_id: PaneId,
        axis: Axis,
        first_share_bps: u16,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        validate_first_share_bps(first_share_bps)?;
        let tab_index = self
            .tab_index_for_pane(pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if !self.tabs[tab_index]
            .root
            .set_nearest_split_ratio(pane_id, axis, first_share_bps)
        {
            return Err(LayoutError::NoMatchingSplit { pane_id, axis });
        }
        self.advance_revision();
        Ok(())
    }

    pub fn update_pane_grids(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        grids: &[(PaneId, u16, u16)],
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        if grids.is_empty() {
            return Err(LayoutError::InvalidGrid);
        }
        let mut pane_ids = BTreeSet::new();
        for &(pane_id, rows, cols) in grids {
            validate_grid(rows, cols)?;
            if !pane_ids.insert(pane_id) {
                return Err(LayoutError::InvalidGrid);
            }
            let pane = self
                .panes
                .get(&pane_id)
                .ok_or(LayoutError::UnknownPane { pane_id })?;
            if pane.host_peer_id != requester {
                return Err(LayoutError::NotPaneHost { pane_id });
            }
        }
        for &(pane_id, rows, cols) in grids {
            let pane = self.panes.get_mut(&pane_id).expect("validated pane exists");
            pane.grid_rows = rows;
            pane.grid_cols = cols;
        }
        self.advance_revision();
        Ok(())
    }

    pub fn set_pane_lock(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        pane_id: PaneId,
        locked: bool,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        let pane = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if pane.host_peer_id != requester {
            return Err(LayoutError::NotPaneHost { pane_id });
        }
        pane.locked = locked;
        self.advance_revision();
        Ok(())
    }

    pub fn mark_pane_exited(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        pane_id: PaneId,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        let pane = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if pane.host_peer_id != requester {
            return Err(LayoutError::NotPaneHost { pane_id });
        }
        if !pane.exited {
            pane.exited = true;
            self.advance_revision();
        }
        Ok(())
    }

    pub fn rename_pane(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        pane_id: PaneId,
        title: String,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        let title = normalize_title(&title)?;
        let pane = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        pane.title = title;
        self.advance_revision();
        Ok(())
    }

    pub fn rename_tab(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        tab_id: TabId,
        title: String,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        self.ensure_no_reservation()?;
        let title = normalize_title(&title)?;
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        tab.title = title;
        self.advance_revision();
        Ok(())
    }

    pub fn pane_ids_in_tab(&self, tab_id: TabId) -> Vec<PaneId> {
        self.tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.root.pane_ids())
            .unwrap_or_default()
    }

    fn check_reservation(&self, base_revision: u64) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)
    }

    fn check_mutation(&self, base_revision: u64) -> Result<(), LayoutError> {
        if base_revision != self.revision {
            return Err(LayoutError::StaleRevision {
                expected: self.revision,
                got: base_revision,
            });
        }
        self.revision
            .checked_add(1)
            .ok_or(LayoutError::RevisionExhausted)?;
        Ok(())
    }

    fn require_member(&self, peer_id: &[u8]) -> Result<(), LayoutError> {
        self.members
            .iter()
            .any(|member| member.peer_id == peer_id)
            .then_some(())
            .ok_or(LayoutError::NotMember)
    }

    fn ensure_no_reservation(&self) -> Result<(), LayoutError> {
        self.pending_reservation
            .is_none()
            .then_some(())
            .ok_or(LayoutError::ReservationPending)
    }

    /// The pending reservation, if `host` is the peer it was handed to.
    ///
    /// Matched on the host rather than the creator: the peer that reports a
    /// pane ready is the peer that spawned it, and for a terminal opened on
    /// another machine those are not the same peer as the one that asked.
    fn match_reservation(
        &self,
        host: &[u8],
        reservation_id: ReservationId,
    ) -> Result<PendingReservation, LayoutError> {
        let pending = self
            .pending_reservation
            .as_ref()
            .ok_or(LayoutError::UnknownReservation { reservation_id })?;
        if pending.reservation_id != reservation_id {
            return Err(LayoutError::UnknownReservation { reservation_id });
        }
        if pending.host_peer_id != host {
            return Err(LayoutError::ReservationCreatorMismatch);
        }
        Ok(pending.clone())
    }

    fn validate_pane_create(&self, target_pane_id: PaneId) -> Result<(), LayoutError> {
        let tab_index =
            self.tab_index_for_pane(target_pane_id)
                .ok_or(LayoutError::UnknownPane {
                    pane_id: target_pane_id,
                })?;
        if self.pane_ids_in_tab_at(tab_index).len() >= MAX_PANES_PER_TAB {
            return Err(LayoutError::PaneLimit);
        }
        if self.tabs[tab_index]
            .root
            .leaf_depth(target_pane_id)
            .expect("found target leaf")
            >= MAX_SPLIT_DEPTH
        {
            return Err(LayoutError::SplitDepthLimit);
        }
        Ok(())
    }

    fn advance_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("check_mutation verified revision can advance");
    }

    fn next_id(&self, id: u64) -> Result<u64, LayoutError> {
        id.checked_add(1).ok_or(LayoutError::IdExhausted)
    }

    fn invalidate_reservation(&mut self) -> Option<InvalidatedReservation> {
        self.pending_reservation
            .take()
            .map(|reservation| InvalidatedReservation {
                reservation_id: reservation.reservation_id,
                creator_peer_id: reservation.creator_peer_id,
            })
    }

    fn tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.root.contains_leaf(pane_id))
    }

    fn pane_ids_in_tab_at(&self, tab_index: usize) -> Vec<PaneId> {
        self.tabs[tab_index].root.pane_ids()
    }
}

impl Node {
    fn contains_leaf(&self, wanted: PaneId) -> bool {
        match self {
            Self::Leaf { pane_id } => *pane_id == wanted,
            Self::Split { first, second, .. } => {
                first.contains_leaf(wanted) || second.contains_leaf(wanted)
            }
        }
    }

    fn leaf_depth(&self, wanted: PaneId) -> Option<usize> {
        match self {
            Self::Leaf { pane_id } => (*pane_id == wanted).then_some(0),
            Self::Split { first, second, .. } => first
                .leaf_depth(wanted)
                .or_else(|| second.leaf_depth(wanted))
                .map(|depth| depth + 1),
        }
    }

    fn pane_ids(&self) -> Vec<PaneId> {
        match self {
            Self::Leaf { pane_id } => vec![*pane_id],
            Self::Split { first, second, .. } => {
                let mut pane_ids = first.pane_ids();
                pane_ids.extend(second.pane_ids());
                pane_ids
            }
        }
    }

    fn collect_pane_ids(&self, depth: usize, pane_ids: &mut BTreeSet<PaneId>) -> bool {
        match self {
            Self::Leaf { pane_id } => *pane_id != 0 && pane_ids.insert(*pane_id),
            Self::Split { first, second, .. } => {
                depth < MAX_SPLIT_DEPTH
                    && first.collect_pane_ids(depth + 1, pane_ids)
                    && second.collect_pane_ids(depth + 1, pane_ids)
            }
        }
    }

    fn has_valid_ratios(&self) -> bool {
        match self {
            Self::Leaf { .. } => true,
            Self::Split {
                first_share_bps,
                first,
                second,
                ..
            } => {
                validate_first_share_bps(*first_share_bps).is_ok()
                    && first.has_valid_ratios()
                    && second.has_valid_ratios()
            }
        }
    }

    fn replace_leaf(&mut self, wanted: PaneId, replacement: Node) -> bool {
        match self {
            Self::Leaf { pane_id } if *pane_id == wanted => {
                *self = replacement;
                true
            }
            Self::Leaf { .. } => false,
            Self::Split { first, second, .. } => {
                first.replace_leaf(wanted, replacement.clone())
                    || second.replace_leaf(wanted, replacement)
            }
        }
    }

    fn set_nearest_split_ratio(
        &mut self,
        wanted: PaneId,
        axis: Axis,
        first_share_bps: u16,
    ) -> bool {
        match self {
            Self::Leaf { .. } => false,
            Self::Split {
                axis: split_axis,
                first,
                second,
                ..
            } => {
                if first.contains_leaf(wanted)
                    && first.set_nearest_split_ratio(wanted, axis, first_share_bps)
                {
                    return true;
                }
                if second.contains_leaf(wanted)
                    && second.set_nearest_split_ratio(wanted, axis, first_share_bps)
                {
                    return true;
                }
                if *split_axis == axis
                    && (first.contains_leaf(wanted) || second.contains_leaf(wanted))
                {
                    if let Self::Split {
                        first_share_bps: share,
                        ..
                    } = self
                    {
                        *share = first_share_bps;
                    }
                    return true;
                }
                false
            }
        }
    }

    fn remove_leaf(self, wanted: PaneId) -> Option<Node> {
        match self {
            Self::Leaf { pane_id } => (pane_id != wanted).then_some(Self::Leaf { pane_id }),
            Self::Split {
                axis,
                first_share_bps,
                first,
                second,
            } => {
                if first.contains_leaf(wanted) {
                    match first.remove_leaf(wanted) {
                        Some(first) => Some(Self::Split {
                            axis,
                            first_share_bps,
                            first: Box::new(first),
                            second,
                        }),
                        None => Some(*second),
                    }
                } else if second.contains_leaf(wanted) {
                    match second.remove_leaf(wanted) {
                        Some(second) => Some(Self::Split {
                            axis,
                            first_share_bps,
                            first,
                            second: Box::new(second),
                        }),
                        None => Some(*first),
                    }
                } else {
                    Some(Self::Split {
                        axis,
                        first_share_bps,
                        first,
                        second,
                    })
                }
            }
        }
    }
}

pub fn validate_first_share_bps(first_share_bps: u16) -> Result<(), LayoutError> {
    if !(MIN_FIRST_SHARE_BPS..=MAX_FIRST_SHARE_BPS).contains(&first_share_bps) {
        return Err(LayoutError::InvalidSplitRatio);
    }
    Ok(())
}

fn validate_grid(grid_rows: u16, grid_cols: u16) -> Result<(), LayoutError> {
    (grid_rows > 0 && grid_cols > 0)
        .then_some(())
        .ok_or(LayoutError::InvalidGrid)
}

fn validate_display_name(display_name: &str) -> Result<(), LayoutError> {
    if display_name.chars().count() > 32 || display_name.chars().any(char::is_control) {
        return Err(LayoutError::InvalidDisplayName);
    }
    Ok(())
}

pub fn normalize_title(title: &str) -> Result<Option<String>, LayoutError> {
    let normalized = title.trim();
    if normalized.is_empty() {
        return Ok(None);
    }
    if normalized.chars().count() > 32 || normalized.chars().any(char::is_control) {
        return Err(LayoutError::InvalidTitle);
    }
    Ok(Some(normalized.to_owned()))
}

fn is_normalized_title(title: &Option<String>) -> bool {
    match title {
        None => true,
        Some(title) => {
            normalize_title(title).is_ok_and(|normalized| normalized.as_deref() == Some(title))
        }
    }
}

fn validate_endpoint_addr(endpoint_addr: &[u8]) -> Result<(), LayoutError> {
    (!endpoint_addr.is_empty() && endpoint_addr.len() <= MAX_ENDPOINT_ADDR_BYTES)
        .then_some(())
        .ok_or(LayoutError::InvalidEndpointAddress)
}

fn validate_peer_id(peer_id: &[u8]) -> Result<(), LayoutError> {
    (!peer_id.is_empty() && peer_id.len() <= MAX_PEER_ID_BYTES)
        .then_some(())
        .ok_or(LayoutError::InvalidPeerId)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: &[u8] = b"host-a";
    const HOST_B: &[u8] = b"host-b";

    fn state() -> SessionState {
        SessionState::new(HOST_A.to_vec(), b"endpoint-a".to_vec(), 24, 80).unwrap()
    }

    #[test]
    fn titles_normalize_and_validate_at_the_layout_boundary() {
        assert_eq!(
            normalize_title("  build logs\u{2003}").unwrap(),
            Some("build logs".into())
        );
        assert_eq!(normalize_title(" \t ").unwrap(), None);
        assert_eq!(
            normalize_title(&"x".repeat(32)).unwrap(),
            Some("x".repeat(32))
        );
        assert_eq!(
            normalize_title(&"x".repeat(33)),
            Err(LayoutError::InvalidTitle)
        );
        assert_eq!(
            normalize_title("line\nbreak"),
            Err(LayoutError::InvalidTitle)
        );
    }

    /// A box paired while a session is already open — the ordinary way to add
    /// a machine — announced `Unspecified` for the life of its node, and
    /// `pin_peers` will not write a peer that has not said it is a machine into
    /// any fleet. So the machine you just paired never made it into a record
    /// and its row lasted exactly as long as the session did.
    #[test]
    fn a_member_may_say_what_it_is_once_and_never_take_it_back() {
        let mut state = state();
        state
            .add_member(state.revision(), HOST_B.to_vec(), b"endpoint-b".to_vec())
            .unwrap();
        let member = |state: &SessionState| {
            state
                .members()
                .iter()
                .find(|member| member.peer_id == HOST_B)
                .expect("member")
                .kind
        };
        assert_eq!(member(&state), MemberKind::Unspecified);

        let before = state.revision();
        assert!(
            state
                .update_member_kind(state.revision(), HOST_B, MemberKind::Machine)
                .unwrap()
                .is_some()
        );
        assert_eq!(member(&state), MemberKind::Machine);
        assert!(state.revision() > before, "the fleet has to hear about it");

        // Said twice is said once: a claim resent on a timer must not cost a
        // revision, or a quiet session would commit forever.
        let settled = state.revision();
        assert!(
            state
                .update_member_kind(state.revision(), HOST_B, MemberKind::Machine)
                .unwrap()
                .is_none()
        );
        assert_eq!(state.revision(), settled);

        // And never taken back. "Is this one of my machines" must not have an
        // answer that depends on when it was asked.
        assert!(
            state
                .update_member_kind(state.revision(), HOST_B, MemberKind::Person)
                .unwrap()
                .is_none()
        );
        assert_eq!(member(&state), MemberKind::Machine);
        assert_eq!(state.revision(), settled);

        // `Unspecified` is what a peer says when it is claiming nothing, so it
        // is never a declaration.
        state
            .update_member_kind(state.revision(), HOST_A, MemberKind::Unspecified)
            .unwrap();
        assert_eq!(
            state
                .members()
                .iter()
                .find(|member| member.peer_id == HOST_A)
                .expect("coordinator")
                .kind,
            MemberKind::Unspecified
        );
        assert_eq!(
            state.update_member_kind(state.revision(), b"stranger", MemberKind::Machine),
            Err(LayoutError::NotMember)
        );
    }

    #[test]
    fn any_member_can_rename_remote_panes_and_tabs_or_clear_them() {
        let mut state = state();
        state
            .add_member(state.revision(), HOST_B.to_vec(), b"endpoint-b".to_vec())
            .unwrap();
        let roots = state.tabs().to_vec();
        let hosts = state
            .panes()
            .map(|pane| (pane.pane_id, pane.host_peer_id.clone()))
            .collect::<Vec<_>>();

        state
            .rename_pane(HOST_B, state.revision(), 1, " remote pane ".into())
            .unwrap();
        assert_eq!(state.pane(1).unwrap().title.as_deref(), Some("remote pane"));
        assert_eq!(state.tabs(), roots.as_slice());
        assert_eq!(
            state
                .panes()
                .map(|pane| (pane.pane_id, pane.host_peer_id.clone()))
                .collect::<Vec<_>>(),
            hosts
        );

        let tab_id = state.create_tab(HOST_B, state.revision(), 24, 80).unwrap();
        state
            .rename_tab(HOST_A, state.revision(), tab_id, "tab title".into())
            .unwrap();
        assert_eq!(
            state
                .tabs()
                .iter()
                .find(|tab| tab.tab_id == tab_id)
                .unwrap()
                .title
                .as_deref(),
            Some("tab title")
        );
        state
            .rename_pane(HOST_A, state.revision(), 1, "  ".into())
            .unwrap();
        assert_eq!(state.pane(1).unwrap().title, None);
    }

    #[test]
    fn title_renames_reject_invalid_targets_stale_revisions_and_reservations() {
        let mut state = state();
        assert_eq!(
            state.rename_pane(HOST_A, state.revision(), 99, "missing".into()),
            Err(LayoutError::UnknownPane { pane_id: 99 })
        );
        assert_eq!(
            state.rename_tab(HOST_A, 0, 1, "stale".into()),
            Err(LayoutError::StaleRevision {
                expected: 1,
                got: 0
            })
        );
        state.reserve_tab(HOST_A, state.revision(), 24, 80).unwrap();
        assert_eq!(
            state.rename_tab(HOST_A, state.revision(), 1, "blocked".into()),
            Err(LayoutError::ReservationPending)
        );

        let mut snapshot = state.snapshot();
        snapshot.tabs[0].title = Some(String::new());
        assert_eq!(
            SessionState::validate_snapshot(&snapshot),
            Err(LayoutError::InvalidSnapshot)
        );
        snapshot.tabs[0].title = Some(" untrimmed ".into());
        assert_eq!(
            SessionState::validate_snapshot(&snapshot),
            Err(LayoutError::InvalidSnapshot)
        );
    }
}
