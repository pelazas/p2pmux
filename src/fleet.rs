//! Where a fleet is meeting, and how a machine that was asleep finds out.
//!
//! A fleet is permanent — the machines you own — and a session is not. Until
//! this module existed, the two were the same field: `pairing.toml` held one
//! session ticket, written once by `p2pmux pair`, and every machine dialled it
//! forever. That works exactly as long as the session it names is alive.
//!
//! When that session ends, the record is a corpse and nothing updates it. The
//! machine that is *away* cannot be told: a member learns about new sessions
//! from the announcements that travel inside a session it has already joined,
//! so a machine that cannot join hears nothing, and a machine that hears
//! nothing cannot join. It is not slow to recover — it never recovers. On
//! 2026-08-16 two healthy machines chased a session that had not existed for
//! days, and the only exit was a person re-running `p2pmux pair`.
//!
//! So the fleet gets an address of its own. It is a secret rather than a
//! location: 32 random bytes minted once, held by every member, and never
//! rewritten. From it both halves of one record in the blind store are derived
//! — see [`crate::hosted_rendezvous`] — and that record holds the ticket of
//! whichever session the fleet is meeting in right now.
//!
//! - A machine that wants its fleet reads the record and dials what it says.
//! - A machine that is hosting the fleet's session writes it, and refreshes it.
//! - A machine that cannot reach what the record says starts a session and
//!   publishes that instead, which is how the role moves when a laptop shuts.
//!
//! Nothing here expires on a timer, and no human has to intervene: the record
//! is a hint that corrects itself the moment it turns out to be wrong.
//!
//! **On trust.** The store is blind — it holds an opaque index and an opaque
//! blob, and can read neither the fleet key nor the ticket. Writing a record
//! anyone will believe therefore needs the fleet key, and the fleet key is
//! already the thing that admits a machine to the fleet: anybody who has it can
//! enrol, so being able to redirect members costs them nothing they did not
//! already have. The AEAD tag is the authentication, and no second signature
//! would add anything. What redirection still cannot do is grant anything —
//! being in a session with your machines is not permission to start work on
//! them, which stays with each machine's own `[work].allow`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::hosted_rendezvous::RecordLocator;

/// Domain separation from the join-code records that share the store. Two
/// contexts, so the index the server sees says nothing about the key.
const INDEX_CONTEXT: &str = "p2pmux fleet rendezvous 2026-08-23 record index";
const KEY_CONTEXT: &str = "p2pmux fleet rendezvous 2026-08-23 record key";

/// 32 bytes. This is not a code anybody retypes, so there is no reason to
/// shorten it towards the ~50 bits a join code lives with — and every reason
/// not to, since it is long-lived and a successful guess is a standing
/// invitation to the fleet.
const KEY_BYTES: usize = 32;

#[derive(Debug, Eq, PartialEq)]
pub enum FleetKeyError {
    /// Not [`KEY_BYTES`] of hex.
    Malformed,
    /// The OS refused to provide randomness.
    Random,
}

impl fmt::Display for FleetKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            // Deliberately does not echo the input: this is a credential, and the
            // message reaches logs and shared panes.
            Self::Malformed => "that is not a p2pmux fleet key",
            Self::Random => "could not generate a fleet key",
        })
    }
}

impl std::error::Error for FleetKeyError {}

/// The secret that names a fleet's record, held by every member.
///
/// Treated exactly as a join code is: never printed in an error, and redacted
/// in [`fmt::Debug`] so a stray `dbg!` or a structured log line cannot leak it.
#[derive(Clone, Eq, PartialEq)]
pub struct FleetKey(String);

impl FleetKey {
    pub fn mint() -> Result<Self, FleetKeyError> {
        let mut bytes = [0_u8; KEY_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| FleetKeyError::Random)?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    /// Parse the stored form. Case-insensitive, because it round-trips through
    /// a TOML file a person is allowed to edit.
    pub fn parse(input: &str) -> Result<Self, FleetKeyError> {
        let canonical = input.trim().to_ascii_lowercase();
        if canonical.len() != KEY_BYTES * 2
            || !canonical.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(FleetKeyError::Malformed);
        }
        Ok(Self(canonical))
    }

    /// The form written to `pairing.toml` and carried in an invitation.
    pub fn hex(&self) -> &str {
        &self.0
    }

    /// The one record this fleet meets at.
    pub fn locator(&self) -> RecordLocator {
        RecordLocator::derive(INDEX_CONTEXT, KEY_CONTEXT, self.0.as_bytes())
    }
}

