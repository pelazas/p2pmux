//! Short join codes that resolve to a ticket from any machine.
//!
//! The service this talks to is a blind store: it is handed an opaque index and an opaque
//! blob, and it never sees either the code or the ticket. That is a deliberate structural
//! choice rather than a hardening pass, because a ticket *is* the permission to join — see
//! [`crate::ticket`] — so a server holding plaintext tickets could walk into any session it
//! had ever brokered, on a product whose pitch is that nobody holds anyone else's keys.
//!
//! Given a code, the client derives two independent values from it:
//!
//! - `index = KDF("…index", code)` — where the record is stored, and all the server knows.
//! - `key   = KDF("…key",   code)` — what the record is sealed with, which never leaves here.
//!
//! Both derivations are domain-separated, so the index the server stores reveals nothing
//! usable about the key that opens the blob sitting at it. Learning every index in the store
//! still leaves an attacker with the original problem: guess the code.
//!
//! What this buys is UX, not capability. Connectivity already works without it — iroh
//! provides relays and node discovery, and a pasted ticket dials fine. The code exists so an
//! invite is ten characters instead of the ~170 a ticket runs to once it carries a relay URL
//! and a couple of direct addresses.

use std::{fmt, time::Duration};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};

/// Excludes `0`, `1`, `I`, `L` and `O`: a code gets read aloud and typed by hand, and those
/// are the pairs that get transcribed wrong.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Ten characters of a 31-symbol alphabet is a little under 50 bits.
///
/// The floor for this is set by online guessing against a rate-limited server rather than by
/// offline work, since a wrong guess yields a 404 and nothing to grind on. Even so, this
/// stays well above the ~40 bits that would make a sustained campaign interesting, because
/// the prize is a shell.
pub const CODE_LEN: usize = 10;

/// Where the printed form breaks. Reading `4KP7Q-M2XRW` aloud is materially easier than
/// reading ten unbroken characters, and the separator is stripped on the way back in.
const GROUP: usize = 5;

const KEY_CONTEXT: &str = "p2pmux hosted rendezvous 2026-07-30 record key";
const INDEX_CONTEXT: &str = "p2pmux hosted rendezvous 2026-07-30 record index";

/// Bytes of the index actually sent to the server, hex-encoded to 32 characters.
///
/// This is a lookup handle, not a commitment anyone verifies, so 128 bits is already far
/// more than the code behind it carries.
const INDEX_BYTES: usize = 16;

const NONCE_BYTES: usize = 12;

/// An upper bound on a sealed record, enforced on both sides so a malformed or hostile
/// response cannot make the client allocate freely.
pub const MAX_RECORD_BYTES: usize = 8 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum CodeError {
    /// Not `CODE_LEN` symbols of the alphabet, once separators and case are normalised.
    Malformed,
    /// The OS refused to provide randomness.
    Random,
}

impl fmt::Display for CodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            // Deliberately does not echo the input: a join code is a credential, and this
            // message reaches logs and shared panes.
            Self::Malformed => "that is not a p2pmux join code",
            Self::Random => "could not generate a join code",
        })
    }
}

impl std::error::Error for CodeError {}

#[derive(Debug, Eq, PartialEq)]
pub enum RecordError {
    /// The blob was not sealed by this code, or was altered in flight.
    Undecryptable,
    /// The record decrypted but does not hold a ticket.
    Malformed,
    /// Larger than [`MAX_RECORD_BYTES`].
    TooLarge,
    Random,
}

impl fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Undecryptable => "that join code does not open this record",
            Self::Malformed | Self::TooLarge => "the rendezvous record is not usable",
            Self::Random => "could not seal the rendezvous record",
        })
    }
}

impl std::error::Error for RecordError {}

/// A code in canonical form: `CODE_LEN` alphabet symbols, no separator.
///
/// Holding one is what lets a caller derive the index and the key, so it is treated as
/// secret: it is never included in an error, and its [`fmt::Debug`] is redacted so a stray
/// `dbg!` or a structured log line cannot leak a live invite.
#[derive(Clone, Eq, PartialEq)]
pub struct JoinCode(String);

