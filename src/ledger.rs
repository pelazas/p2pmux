//! The append-only, authenticated record of everything that happened to a session's layout.
//!
//! A `LayoutCommit` used to be a bare snapshot: the whole layout, again, stamped with a
//! revision number. That form cannot answer the two questions that matter once a session
//! outlives the people watching it -- *who did this* and *is this the whole story* -- because
//! a snapshot has no author and no predecessor. Worse, the coordinator relays commits under
//! its own name, so a member had no way to attribute a change to anyone at all.
//!
//! Entries fix both. Each names its author, carries the request that author made, and hashes
//! the entry before it, so the sequence a member receives is checkable rather than merely
//! plausible: a dropped, reordered, or rewritten entry breaks the chain, and a forged one
//! fails a signature. The materialized layout still travels alongside as a checkpoint,
//! because that is what renderers draw, but the entry is the thing that is true.

use blake3::Hasher;
use iroh::{PublicKey, SecretKey, Signature};

use crate::protocol::{
    LEDGER_HASH_BYTES, LEDGER_SIGNATURE_BYTES, LedgerEntry, LedgerEntryKind,
    MAX_LEDGER_PAYLOAD_BYTES,
};

/// What `prev_hash` holds in the first entry, which has nothing behind it.
pub const GENESIS_PREV_HASH: [u8; LEDGER_HASH_BYTES] = [0; LEDGER_HASH_BYTES];