impl fmt::Debug for FleetKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FleetKey(<redacted>)")
    }
}

/// What the fleet record says: where the fleet is meeting, and who said so.
///
/// JSON rather than the bare ticket a join code's record holds, because this
/// one has to answer a second question — *is this mine?* — that decides whether
/// a coordinator refreshes the record or leaves it to whoever owns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FleetRecord {
    /// The session ticket to dial.
    pub ticket: String,
    /// The proved machine key of whoever published this, so a coordinator can
    /// tell its own record from another machine's. Not a permission: a member
    /// that finds somebody else's record simply does not overwrite it.
    #[serde(default)]
    pub host: String,
    /// Unix seconds, for `p2pmux machines` to say how fresh this is. Nothing
    /// decides anything on it — staleness is discovered by dialling and
    /// failing, which is the only test that does not need two clocks to agree.
    #[serde(default)]
    pub published_at: u64,
}

impl FleetRecord {
    pub fn encode(&self) -> String {
        // `unwrap` is unreachable: every field is a String or a u64.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Read a record back.
    ///
    /// A record this version cannot parse is treated as no record at all, which
    /// is the same thing a *dead* record already degrades to: the caller starts
    /// a session and publishes over it. That is what keeps a future field from
    /// stranding an old binary the way the session ticket used to.
    pub fn decode(raw: &str) -> Option<Self> {
        let record: Self = serde_json::from_str(raw).ok()?;
        (!record.ticket.trim().is_empty()).then_some(record)
    }
}

/// A second, independent record that a pairing code also names.
///
/// `p2pmux pair` has to hand over the fleet's address, and the obvious place —
/// inside the record its code already points at — is the one place it must not
/// go. That record holds a bare ticket, and a build from last month reads it
/// expecting exactly that; putting a structure there would make an old machine
/// refuse a code a new one printed, on the single command whose entire job is
/// introducing two machines.
///
/// So the address goes in a sibling record under its own contexts. An old
/// client fetches the ticket and never knows this exists; a new one fetches
/// both and finds nothing here when the machine that printed the code was old.
/// Neither has to know what the other is.
const HANDOVER_INDEX_CONTEXT: &str = "p2pmux fleet handover 2026-08-23 record index";
const HANDOVER_KEY_CONTEXT: &str = "p2pmux fleet handover 2026-08-23 record key";

fn handover_locator(code: &crate::hosted_rendezvous::JoinCode) -> RecordLocator {
    RecordLocator::derive(
        HANDOVER_INDEX_CONTEXT,
        HANDOVER_KEY_CONTEXT,
        code.canonical().as_bytes(),
    )
}

/// Offer this fleet's address to whoever types `code`.
///
/// Only ever a code minted for a pairing, never the session's own join code.
/// They are different credentials and were only ever one string by accident:
/// a guest you hand a join code to could otherwise read the address of the
/// fleet they were invited to sit beside.
pub async fn offer_handover(
    code: &crate::hosted_rendezvous::JoinCode,
    key: &FleetKey,
) -> Result<(), crate::hosted_rendezvous::PublishError> {
    crate::hosted_rendezvous::HostedRendezvous::new()?
        .publish_at(&handover_locator(code), key.hex())
        .await
}

/// Take the fleet address a pairing code offers, if it offers one.
///
/// `None` for a code printed by a build that had no address to hand over, which
/// is not a failure: the machine pairs on the ticket as it always did, and picks
/// an address up from the first session it shares with a machine that has one.
pub async fn accept_handover(code: &crate::hosted_rendezvous::JoinCode) -> Option<FleetKey> {
    let store = crate::hosted_rendezvous::HostedRendezvous::new().ok()?;
    let raw = store.resolve_at(&handover_locator(code)).await.ok()?;
    FleetKey::parse(&raw).ok()
}

/// Why a fleet's meeting place could not be read.
#[derive(Debug)]
pub enum LocateError {
    /// The store answered, and nothing is there. Nobody in this fleet is
    /// hosting a session right now — which is an ordinary state, not a fault.
    ///
    /// A record this build cannot parse arrives here too. It means the same
    /// thing to every caller: there is nowhere to go, so make somewhere.
    Nobody,
    /// The store could not be asked. Distinct from [`Self::Nobody`] because the
    /// fleet may well be meeting somewhere and this machine simply cannot find
    /// out — the difference between "start a session" and "say why not".
    Unreachable(String),
}

impl fmt::Display for LocateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nobody => formatter.write_str("no machine in this fleet is hosting a session"),
            Self::Unreachable(detail) => {
                write!(formatter, "could not reach the fleet directory ({detail})")
            }
        }
    }
}

