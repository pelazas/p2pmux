//! One anonymous line a day, and only if the person said yes.
//!
//! Everything p2pmux currently knows about its own use is a proxy. Release asset
//! downloads count tarballs, which mirrors and CI also fetch. Stranger-opened
//! issues count people annoyed enough to type. Neither answers the question a
//! solo project actually has to answer — did anybody come back on Thursday — and
//! guessing at it from GitHub has been wrong in both directions.
//!
//! Three rules shape this, and the first one is not the usual one:
//!
//! - **It asks.** Once, on first run, in the terminal, with the whole payload on
//!   screen and `[Y/n]` at the end. Opt-out collection is the norm for developer
//!   tools and would get better numbers, and it is the wrong trade for this one:
//!   p2pmux's entire claim is that your keys and your processes stay on your
//!   machine, and a beacon nobody agreed to would cost more trust than the data
//!   is worth. A machine with no terminal to ask in is never asked and never
//!   sends.
//! - **It never delays anything.** The send runs on its own thread and nothing
//!   waits for it. A network that is down, a DNS that does not resolve, a
//!   corporate proxy that eats it — all of them are a thread that ends quietly.
//! - **It never says anything.** No success line, no failure line, no retry
//!   notice. A telemetry error the user can see is worse than no telemetry:
//!   it is noise about a thing that was supposed to cost them nothing.
//!
//! What goes over the wire is in [`Payload`], which is the same struct
//! `p2pmux telemetry show` prints. There is deliberately no code path that adds a
//! field to it at runtime — the way to collect one more thing is to edit this
//! file and the table in `services/metrics/schema.sql`, in a commit somebody can
//! read.
//!
//! The install id here is **not** the machine key from [`crate::machine_id`].
//! That one is announced to peers and signed over the peer id, so a metrics row
//! derived from it would be a metrics row tied to an identity other people have
//! seen. This is 32 unrelated random hex characters, and deleting the file makes
//! this machine a new install.

use std::{
    fs, io,
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Where a ping goes. See `services/metrics/` for what happens to it.
const ENDPOINT: &str = "https://m.p2pmux.com/p";

/// The endpoint, or a different one somebody chose.
///
/// Overridable because `services/metrics/` is thirty lines of Worker and one
/// table, and a team that would rather send this to their own account should be
/// able to without a fork. It is also how the live test reaches a staging copy
/// rather than the real one. Not a security surface: anybody who can set this
/// process's environment can already read everything it would have sent.
fn endpoint() -> String {
    std::env::var("P2PMUX_METRICS_URL").unwrap_or_else(|_| String::from(ENDPOINT))
}

/// How long a send stands before another is worth making.
///
/// A day, matched to `update_check`, so a person who opens twenty sessions
/// between breakfast and lunch makes one request and appears once.
const SEND_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bound on the whole request. Nothing waits on this, but a connection that
/// hangs forever is a thread that never ends.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The most often counters are written to disk.
///
/// Counting happens in memory; this only bounds how much a crash can lose. Agent
/// notifications arrive on every tool call, so writing a file per event would put
/// filesystem work on the path of something that fires hundreds of times an hour
/// to record a number nobody reads until tomorrow.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Whether this machine has been asked, and what it said.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Consent {
    /// Never asked. Sends nothing, and the next interactive run will ask.
    Unasked,
    Granted,
    Denied,
}

impl Consent {
    fn from_stored(raw: Option<&str>) -> Self {
        match raw {
            Some("granted") => Self::Granted,
            Some("denied") => Self::Denied,
            _ => Self::Unasked,
        }
    }

    fn as_stored(self) -> Option<&'static str> {
        match self {
            Self::Unasked => None,
            Self::Granted => Some("granted"),
            Self::Denied => Some("denied"),
        }
    }
}

/// The one file this module owns.
///
/// One file rather than a key in `config.toml` and state somewhere else: consent,
/// the id it applies to, the counters it produced and the time they were last
/// sent are one concern, and a person revoking consent should be able to delete
/// one path and be done. `config.toml` carries a comment pointing here.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Record {
    /// `"granted"`, `"denied"`, or absent for a machine that has not been asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    consent: Option<String>,
    /// 32 lowercase hex, created the first time consent is granted — not before,
    /// so a machine that said no never has an id at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default)]
    sessions: u32,
    #[serde(default)]
    peers: u32,
    #[serde(default)]
    agents: u32,
    /// Sticky: a session on this machine once reached two members.
    #[serde(default)]
    activated: bool,
    #[serde(default)]
    last_sent_unix_ms: u64,
    /// Whether the one-time question below has been asked. Local only, and
    /// deliberately not a payload field: what somebody was shown on their own
    /// terminal is not something to report back.
    #[serde(default)]
    asked_for_a_word: bool,
}