impl JoinCode {
    pub fn mint() -> Result<Self, CodeError> {
        let mut code = String::with_capacity(CODE_LEN);
        // Rejection sampling: 256 is not a multiple of 31, so taking the raw byte modulo the
        // alphabet would make the first eight symbols measurably likelier than the rest.
        // Drawing a fresh byte instead of folding a biased one keeps every symbol uniform.
        let limit = (u8::MAX as usize / ALPHABET.len()) * ALPHABET.len();
        while code.len() < CODE_LEN {
            let mut chunk = [0_u8; 32];
            getrandom::fill(&mut chunk).map_err(|_| CodeError::Random)?;
            for byte in chunk {
                if code.len() == CODE_LEN {
                    break;
                }
                if (byte as usize) < limit {
                    code.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
                }
            }
        }
        Ok(Self(code))
    }

    /// Accepts what a human retypes: any case, with or without the printed separator, and
    /// with surrounding whitespace.
    pub fn parse(input: &str) -> Result<Self, CodeError> {
        let canonical: String = input
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '-')
            .map(|character| character.to_ascii_uppercase())
            .collect();
        if canonical.len() != CODE_LEN || !canonical.bytes().all(|byte| ALPHABET.contains(&byte)) {
            return Err(CodeError::Malformed);
        }
        Ok(Self(canonical))
    }

    /// Whether `input` could be a join code, used to tell a code from a ticket at the CLI.
    pub fn looks_like_code(input: &str) -> bool {
        Self::parse(input).is_ok()
    }

    /// The grouped form, for printing and for the share panel.
    pub fn printable(&self) -> String {
        let (head, tail) = self.0.split_at(GROUP.min(self.0.len()));
        format!("{head}-{tail}")
    }

    /// The canonical, separator-free form. Prefer [`Self::printable`] for anything a person
    /// reads.
    pub fn canonical(&self) -> &str {
        &self.0
    }

    /// Where the sealed record lives. This is the only derivation the server ever sees.
    pub fn index(&self) -> String {
        let derived = blake3::derive_key(INDEX_CONTEXT, self.0.as_bytes());
        derived[..INDEX_BYTES]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn key(&self) -> [u8; 32] {
        blake3::derive_key(KEY_CONTEXT, self.0.as_bytes())
    }

    /// Seal a ticket for this code. The result is `nonce || ciphertext`.
    pub fn seal(&self, ticket: &str) -> Result<Vec<u8>, RecordError> {
        let mut nonce = [0_u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|_| RecordError::Random)?;
        let cipher =
            Aes256Gcm::new_from_slice(&self.key()).map_err(|_| RecordError::Undecryptable)?;
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), ticket.as_bytes())
            .map_err(|_| RecordError::Random)?;
        let mut record = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        record.extend_from_slice(&nonce);
        record.extend_from_slice(&ciphertext);
        if record.len() > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge);
        }
        Ok(record)
    }

    /// Recover the ticket a record was sealed with.
    ///
    /// A wrong code fails here rather than at the server, which is the point: the store
    /// cannot tell a legitimate reader from anyone else, so authentication happens against
    /// the AEAD tag once the blob is already in hand.
    pub fn open(&self, record: &[u8]) -> Result<String, RecordError> {
        if record.len() > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge);
        }
        if record.len() <= NONCE_BYTES {
            return Err(RecordError::Malformed);
        }
        let (nonce, ciphertext) = record.split_at(NONCE_BYTES);
        let cipher =
            Aes256Gcm::new_from_slice(&self.key()).map_err(|_| RecordError::Undecryptable)?;
        let plaintext = cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| RecordError::Undecryptable)?;
        String::from_utf8(plaintext).map_err(|_| RecordError::Malformed)
    }
}

/// Redacted: a code is a live invite, and `{:?}` reaches logs and panic messages.
impl fmt::Debug for JoinCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JoinCode(<redacted>)")
    }
}