impl std::error::Error for LocateError {}

/// Where this fleet is meeting, according to the record.
///
/// The answer is a hint, not a promise. The machine that published it may have
/// shut its laptop a second later, and no amount of freshness checking here
/// would find that out — dialling it is the only test that means anything. A
/// caller that fails to reach what this returns should publish its own session
/// over the top; see this module's header.
pub async fn locate(key: &FleetKey) -> Result<FleetRecord, LocateError> {
    let store = crate::hosted_rendezvous::HostedRendezvous::new()
        .map_err(|error| LocateError::Unreachable(error.to_string()))?;
    match store.resolve_at(&key.locator()).await {
        Ok(raw) => FleetRecord::decode(&raw).ok_or(LocateError::Nobody),
        Err(crate::hosted_rendezvous::ResolveError::NotFound) => Err(LocateError::Nobody),
        // A record we cannot open is one sealed under a different key, which
        // for a fleet means the file was hand-edited or two builds disagree.
        // Either way there is nothing here for us, and taking over is right.
        Err(crate::hosted_rendezvous::ResolveError::WrongCode) => Err(LocateError::Nobody),
        Err(error) => Err(LocateError::Unreachable(error.to_string())),
    }
}

/// Say that this fleet is meeting where `record` says.
pub async fn publish(
    key: &FleetKey,
    record: &FleetRecord,
) -> Result<(), crate::hosted_rendezvous::PublishError> {
    crate::hosted_rendezvous::HostedRendezvous::new()?
        .publish_at(&key.locator(), &record.encode())
        .await
}

/// Take the record down, on a clean exit.
///
/// Best effort, and deliberately so: a member that finds a dead ticket here
/// publishes over it, so the cost of failing is one wasted dial rather than a
/// stranded fleet.
pub async fn withdraw(key: &FleetKey) -> Result<(), crate::hosted_rendezvous::PublishError> {
    crate::hosted_rendezvous::HostedRendezvous::new()?
        .remove_at(&key.locator())
        .await
}

/// A fleet record held open for as long as this machine is hosting the session.
///
/// The store expires a record that stops being refreshed, which is what keeps a
/// machine that was unplugged from advertising a session that died with it.
/// A machine that is still hosting therefore has to keep saying so, and say
/// otherwise on the way out by deleting the record outright.
pub struct HostedFleet {
    key: FleetKey,
    refresh: tokio::task::JoinHandle<()>,
}

impl HostedFleet {
    /// Publish `record` and hold its TTL open until [`Self::retire`].
    pub async fn claim(
        key: FleetKey,
        record: FleetRecord,
    ) -> Result<Self, crate::hosted_rendezvous::PublishError> {
        publish(&key, &record).await?;
        let refresh = tokio::spawn({
            let key = key.clone();
            async move {
                loop {
                    tokio::time::sleep(crate::hosted_rendezvous::REFRESH_INTERVAL).await;
                    // A failed refresh is not worth retrying tightly: the record
                    // still has most of its TTL, and the next tick is well
                    // inside it. A fleet whose record does lapse is not stranded
                    // either — the next member to look takes over.
                    let _ = publish(&key, &record).await;
                }
            }
        });
        Ok(Self { key, refresh })
    }

    /// Stop refreshing and take the record down.
    pub async fn retire(self) {
        self.refresh.abort();
        let _ = withdraw(&self.key).await;
    }
}

/// How long after a failed publish it is worth trying again.
///
/// The node ticks this every couple of seconds, which is the right cadence for
/// noticing that a session changed hands and quite the wrong one for hammering
/// a store that just refused us.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Keeps the fleet's record pointing at the session this node is hosting.
///
/// Driven from the node's peer-scan tick, which is where the answer can change:
/// a coordinator that steps down stops publishing, a member promoted in its
/// place starts, and a machine paired while the node was already running
/// acquires a fleet to publish to. Reading those off the session descriptor —
/// which failover already keeps current — means this needs no notion of role
/// changes of its own.
///
/// It never takes the record down. A step-down and a shutdown both look like a
/// coordinator that has stopped refreshing, and in both cases another machine
/// is about to publish its own session; a withdrawal racing that would leave
/// the fleet with no address at all, which is the exact failure this module was
/// written to end. Letting the record expire instead costs a member one dial
/// that fails, after which it takes over. One is self-correcting and the other
/// is not.
pub struct FleetHost {
    handle: tokio::runtime::Handle,
    state: std::sync::Arc<std::sync::Mutex<Claim>>,
}