/// Exactly what a ping contains, and exactly what `p2pmux telemetry show` prints.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Payload {
    pub id: String,
    pub version: &'static str,
    pub os: String,
    pub sessions: u32,
    pub peers: u32,
    pub agents: u32,
    pub activated: bool,
}

/// Which number an event moves.
#[derive(Clone, Copy, Debug)]
pub enum Counter {
    /// A session was started on this machine.
    Sessions,
    /// Somebody else joined a session hosted here.
    Peers,
    /// An agent said it wanted attention.
    Agents,
}

pub fn state_path() -> Result<PathBuf, io::Error> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no HOME or XDG_CONFIG_HOME to store telemetry state in",
            )
        })?;
    Ok(base.join("p2pmux").join("telemetry.json"))
}

/// This build's platform, as the metrics service spells it: `macos-aarch64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether the environment forbids this regardless of what is stored.
///
/// `DO_NOT_TRACK` is the cross-tool convention and is honoured whatever it is set
/// to, which is what every other implementation does and what anybody setting it
/// expects. `CI` catches the case this would otherwise get most wrong: a build
/// agent that installs p2pmux on every run is not a user, and counting it would
/// quietly invent a population.
fn suppressed() -> bool {
    // A test run is not a user. The counters are bumped from `Coordinator::admit`
    // and from the session launcher, both of which the unit tests drive hundreds
    // of times, and a contributor's `cargo test` must not post their afternoon to
    // the metrics service as real use. Integration tests reach the binary rather
    // than this crate, so they isolate `HOME` instead.
    if cfg!(test) {
        return true;
    }
    suppressed_by(|name| std::env::var(name).ok())
}

/// The decision itself, over a lookup rather than over the process environment.
///
/// Split out because the lib forbids unsafe and `set_var` is unsafe, so the only
/// way to test the switches people actually rely on is to hand the rule its
/// inputs. That is the better shape anyway: what is under test is the rule.
fn suppressed_by(lookup: impl Fn(&str) -> Option<String>) -> bool {
    if lookup("P2PMUX_TELEMETRY").as_deref() == Some("0") {
        return true;
    }
    ["DO_NOT_TRACK", "CI"]
        .iter()
        .any(|name| lookup(name).is_some_and(|value| value != "0" && !value.is_empty()))
}