/// The deployed blind store. Overridable so a test or a fork can point elsewhere.
pub const DEFAULT_ENDPOINT: &str = "https://rv.p2pmux.com";
pub const ENDPOINT_ENV: &str = "P2PMUX_RENDEZVOUS_URL";

/// How long a published record outlives its last refresh.
///
/// The node re-seals well inside this window and deletes on a clean exit, so the only thing
/// this actually bounds is how long a *crashed* node's record lingers. Six hours costs four
/// KV writes a day per live session, which matters: writes are the binding free-tier limit at
/// roughly 1k/day against ~100k reads.
pub const RECORD_TTL: Duration = Duration::from_secs(6 * 60 * 60);
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// A slow rendezvous must not wedge session startup: the ticket still works without it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum PublishError {
    Code(CodeError),
    Record(RecordError),
    /// The store could not be reached, or answered with something unusable.
    Unreachable(String),
    /// The store refused the write, almost always rate limiting.
    Refused,
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(error) => error.fmt(formatter),
            Self::Record(error) => error.fmt(formatter),
            Self::Unreachable(detail) => {
                write!(
                    formatter,
                    "the rendezvous service is unreachable ({detail})"
                )
            }
            Self::Refused => formatter.write_str("the rendezvous service refused the request"),
        }
    }
}

impl std::error::Error for PublishError {}

#[derive(Debug)]
pub enum ResolveError {
    Code(CodeError),
    /// No record at that index: expired, deleted, or never existed. Deliberately not
    /// distinguished from a wrong code — the store cannot tell those apart either.
    NotFound,
    /// A record was found but this code does not open it.
    WrongCode,
    Unreachable(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(error) => error.fmt(formatter),
            Self::NotFound => formatter
                .write_str("that join code has expired or does not exist; ask for a fresh one"),
            Self::WrongCode => formatter.write_str("that join code does not open this record"),
            Self::Unreachable(detail) => {
                write!(
                    formatter,
                    "the rendezvous service is unreachable ({detail})"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// A client for the blind store.
#[derive(Clone, Debug)]
pub struct HostedRendezvous {
    endpoint: String,
    http: reqwest::Client,
}

impl HostedRendezvous {
    pub fn new() -> Result<Self, PublishError> {
        Self::at(std::env::var(ENDPOINT_ENV).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned()))
    }

    pub fn at(endpoint: impl Into<String>) -> Result<Self, PublishError> {
        // reqwest is built without a default crypto provider so that it rides the `ring`
        // build iroh already pulls in, rather than dragging aws-lc-rs and a cmake toolchain
        // into the build. That means the process default has to exist before the first
        // client is constructed; installing it twice is not an error worth reporting.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("p2pmux/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| PublishError::Unreachable(error.to_string()))?;
        Ok(Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            http,
        })
    }

    fn record_url(&self, code: &JoinCode) -> String {
        format!("{}/r/{}", self.endpoint, code.index())
    }

    /// Seal `ticket` under a fresh code and store it. Returns the code to hand out.
    pub async fn publish(&self, ticket: &str) -> Result<JoinCode, PublishError> {
        let code = JoinCode::mint().map_err(PublishError::Code)?;
        self.republish(&code, ticket).await?;
        Ok(code)
    }

    /// Re-seal an existing code's record, resetting its TTL.
    ///
    /// This deliberately seals again rather than reusing the stored bytes: it is a fresh
    /// nonce every time, and the alternative would be a read-modify-write against a store
    /// that is allowed to lie about what it holds.
    pub async fn republish(&self, code: &JoinCode, ticket: &str) -> Result<(), PublishError> {
        let record = code.seal(ticket).map_err(PublishError::Record)?;
        let response = self
            .http
            .put(self.record_url(code))
            .query(&[("ttl", RECORD_TTL.as_secs().to_string())])
            .header("content-type", "application/octet-stream")
            .body(record)
            .send()
            .await
            .map_err(|error| PublishError::Unreachable(strip_url(&error.to_string())))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            // 429 in practice; the rest of the 4xx range means this client and the store
            // disagree about the protocol, which is equally not worth retrying.
            400..=499 => Err(PublishError::Refused),
            status => Err(PublishError::Unreachable(format!("HTTP {status}"))),
        }
    }

    /// Fetch and open the record a code points at.
    pub async fn resolve(&self, code: &JoinCode) -> Result<String, ResolveError> {
        let response = self
            .http
            .get(self.record_url(code))
            .send()
            .await
            .map_err(|error| ResolveError::Unreachable(strip_url(&error.to_string())))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(ResolveError::NotFound);
        }
        if !response.status().is_success() {
            return Err(ResolveError::Unreachable(format!(
                "HTTP {}",
                response.status().as_u16()
            )));
        }
        let record = response
            .bytes()
            .await
            .map_err(|error| ResolveError::Unreachable(strip_url(&error.to_string())))?;
        code.open(&record).map_err(|_| ResolveError::WrongCode)
    }