#[derive(Default)]
struct Claim {
    /// The fleet key and ticket last written to the store.
    published: Option<(String, String)>,
    written_at: Option<std::time::Instant>,
    /// When a failed attempt may be repeated.
    retry_at: Option<std::time::Instant>,
    /// A publish is in the air. Without this the tick would start a second one
    /// every two seconds for as long as the first was slow.
    busy: bool,
}

impl FleetHost {
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            state: std::sync::Arc::new(std::sync::Mutex::new(Claim::default())),
        }
    }

    /// What this node should be advertising, if anything.
    ///
    /// Three conditions, and every one of them has a session it is there to
    /// exclude: `p2pmux create` (not the fleet's home), a member of the home
    /// session (the coordinator publishes, not the room), and a machine with no
    /// fleet to publish to.
    fn wanted(descriptor: &crate::session_store::SessionDescriptor) -> Option<(FleetKey, String)> {
        Self::wanted_with(descriptor, crate::pairing::load_or_empty().fleet_key())
    }

    /// The decision itself, with the pairing record handed in.
    ///
    /// Split from [`Self::wanted`] so it can be tested without a config
    /// directory: pointing `XDG_CONFIG_HOME` at a temporary path is the kind of
    /// setup that quietly stops applying and leaves a test passing against the
    /// developer's own fleet.
    fn wanted_with(
        descriptor: &crate::session_store::SessionDescriptor,
        key: Option<FleetKey>,
    ) -> Option<(FleetKey, String)> {
        if !descriptor.hosts_fleet
            || descriptor.role != crate::session_store::SessionRole::Coordinator
        {
            return None;
        }
        Some((key?, descriptor.ticket.clone()?))
    }

    /// Called from the node's peer scan. Cheap unless something changed.
    pub fn tick(&self, descriptor: &crate::session_store::SessionDescriptor) {
        let Some((key, ticket)) = Self::wanted(descriptor) else {
            return;
        };
        let now = std::time::Instant::now();
        {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.busy || state.retry_at.is_some_and(|retry| now < retry) {
                return;
            }
            let current = (key.hex().to_owned(), ticket.clone());
            let fresh = state.published.as_ref() == Some(&current)
                && state.written_at.is_some_and(|at| {
                    now.duration_since(at) < crate::hosted_rendezvous::REFRESH_INTERVAL
                });
            if fresh {
                return;
            }
            state.busy = true;
        }
        let state = self.state.clone();
        let record = FleetRecord {
            ticket: ticket.clone(),
            host: crate::machine_id::to_hex(&crate::machine_id::machine_id()),
            published_at: crate::pairing::now_unix(),
        };
        self.handle.spawn(async move {
            let outcome = publish(&key, &record).await;
            let Ok(mut state) = state.lock() else {
                return;
            };
            state.busy = false;
            match outcome {
                Ok(()) => {
                    state.published = Some((key.hex().to_owned(), record.ticket));
                    state.written_at = Some(std::time::Instant::now());
                    state.retry_at = None;
                }
                // Said once rather than every tick. A fleet whose record is
                // stale is not broken — members that cannot reach the old
                // address publish their own over it — so this is a note, not
                // an alarm.
                Err(error) => {
                    if state.retry_at.is_none() {
                        eprintln!(
                            "p2pmux node: could not say where this fleet is meeting: {error}"
                        );
                    }
                    state.retry_at = Some(std::time::Instant::now() + RETRY_AFTER);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_key_parses_back_and_is_never_shown_by_debug() {
        let key = FleetKey::mint().expect("key should mint");

        assert_eq!(key.hex().len(), KEY_BYTES * 2);
        assert_eq!(FleetKey::parse(key.hex()).expect("should parse"), key);
        assert_eq!(format!("{key:?}"), "FleetKey(<redacted>)");
    }

    #[test]
    fn a_key_survives_the_round_trip_through_a_file_a_person_may_have_touched() {
        let key = FleetKey::mint().expect("key should mint");

        for variant in [
            key.hex().to_owned(),
            key.hex().to_uppercase(),
            format!("  {}  \n", key.hex()),
        ] {
            assert_eq!(
                FleetKey::parse(&variant).expect("variant should parse"),
                key,
                "variant {variant:?} did not normalise"
            );
        }
    }

    #[test]
    fn malformed_keys_are_rejected_without_echoing_them() {
        for input in ["", "short", &"z".repeat(64), &"a".repeat(63)] {
            let error = FleetKey::parse(input).expect_err("should be rejected");
            assert_eq!(error, FleetKeyError::Malformed);
            if !input.is_empty() {
                assert!(!error.to_string().contains(input));
            }
        }
    }

    #[test]
    fn distinct_fleets_land_on_distinct_records() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let key = FleetKey::mint().expect("key should mint");
            assert!(
                seen.insert(key.locator().index().to_owned()),
                "index collision within 256 fleets"
            );
        }
    }

    #[test]
    fn a_fleet_record_survives_the_store() {
        let key = FleetKey::mint().expect("key should mint");
        let record = FleetRecord {
            ticket: "p2pmux-v3:TICKET".to_owned(),
            host: "machine".to_owned(),
            published_at: 1_787_474_694,
        };

        let sealed = key
            .locator()
            .seal(&record.encode())
            .expect("record should seal");
        let opened = key.locator().open(&sealed).expect("record should open");

        assert_eq!(FleetRecord::decode(&opened), Some(record));
    }

    #[test]
    fn another_fleet_cannot_open_this_ones_record() {
        let key = FleetKey::mint().expect("key should mint");
        let other = FleetKey::mint().expect("key should mint");
        let sealed = key.locator().seal("{}").expect("record should seal");

        assert!(other.locator().open(&sealed).is_err());
    }

    /// A coordinator of the fleet's home session, which is the one shape that
    /// should publish.
    fn hosting_descriptor() -> crate::session_store::SessionDescriptor {
        let mut descriptor = crate::session_store::SessionDescriptor::new(
            "0123456789abcdef0123456789abcdef".to_owned(),
            "warsaw".to_owned(),
            std::path::PathBuf::from("/tmp/p2pmux-test.sock"),
            1,
            crate::session_store::SessionRole::Coordinator,
        );
        descriptor.hosts_fleet = true;
        descriptor.ticket = Some("p2pmux-v3:TICKET".to_owned());
        descriptor
    }

    #[test]
    fn the_coordinator_of_the_fleets_home_session_publishes_it() {
        let key = FleetKey::mint().expect("key should mint");

        assert_eq!(
            FleetHost::wanted_with(&hosting_descriptor(), Some(key.clone())),
            Some((key, "p2pmux-v3:TICKET".to_owned()))
        );
    }

    #[test]
    fn a_session_that_is_not_the_fleets_home_never_publishes_itself() {
        // `p2pmux create`, or a guest's join. Publishing it would move the whole
        // fleet into a session that was never meant to be one — and the machine
        // that did it would be the only one that knew.
        let key = FleetKey::mint().expect("key should mint");
        let mut descriptor = hosting_descriptor();
        descriptor.hosts_fleet = false;

        assert_eq!(FleetHost::wanted_with(&descriptor, Some(key)), None);
    }

    #[test]
    fn a_member_of_the_home_session_leaves_publishing_to_its_coordinator() {
        let key = FleetKey::mint().expect("key should mint");
        let mut descriptor = hosting_descriptor();
        descriptor.role = crate::session_store::SessionRole::Member;

        assert_eq!(FleetHost::wanted_with(&descriptor, Some(key)), None);
    }

    #[test]
    fn a_machine_with_no_fleet_has_nowhere_to_publish() {
        assert_eq!(FleetHost::wanted_with(&hosting_descriptor(), None), None);
    }

    #[test]
    fn a_coordinator_without_a_ticket_yet_says_nothing_rather_than_something_empty() {
        // There is a window during startup where the role is set and the ticket
        // is not. Publishing an empty ticket there would point the fleet at
        // nothing and look exactly like a fleet that is meeting somewhere.
        let key = FleetKey::mint().expect("key should mint");
        let mut descriptor = hosting_descriptor();
        descriptor.ticket = None;

        assert_eq!(FleetHost::wanted_with(&descriptor, Some(key)), None);
    }

    #[test]
    fn a_record_without_a_ticket_is_no_record() {
        // Both halves of the same rule: a blob this version cannot read must
        // degrade to "nobody is hosting" so the caller takes over, rather than
        // to an error that leaves the fleet with nowhere to go.
        assert_eq!(FleetRecord::decode("not json at all"), None);
        assert_eq!(FleetRecord::decode(r#"{"ticket":"  "}"#), None);
        assert_eq!(
            FleetRecord::decode(r#"{"ticket":"p2pmux-v3:T","unknown_future_field":1}"#)
                .map(|record| record.ticket),
            Some("p2pmux-v3:T".to_owned())
        );
    }
}