fn read_record() -> Record {
    state_path()
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Replace the state file, or do nothing.
///
/// Written beside and renamed over, so a machine that loses power mid-write is
/// left with the previous state rather than half a JSON document — which
/// `read_record` would treat as a machine that had never been asked, and which
/// would therefore ask again.
fn write_record(record: &Record) {
    let Ok(path) = state_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec_pretty(record) else {
        return;
    };
    let temporary = path.with_extension("json.tmp");
    if fs::write(&temporary, &bytes).is_ok() {
        let _ = fs::rename(&temporary, &path);
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn new_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return String::new();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What this machine has agreed to, read once per process.
///
/// Cached because the answer cannot change under a running process — the only
/// things that write it are this process's own prompt and `p2pmux telemetry`,
/// which is a different one — and because [`bump`] is called from the agent
/// notification path, where a file read per event would be a file read per tool
/// call.
pub fn consent() -> Consent {
    static CONSENT: OnceLock<Consent> = OnceLock::new();
    *CONSENT.get_or_init(|| {
        if suppressed() {
            return Consent::Denied;
        }
        Consent::from_stored(read_record().consent.as_deref())
    })
}

/// The question, its exact wording, and what a reply means.
///
/// Separated from the terminal so a test can put a line in and read the answer
/// out. `Enter` means yes, which is the whole reason this gets useful numbers:
/// a prompt nobody can answer without reading it is a prompt most people decline
/// by reflex, and the honest way to raise a yes rate is to make the ask small
/// and the reason plain, not to skip it.
pub const PROMPT: &str = "\
p2pmux can send one anonymous line a day: a random id, the version, the OS, how
many sessions you started, whether anybody joined one, and nothing else. No
hostnames, no directories, no session names, nothing you typed. Terminal traffic
never goes near it — that stays peer to peer.

It is how a project with no analytics finds out whether anyone came back.

  p2pmux telemetry show    print the exact line this machine would send
  p2pmux telemetry off     stop sending it, any time

Send it? [Y/n] ";

/// Read one answer. Anything but a `n` is yes, including an empty line.
pub fn answer_from(reply: &str) -> Consent {
    match reply.trim().to_ascii_lowercase().as_str() {
        "n" | "no" => Consent::Denied,
        _ => Consent::Granted,
    }
}

/// Ask, once, if this is a terminal and nothing has been decided yet.
///
/// Called before any command that takes over the screen, so the question is
/// never competing with a TUI for the same rows. Returns without asking — and
/// without writing anything — when there is no terminal to ask in: an unattended
/// droplet running `p2pmux daemon` has nobody to consent, and a prompt written
/// into a log where no answer can arrive would be worse than silence.
pub fn ask_once() {
    if suppressed() || consent() != Consent::Unasked {
        return;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stderr())
    {
        return;
    }
    use io::Write;
    let mut stderr = io::stderr().lock();
    if write!(stderr, "\n{PROMPT}").is_err() || stderr.flush().is_err() {
        return;
    }
    let mut reply = String::new();
    // A closed stdin reads zero bytes forever rather than blocking. Treat it the
    // same as no terminal: decide nothing, ask again next time.
    if io::stdin().read_line(&mut reply).unwrap_or(0) == 0 {
        let _ = writeln!(stderr);
        return;
    }
    let answer = answer_from(&reply);
    set_consent(answer);
    let _ = writeln!(
        stderr,
        "{}\n",
        match answer {
            Consent::Granted => "Thanks — one line a day. `p2pmux telemetry off` stops it.",
            _ => "Nothing will be sent.",
        }
    );
}

/// Record an answer, creating an id the first time one is needed.
///
/// The id is created on grant rather than on first run, so a machine that
/// declined never has one to leak, and a machine that later changes its mind
/// becomes a new install rather than retroactively naming its past.
pub fn set_consent(answer: Consent) {
    let mut record = read_record();
    record.consent = answer.as_stored().map(str::to_owned);
    if answer == Consent::Granted && record.id.is_none() {
        record.id = Some(new_id());
    }
    write_record(&record);
}

/// Add to a counter. A no-op on any machine that did not say yes.
///
/// Two speeds, because the three counters have two very different shapes. A
/// session start and a member joining happen a handful of times a day and are
/// the numbers every decision rests on, so they are written through: a process
/// that exits a second later still counted them. Agent notifications fire on
/// every tool call an agent makes, so they accumulate in memory and reach the
/// disk at most every [`FLUSH_INTERVAL`] — losing up to thirty seconds of a
/// number nobody reads until tomorrow, in exchange for not putting a file write
/// on a path that runs hundreds of times an hour.
pub fn bump(counter: Counter, amount: u32) {
    if amount == 0 || consent() != Consent::Granted {
        return;
    }
    match counter {
        Counter::Sessions => flush(amount, 0, 0, false),
        Counter::Peers => flush(0, amount, 0, false),
        Counter::Agents => bump_agents(amount),
    }
}

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Agent notifications, counted in memory and written on a timer.
static PENDING_AGENTS: AtomicU32 = AtomicU32::new(0);
static LAST_FLUSH: AtomicU64 = AtomicU64::new(0);

fn bump_agents(amount: u32) {
    PENDING_AGENTS.fetch_add(amount, Ordering::Relaxed);
    let now = unix_ms_now();
    let last = LAST_FLUSH.load(Ordering::Relaxed);
    // The first one goes straight to disk. An agent that fired once and an agent
    // that never fired are different facts, and thirty seconds of silence after
    // the first is exactly when a short session ends.
    if last != 0 && now.saturating_sub(last) < FLUSH_INTERVAL.as_millis() as u64 {
        return;
    }
    if LAST_FLUSH
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return; // Another thread owns this window.
    }
    flush(0, 0, PENDING_AGENTS.swap(0, Ordering::Relaxed), false);
}

/// Put whatever is still in memory on disk, whatever the timer says.
///
/// Called before a ping is built, so the line that goes out is the line that is
/// true rather than the one that was true half a minute ago.
fn flush_pending() {
    let pending = PENDING_AGENTS.swap(0, Ordering::Relaxed);
    if pending > 0 {
        LAST_FLUSH.store(unix_ms_now(), Ordering::Relaxed);
        flush(0, 0, pending, false);
    }
}

/// Note that a session on this machine reached two members.
///
/// The number the roadmap turns on, and the reason it is a flag rather than a
/// counter: one person in a p2pmux session is using a worse tmux, and the
/// question is whether the second person ever arrived, not how often.
pub fn mark_activated() {
    if consent() != Consent::Granted {
        return;
    }
    flush(0, 0, 0, true);
}

/// Fold a delta into the stored counters.
///
/// Read-modify-write rather than write-what-I-have, because the node and the
/// client are separate processes writing the same file: taking the disk's
/// numbers and adding to them means a concurrent flush loses one increment in a
/// true interleave, where overwriting would lose everything the other process
/// had counted. Counts are a floor, not a census, and this is the cheap way to
/// make the floor high.
fn flush(sessions: u32, peers: u32, agents: u32, activated: bool) {
    if sessions == 0 && peers == 0 && agents == 0 && !activated {
        return;
    }
    let mut record = read_record();
    if Consent::from_stored(record.consent.as_deref()) != Consent::Granted {
        return; // Revoked in another process since this one cached its answer.
    }
    record.sessions = record.sessions.saturating_add(sessions);
    record.peers = record.peers.saturating_add(peers);
    record.agents = record.agents.saturating_add(agents);
    record.activated |= activated;
    write_record(&record);
}

/// The line this machine would send right now, or `None` if it would send none.
pub fn payload() -> Option<Payload> {
    let record = read_record();
    if Consent::from_stored(record.consent.as_deref()) != Consent::Granted {
        return None;
    }
    Some(Payload {
        id: record.id.unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION"),
        os: platform(),
        sessions: record.sessions,
        peers: record.peers,
        agents: record.agents,
        activated: record.activated,
    })
}

/// Where the one-time question sends people.
///
/// A redirect on the site rather than the destination itself, so the place it
/// points can move — to a form, to a thread, to a mailbox — without shipping a
/// new binary to everybody who already has one.
const FEEDBACK_URL: &str = "https://p2pmux.com/hi";

/// Ask, once ever, on a machine where somebody else has actually joined.
///
/// The only thing p2pmux ever asks for unprompted, and the timing is the whole
/// point: after a session with a second person in it, printed as that session
/// closes, when what happened is still in mind and nothing is waiting on the
/// answer. Not in the inbox, where it would compete with work; not on install,
/// when there is nothing to say yet.
///
/// Gated on telemetry consent, because `activated` is only recorded on machines
/// that agreed — which also means the people who declined are not asked to go
/// and fill anything in, and that is the right way round.
pub fn ask_for_a_word() {
    if suppressed() || consent() != Consent::Granted {
        return;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return;
    }
    let mut record = read_record();
    if !record.activated || record.asked_for_a_word {
        return;
    }
    record.asked_for_a_word = true;
    write_record(&record);
    use io::Write;
    let mut stderr = io::stderr().lock();
    let _ = writeln!(
        stderr,
        "\nSomebody else has been in a session on this machine — the part of p2pmux\n\
         nobody has written in about yet. What were you two doing?\n\n  \
         {FEEDBACK_URL}\n\nAsked once, and never again."
    );
}

/// The line this machine would send, whether or not it is sending.
///
/// Separate from [`payload`] because the question `p2pmux telemetry show` answers
/// is "what would this send about me", and answering it with nothing on a machine
/// that declined reads as evasion rather than as reassurance. A machine that has
/// not been asked has no id yet, and says so rather than inventing one it would
/// then have to keep.
pub fn would_send() -> Payload {
    // Printing a number that is thirty seconds stale would make the one command
    // written to be checkable the one command that is slightly wrong.
    flush_pending();
    let record = read_record();
    Payload {
        id: record
            .id
            .unwrap_or_else(|| String::from("<generated if you say yes>")),
        version: env!("CARGO_PKG_VERSION"),
        os: platform(),
        sessions: record.sessions,
        peers: record.peers,
        agents: record.agents,
        activated: record.activated,
    }
}

/// Whether enough time has passed since the last successful send.
fn due(record: &Record, now_unix_ms: u64) -> bool {
    // Never sent, said outright rather than left to arithmetic. Subtracting from
    // zero happens to exceed a day for every real clock, which made this case
    // work by accident and would have stopped working on any machine whose clock
    // read 1970 — which is exactly what a board with a dead RTC reads at boot.
    if record.last_sent_unix_ms == 0 {
        return true;
    }
    // A clock that has gone backwards leaves a send stamped in the future.
    // Treat that as due rather than as covered for the next fifty years.
    record.last_sent_unix_ms > now_unix_ms
        || now_unix_ms.saturating_sub(record.last_sent_unix_ms) >= SEND_INTERVAL.as_millis() as u64
}

async fn post(body: String) -> bool {
    // Same reasoning as the rendezvous client and the update check: reqwest is
    // built with no default crypto provider so it rides the `ring` build iroh
    // already pulls in, and the process default has to be installed before the
    // first client exists.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Named, so the request is identifiable in a log as a p2pmux ping rather
        // than as an anonymous POST from nowhere.
        .user_agent(concat!("p2pmux/", env!("CARGO_PKG_VERSION")))
        .build()
    else {
        return false;
    };
    client
        .post(endpoint())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

/// Send today's line if one is due, and zero the counters if it lands.
///
/// Returns whether a line was accepted, which nothing in the product reads --
/// the caller is a detached thread and there is no correct thing for it to do
/// with a failure. The live test reads it.
///
/// Zeroed only on success, and by subtracting what was sent rather than by
/// writing zero: the counters kept moving while the request was in flight, and a
/// session started during those ten seconds belongs to tomorrow's line rather
/// than to nobody's.
pub fn send_if_due() -> bool {
    if suppressed() || consent() != Consent::Granted {
        return false;
    }
    flush_pending();
    let record = read_record();
    let now = unix_ms_now();
    if !due(&record, now) {
        return false;
    }
    let Some(id) = record.id.clone().filter(|id| id.len() == 32) else {
        return false;
    };
    let payload = Payload {
        id,
        version: env!("CARGO_PKG_VERSION"),
        os: platform(),
        sessions: record.sessions,
        peers: record.peers,
        agents: record.agents,
        activated: record.activated,
    };
    let Ok(body) = serde_json::to_string(&payload) else {
        return false;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    if !runtime.block_on(post(body)) {
        return false;
    }
    let mut latest = read_record();
    latest.sessions = latest.sessions.saturating_sub(payload.sessions);
    latest.peers = latest.peers.saturating_sub(payload.peers);
    latest.agents = latest.agents.saturating_sub(payload.agents);
    latest.last_sent_unix_ms = now;
    write_record(&latest);
    true
}

/// Start the send. Nothing waits for it and nothing hears about it.
pub fn spawn() {
    if suppressed() || consent() != Consent::Granted {
        return;
    }
    std::thread::spawn(|| {
        let _ = send_if_due();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enter means yes. That is the whole design of the prompt, and a change
    /// here would silently turn an informed default into a refusal nobody typed.
    #[test]
    fn an_empty_answer_is_a_yes_and_only_n_is_a_no() {
        for yes in ["", "\n", "y", "Y", "yes", "  \n", "sure"] {
            assert_eq!(answer_from(yes), Consent::Granted, "{yes:?}");
        }
        for no in ["n", "N", "no", "NO", " no \n"] {
            assert_eq!(answer_from(no), Consent::Denied, "{no:?}");
        }
    }

    /// A machine that has not been asked must not be treated as one that agreed.
    #[test]
    fn only_the_stored_word_granted_is_consent() {
        assert_eq!(Consent::from_stored(Some("granted")), Consent::Granted);
        assert_eq!(Consent::from_stored(Some("denied")), Consent::Denied);
        assert_eq!(Consent::from_stored(None), Consent::Unasked);
        assert_eq!(Consent::from_stored(Some("")), Consent::Unasked);
        assert_eq!(Consent::from_stored(Some("true")), Consent::Unasked);
    }

    #[test]
    fn a_send_is_due_after_a_day_and_after_a_clock_that_went_backwards() {
        let day = SEND_INTERVAL.as_millis() as u64;
        let record = |sent| Record {
            last_sent_unix_ms: sent,
            ..Default::default()
        };

        // Never sent, including on a machine whose clock still reads 1970.
        assert!(due(&record(0), 1_000_000));
        assert!(due(&record(0), 0));
        assert!(!due(&record(1_000_000), 1_000_000));
        assert!(!due(&record(1_000_000), 1_000_000 + day - 1));
        assert!(due(&record(1_000_000), 1_000_000 + day));
        // Stamped in the future by a clock that moved backwards. Sending one
        // extra line costs one request; trusting it costs every future day.
        assert!(due(&record(2_000_000), 1_000_000));
    }

    /// The payload is the privacy policy, so its shape is a test rather than a
    /// convention: a field added here is a field the metrics service would have
    /// to be taught to store, and both edits should show up in one review.
    #[test]
    fn the_payload_is_seven_fields_and_no_more() {
        let payload = Payload {
            id: "0".repeat(32),
            version: "0.1.13",
            os: String::from("linux-x86_64"),
            sessions: 2,
            peers: 1,
            agents: 7,
            activated: true,
        };
        let json = serde_json::to_value(&payload).expect("serializes");
        let object = json.as_object().expect("an object");

        let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "activated",
                "agents",
                "id",
                "os",
                "peers",
                "sessions",
                "version"
            ],
            "the wire format changed; services/metrics must change with it"
        );
    }

    /// The id has to match what the service will accept, or every ping is a 400
    /// nobody sees — the send path is silent by design, so a malformed id would
    /// look exactly like nobody using p2pmux.
    #[test]
    fn a_new_id_is_thirty_two_lowercase_hex_characters() {
        let id = new_id();
        assert_eq!(id.len(), 32, "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "{id}"
        );
        assert_ne!(id, new_id(), "two installs must not share an id");
    }

    #[test]
    fn the_platform_matches_the_shape_the_service_validates() {
        let platform = platform();
        let (os, arch) = platform.split_once('-').expect("os-arch");
        assert!(!os.is_empty() && !arch.is_empty(), "{platform}");
        assert!(
            platform
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
            "{platform}"
        );
    }

    /// Every path out of `suppressed` is a path where nothing is sent, so the
    /// conventional switches have to actually work rather than merely be read.
    #[test]
    fn the_conventional_off_switches_are_honoured() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            move |name: &str| {
                owned
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };

        assert!(suppressed_by(env(&[("DO_NOT_TRACK", "1")])));
        assert!(suppressed_by(env(&[("DO_NOT_TRACK", "yes")])));
        assert!(suppressed_by(env(&[("CI", "true")])));
        assert!(suppressed_by(env(&[("P2PMUX_TELEMETRY", "0")])));

        // Set to a falsey value is not set. Shells and CI images export empty
        // variables all the time, and treating that as "off" would silence
        // telemetry on machines nobody meant to silence.
        assert!(!suppressed_by(env(&[("DO_NOT_TRACK", "0")])));
        assert!(!suppressed_by(env(&[("DO_NOT_TRACK", "")])));
        assert!(!suppressed_by(env(&[("CI", "")])));
        assert!(!suppressed_by(env(&[("P2PMUX_TELEMETRY", "1")])));
        assert!(
            !suppressed_by(env(&[])),
            "a clean environment suppresses nothing"
        );
    }

    /// A record round-trips through the file format the state path holds, and an
    /// unreadable one reads back as a machine that was never asked — which is
    /// what makes a half-written file ask again rather than silently stop.
    #[test]
    fn a_damaged_state_file_reads_as_never_asked() {
        assert_eq!(
            serde_json::from_slice::<Record>(b"{\"consent\":\"granted\"")
                .ok()
                .and_then(|record| record.consent),
            None
        );
        let stored: Record =
            serde_json::from_slice(br#"{"consent":"granted","id":"ab","sessions":3}"#)
                .expect("parses");
        assert_eq!(stored.consent.as_deref(), Some("granted"));
        assert_eq!(stored.sessions, 3);
        assert_eq!(stored.peers, 0, "absent counters default rather than fail");
        assert!(!stored.activated);
    }
}