    /// Best-effort removal on a clean exit. A failure here is covered by the TTL.
    pub async fn remove(&self, code: &JoinCode) -> Result<(), PublishError> {
        self.http
            .delete(self.record_url(code))
            .send()
            .await
            .map_err(|error| PublishError::Unreachable(strip_url(&error.to_string())))?;
        Ok(())
    }
}

/// A live code, kept resolvable for as long as the session runs.
///
/// A published record expires on its own, which is what keeps a crashed node from leaving an
/// invite behind forever. A node that is still running therefore has to say so periodically,
/// and say so again on the way out by deleting the record outright.
pub struct PublishedCode {
    store: HostedRendezvous,
    code: JoinCode,
    refresh: tokio::task::JoinHandle<()>,
}

impl PublishedCode {
    /// Publish `ticket` under a fresh code and hold its TTL open until [`Self::retire`].
    pub async fn publish(ticket: String) -> Result<Self, PublishError> {
        let store = HostedRendezvous::new()?;
        let code = store.publish(&ticket).await?;
        let refresh = tokio::spawn({
            let store = store.clone();
            let code = code.clone();
            async move {
                loop {
                    tokio::time::sleep(REFRESH_INTERVAL).await;
                    // A failed refresh is not worth surfacing or retrying tightly: the record
                    // still has most of its TTL left, and the next tick is well inside it.
                    let _ = store.republish(&code, &ticket).await;
                }
            }
        });
        Ok(Self {
            store,
            code,
            refresh,
        })
    }

    pub fn code(&self) -> &JoinCode {
        &self.code
    }

    /// Stop refreshing and delete the record. Best effort: the TTL is the real guarantee.
    pub async fn retire(self) {
        self.refresh.abort();
        let _ = self.store.remove(&self.code).await;
    }
}

