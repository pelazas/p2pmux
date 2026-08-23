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