/// Domain tag for the hash the coordinator signs to fix an entry's position.
const ENTRY_DOMAIN: &[u8] = b"p2pmux/ledger/entry/v1";
/// Domain tag for the hash an author signs to prove it asked for a change.
///
/// Deliberately different from `ENTRY_DOMAIN`: the two hashes cover different fields and
/// mean different things, and a signature obtained for one must never be replayable as the
/// other.
const INTENT_DOMAIN: &[u8] = b"p2pmux/ledger/intent/v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerError {
    /// A field was missing, the wrong length, or an unknown enum value.
    Malformed(&'static str),
    /// The chain skipped, repeated, or rewound.
    OutOfOrder { expected: u64, found: u64 },
    /// The right position, but not built on the entry the verifier last saw.
    Forked,
    /// Not signed by the coordinator this member joined.
    BadCoordinatorSignature,
    /// The named author did not sign the request the entry attributes to them.
    BadAuthorSignature,
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(field) => write!(formatter, "malformed ledger entry: {field}"),
            Self::OutOfOrder { expected, found } => {
                write!(
                    formatter,
                    "ledger entry {found} arrived where {expected} was expected"
                )
            }
            Self::Forked => formatter.write_str("ledger entry does not follow the one before it"),
            Self::BadCoordinatorSignature => {
                formatter.write_str("ledger entry was not signed by this session's coordinator")
            }
            Self::BadAuthorSignature => {
                formatter.write_str("ledger entry's author did not sign the change it claims")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

/// The coordinator's end of the ledger: assigns positions and signs them.
pub struct LedgerWriter {
    session_id: Vec<u8>,
    secret: SecretKey,
    next_seq: u64,
    prev_hash: [u8; LEDGER_HASH_BYTES],
}

impl LedgerWriter {
    pub fn new(session_id: Vec<u8>, secret: SecretKey) -> Self {
        Self {
            session_id,
            secret,
            next_seq: 1,
            prev_hash: GENESIS_PREV_HASH,
        }
    }

    /// The position the next entry will take, and the hash it will name as its predecessor.
    pub fn head(&self) -> (u64, [u8; LEDGER_HASH_BYTES]) {
        (self.next_seq, self.prev_hash)
    }

    /// Seal one change into the ledger.
    ///
    /// `author_signature` is the author's own proof that it asked for this, forwarded
    /// unchanged from the request that carried it. It is empty for the changes nobody
    /// requested -- a member's connection dropping, a reservation timing out -- which the
    /// coordinator authors on its own account.
    pub fn append(
        &mut self,
        author_peer_id: Vec<u8>,
        kind: LedgerEntryKind,
        payload: Vec<u8>,
        author_signature: Vec<u8>,
    ) -> LedgerEntry {
        let mut entry = LedgerEntry {
            seq: self.next_seq,
            prev_hash: self.prev_hash.to_vec(),
            author_peer_id,
            kind: kind as i32,
            payload,
            author_signature,
            coordinator_signature: Vec::new(),
        };
        let hash = entry_hash(&self.session_id, &entry);
        entry.coordinator_signature = self.secret.sign(&hash).to_bytes().to_vec();
        self.next_seq = self.next_seq.saturating_add(1);
        self.prev_hash = hash;
        entry
    }
}

/// A member's end of the ledger: checks that what arrives is what the coordinator wrote.
///
/// The verifier starts trusting nothing but the coordinator's public key, which the join
/// handshake already tied to the ticket, and builds its position from the first entry it
/// sees. A member that joins mid-session cannot check the entries it was never sent, so it
/// anchors on the first one it receives and verifies every one after that.
pub struct LedgerVerifier {
    session_id: Vec<u8>,
    coordinator: PublicKey,
    expected_seq: Option<u64>,
    prev_hash: Option<[u8; LEDGER_HASH_BYTES]>,
}

impl LedgerVerifier {
    pub fn new(session_id: Vec<u8>, coordinator: PublicKey) -> Self {
        Self {
            session_id,
            coordinator,
            expected_seq: None,
            prev_hash: None,
        }
    }

    /// Check one entry and, if it holds up, advance to it.
    ///
    /// A rejected entry leaves the verifier where it was, so a caller that drops the
    /// connection and a caller that ignores the entry both stay consistent.
    pub fn accept(&mut self, entry: &LedgerEntry) -> Result<(), LedgerError> {
        let hash = self.check(entry)?;
        self.expected_seq = Some(entry.seq.saturating_add(1));
        self.prev_hash = Some(hash);
        Ok(())
    }

    fn check(&self, entry: &LedgerEntry) -> Result<[u8; LEDGER_HASH_BYTES], LedgerError> {
        if entry.seq == 0 {
            return Err(LedgerError::Malformed("seq"));
        }
        if entry.prev_hash.len() != LEDGER_HASH_BYTES {
            return Err(LedgerError::Malformed("prev_hash"));
        }
        if entry.author_peer_id.is_empty() {
            return Err(LedgerError::Malformed("author_peer_id"));
        }
        if entry.payload.len() > MAX_LEDGER_PAYLOAD_BYTES {
            return Err(LedgerError::Malformed("payload"));
        }
        let kind =
            LedgerEntryKind::try_from(entry.kind).map_err(|_| LedgerError::Malformed("kind"))?;
        if let Some(expected) = self.expected_seq
            && entry.seq != expected
        {
            return Err(LedgerError::OutOfOrder {
                expected,
                found: entry.seq,
            });
        }
        if let Some(prev_hash) = self.prev_hash
            && entry.prev_hash != prev_hash
        {
            return Err(LedgerError::Forked);
        }

        if !entry.author_signature.is_empty() {
            let author = public_key(&entry.author_peer_id)
                .ok_or(LedgerError::Malformed("author_peer_id"))?;
            let signature = signature(&entry.author_signature)
                .ok_or(LedgerError::Malformed("author_signature"))?;
            let intent = intent_hash(
                &self.session_id,
                &entry.author_peer_id,
                kind,
                &entry.payload,
            );
            author
                .verify(&intent, &signature)
                .map_err(|_| LedgerError::BadAuthorSignature)?;
        }

        let hash = entry_hash(&self.session_id, entry);
        let signature = signature(&entry.coordinator_signature)
            .ok_or(LedgerError::Malformed("coordinator_signature"))?;
        self.coordinator
            .verify(&hash, &signature)
            .map_err(|_| LedgerError::BadCoordinatorSignature)?;
        Ok(hash)
    }
}

/// The bytes an author signs to prove it asked for a change.
///
/// Position is deliberately absent: an author cannot know which sequence number its request
/// will land on, and the coordinator's own signature is what fixes that. The session id is
/// present so a signature harvested from one session cannot be replanted in another.
pub fn intent_hash(
    session_id: &[u8],
    author_peer_id: &[u8],
    kind: LedgerEntryKind,
    payload: &[u8],
) -> [u8; LEDGER_HASH_BYTES] {
    let mut hasher = Hasher::new();
    hasher.update(INTENT_DOMAIN);
    absorb(&mut hasher, session_id);
    absorb(&mut hasher, author_peer_id);
    hasher.update(&(kind as i32).to_le_bytes());
    absorb(&mut hasher, payload);
    *hasher.finalize().as_bytes()
}

/// The bytes the coordinator signs to fix an entry's place in the chain.
///
/// Covers the author's signature as well as the change, so an entry cannot be re-attributed
/// by swapping one signature for another and keeping the coordinator's.
pub fn entry_hash(session_id: &[u8], entry: &LedgerEntry) -> [u8; LEDGER_HASH_BYTES] {
    let mut hasher = Hasher::new();
    hasher.update(ENTRY_DOMAIN);
    absorb(&mut hasher, session_id);
    hasher.update(&entry.seq.to_le_bytes());
    absorb(&mut hasher, &entry.prev_hash);
    absorb(&mut hasher, &entry.author_peer_id);
    hasher.update(&entry.kind.to_le_bytes());
    absorb(&mut hasher, &entry.payload);
    absorb(&mut hasher, &entry.author_signature);
    *hasher.finalize().as_bytes()
}

/// Feed one variable-length field, length first.
///
/// Without the length, `("ab", "c")` and `("a", "bc")` would hash the same and two different
/// entries could share a signature.
fn absorb(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn public_key(bytes: &[u8]) -> Option<PublicKey> {
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    PublicKey::from_bytes(&bytes).ok()
}

fn signature(bytes: &[u8]) -> Option<Signature> {
    let bytes: [u8; LEDGER_SIGNATURE_BYTES] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SecretKey {
        SecretKey::from_bytes(&[seed; 32])
    }

    fn writer(seed: u8) -> LedgerWriter {
        LedgerWriter::new(b"session".to_vec(), key(seed))
    }

    fn verifier(seed: u8) -> LedgerVerifier {
        LedgerVerifier::new(b"session".to_vec(), key(seed).public())
    }

    fn append(writer: &mut LedgerWriter, payload: &[u8]) -> LedgerEntry {
        writer.append(
            key(9).public().as_bytes().to_vec(),
            LedgerEntryKind::LayoutChange,
            payload.to_vec(),
            Vec::new(),
        )
    }

    #[test]
    fn a_chain_the_coordinator_wrote_verifies_end_to_end() {
        let mut writer = writer(1);
        let mut verifier = verifier(1);

        for step in 0..4u8 {
            let entry = append(&mut writer, &[step]);
            assert_eq!(entry.seq, u64::from(step) + 1);
            assert_eq!(verifier.accept(&entry), Ok(()));
        }
    }

    #[test]
    fn the_first_entry_is_anchored_to_nothing() {
        let mut writer = writer(1);
        let entry = append(&mut writer, b"first");

        assert_eq!(entry.seq, 1);
        assert_eq!(entry.prev_hash, GENESIS_PREV_HASH.to_vec());
    }

    #[test]
    fn a_skipped_entry_is_refused_rather_than_quietly_accepted() {
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let first = append(&mut writer, b"one");
        let _dropped = append(&mut writer, b"two");
        let third = append(&mut writer, b"three");

        verifier.accept(&first).expect("first entry");
        assert_eq!(
            verifier.accept(&third),
            Err(LedgerError::OutOfOrder {
                expected: 2,
                found: 3
            })
        );
    }

    #[test]
    fn an_entry_rewritten_in_place_breaks_the_chain() {
        // The coordinator re-signs a different history from the same position. Sequence
        // numbers still line up; only the hash of what came before gives it away.
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let first = append(&mut writer, b"one");
        verifier.accept(&first).expect("first entry");

        let mut rival = LedgerWriter::new(b"session".to_vec(), key(1));
        let _other_first = append(&mut rival, b"something else");
        let rival_second = append(&mut rival, b"two");

        assert_eq!(rival_second.seq, 2);
        assert_eq!(verifier.accept(&rival_second), Err(LedgerError::Forked));
    }

    #[test]
    fn an_entry_signed_by_anybody_else_is_refused() {
        let mut impostor = writer(2);
        let mut verifier = verifier(1);
        let entry = append(&mut impostor, b"one");

        assert_eq!(
            verifier.accept(&entry),
            Err(LedgerError::BadCoordinatorSignature)
        );
    }

    #[test]
    fn editing_a_sealed_entry_invalidates_it() {
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let mut entry = append(&mut writer, b"delete pane 4");
        entry.payload = b"delete pane 7".to_vec();

        assert_eq!(
            verifier.accept(&entry),
            Err(LedgerError::BadCoordinatorSignature)
        );
    }

    #[test]
    fn reattributing_an_entry_invalidates_it() {
        // The interesting forgery is not editing the change but changing who is on record
        // as having asked for it.
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let mut entry = append(&mut writer, b"delete pane 4");
        entry.author_peer_id = key(8).public().as_bytes().to_vec();

        assert_eq!(
            verifier.accept(&entry),
            Err(LedgerError::BadCoordinatorSignature)
        );
    }

    #[test]
    fn a_rejected_entry_does_not_move_the_verifier_on() {
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let first = append(&mut writer, b"one");
        verifier.accept(&first).expect("first entry");

        let mut tampered = append(&mut writer, b"two");
        let good = tampered.clone();
        tampered.payload = b"forged".to_vec();
        assert!(verifier.accept(&tampered).is_err());

        // The real entry 2 still fits: a refusal must not have consumed its position.
        assert_eq!(verifier.accept(&good), Ok(()));
    }

    #[test]
    fn a_chain_from_another_session_does_not_verify_here() {
        let mut writer = LedgerWriter::new(b"other-session".to_vec(), key(1));
        let mut verifier = verifier(1);
        let entry = append(&mut writer, b"one");

        assert_eq!(
            verifier.accept(&entry),
            Err(LedgerError::BadCoordinatorSignature)
        );
    }

    #[test]
    fn an_author_signature_proves_who_asked() {
        let author = key(9);
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let payload = b"delete pane 4".to_vec();
        let intent = intent_hash(
            b"session",
            author.public().as_bytes(),
            LedgerEntryKind::LayoutChange,
            &payload,
        );
        let entry = writer.append(
            author.public().as_bytes().to_vec(),
            LedgerEntryKind::LayoutChange,
            payload,
            author.sign(&intent).to_bytes().to_vec(),
        );

        assert_eq!(verifier.accept(&entry), Ok(()));
    }

    #[test]
    fn a_coordinator_cannot_pin_a_change_on_a_member_who_did_not_sign_it() {
        let author = key(9);
        let bystander = key(8);
        let mut writer = writer(1);
        let mut verifier = verifier(1);
        let payload = b"delete pane 4".to_vec();
        // A signature the bystander really did make, for a change it really did ask for --
        // reused here under someone else's name.
        let intent = intent_hash(
            b"session",
            bystander.public().as_bytes(),
            LedgerEntryKind::LayoutChange,
            &payload,
        );
        let entry = writer.append(
            author.public().as_bytes().to_vec(),
            LedgerEntryKind::LayoutChange,
            payload,
            bystander.sign(&intent).to_bytes().to_vec(),
        );

        assert_eq!(
            verifier.accept(&entry),
            Err(LedgerError::BadAuthorSignature)
        );
    }

    #[test]
    fn an_intent_signature_does_not_double_as_a_position_signature() {
        // If the two hashes shared a domain, a member's own request signature would be a
        // valid coordinator signature for the same entry, and any member could seal
        // entries. The domain tags are what stop that.
        let payload = b"delete pane 4".to_vec();
        let author = key(9).public();
        let intent = intent_hash(
            b"session",
            author.as_bytes(),
            LedgerEntryKind::LayoutChange,
            &payload,
        );
        let entry = LedgerEntry {
            seq: 1,
            prev_hash: GENESIS_PREV_HASH.to_vec(),
            author_peer_id: author.as_bytes().to_vec(),
            kind: LedgerEntryKind::LayoutChange as i32,
            payload,
            author_signature: Vec::new(),
            coordinator_signature: Vec::new(),
        };

        assert_ne!(intent, entry_hash(b"session", &entry));
    }

    #[test]
    fn fields_cannot_be_slid_across_each_other() {
        // Length-prefixing is the only thing separating these two entries; without it the
        // concatenated bytes -- and so the signature -- would be identical.
        let left = LedgerEntry {
            seq: 1,
            prev_hash: GENESIS_PREV_HASH.to_vec(),
            author_peer_id: b"ab".to_vec(),
            kind: LedgerEntryKind::LayoutChange as i32,
            payload: b"c".to_vec(),
            author_signature: Vec::new(),
            coordinator_signature: Vec::new(),
        };
        let right = LedgerEntry {
            author_peer_id: b"a".to_vec(),
            payload: b"bc".to_vec(),
            ..left.clone()
        };

        assert_ne!(
            entry_hash(b"session", &left),
            entry_hash(b"session", &right)
        );
    }
}