/// reqwest puts the full request URL in its error strings, and ours carries the index —
/// which is derived from the code. Errors reach logs and shared panes, so cut it out.
fn strip_url(message: &str) -> String {
    match message.split_once(" for url (") {
        Some((head, _)) => head.to_owned(),
        None => message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_ticket_round_trips_through_its_own_code() {
        let code = JoinCode::mint().expect("code should mint");
        let record = code.seal("p2pmux-v3:TICKET").expect("record should seal");

        assert_eq!(
            code.open(&record).expect("record should open"),
            "p2pmux-v3:TICKET"
        );
    }

    #[test]
    fn another_code_cannot_open_the_record() {
        let code = JoinCode::mint().expect("code should mint");
        let other = JoinCode::mint().expect("code should mint");
        let record = code.seal("p2pmux-v3:TICKET").expect("record should seal");

        assert_eq!(other.open(&record), Err(RecordError::Undecryptable));
    }

    #[test]
    fn a_tampered_record_is_rejected_rather_than_returned() {
        // The whole design rests on the store being untrusted, so a flipped bit anywhere in
        // the blob has to fail closed instead of yielding a mangled ticket.
        let code = JoinCode::mint().expect("code should mint");
        let record = code.seal("p2pmux-v3:TICKET").expect("record should seal");

        for index in 0..record.len() {
            let mut tampered = record.clone();
            tampered[index] ^= 0b0000_0001;
            assert_eq!(
                code.open(&tampered),
                Err(RecordError::Undecryptable),
                "byte {index} was not authenticated"
            );
        }
    }

    #[test]
    fn sealing_the_same_ticket_twice_produces_different_records() {
        // A repeated nonce under one key is the failure that breaks GCM outright, and the
        // node re-seals on every TTL refresh.
        let code = JoinCode::mint().expect("code should mint");
        let first = code.seal("p2pmux-v3:TICKET").expect("record should seal");
        let second = code.seal("p2pmux-v3:TICKET").expect("record should seal");

        assert_ne!(first, second);
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn the_index_leaks_nothing_that_opens_the_record() {
        // The server holds exactly this pair. If the index were a prefix of, or derivable
        // from, the key, the blind store would not be blind.
        let code = JoinCode::mint().expect("code should mint");
        let index = code.index();
        let key = code.key();

        assert_eq!(index.len(), INDEX_BYTES * 2);
        let key_hex: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(!key_hex.contains(&index));
        assert!(!index.contains(&key_hex[..INDEX_BYTES]));
    }

    #[test]
    fn distinct_codes_land_on_distinct_indices() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(
                seen.insert(JoinCode::mint().expect("code should mint").index()),
                "index collision within 256 codes"
            );
        }
    }

    #[test]
    fn a_code_survives_being_retyped_by_a_human() {
        let code = JoinCode::mint().expect("code should mint");
        let printed = code.printable();

        for variant in [
            printed.clone(),
            printed.to_lowercase(),
            printed.replace('-', ""),
            format!("  {printed}  "),
            format!("{} {}", &printed[..GROUP], &printed[GROUP + 1..]),
        ] {
            assert_eq!(
                JoinCode::parse(&variant).expect("variant should parse"),
                code,
                "variant {variant:?} did not normalise"
            );
        }
    }

    #[test]
    fn malformed_codes_are_rejected_without_echoing_them() {
        for input in [
            "",
            "SHORT",
            "TOOMANYCHARACTERS",
            // `0`, `1`, `I`, `L` and `O` are deliberately outside the alphabet.
            "0123456789",
            "ABCDEFGHIJ",
        ] {
            let error = JoinCode::parse(input).expect_err("should be rejected");
            assert_eq!(error, CodeError::Malformed);
            if !input.is_empty() {
                assert!(!error.to_string().contains(input));
            }
        }
    }

    #[test]
    fn a_minted_code_is_printable_parseable_and_never_shown_by_debug() {
        let code = JoinCode::mint().expect("code should mint");

        assert_eq!(code.canonical().len(), CODE_LEN);
        assert_eq!(code.printable().len(), CODE_LEN + 1);
        assert!(JoinCode::looks_like_code(&code.printable()));
        let debugged = format!("{code:?}");
        assert!(!debugged.contains(code.canonical()));
    }

    #[test]
    fn minting_uses_the_whole_alphabet_without_favouring_its_start() {
        // Rejection sampling is the reason this holds: `byte % 31` would make the first
        // eight symbols about 29% likelier, which is exactly the kind of quiet entropy loss
        // that never shows up in a round-trip test.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2_000 {
            seen.extend(
                JoinCode::mint()
                    .expect("code should mint")
                    .canonical()
                    .bytes(),
            );
        }

        assert_eq!(
            seen.len(),
            ALPHABET.len(),
            "some symbols never appeared in 20k draws"
        );
    }

    #[test]
    fn a_record_larger_than_the_cap_is_refused() {
        let code = JoinCode::mint().expect("code should mint");

        assert_eq!(
            code.open(&vec![0_u8; MAX_RECORD_BYTES + 1]),
            Err(RecordError::TooLarge)
        );
        assert_eq!(code.open(&[0_u8; NONCE_BYTES]), Err(RecordError::Malformed));
    }
}
