//! The permanent association between two machines you own.
//!
//! Pairing is the onboarding primitive. Without it, adding a machine means
//! SSHing in and starting a session by hand every time — tolerable for an
//! experiment of one, fatal for a product.
//!
//! What it records is small on purpose:
//!
//! - **The home session's ticket.** Every paired machine rejoins the same
//!   session, which is what makes bare `p2pmux` work on either of them with no
//!   code typed. The short-code-to-ticket mechanism already exists, so pairing
//!   is mostly persistence plus auto-join on start.
//! - **The machines paired with this one**, by authenticated peer id and by the
//!   name they go under, so the inbox can say `asleep` about a machine that is
//!   switched off rather than forgetting it.
//!
//!   This file is what decides ownership, and it is the only thing that does. A
//!   member of a session says on the wire whether a person or a machine is at
//!   the other end, but that claim can only ever *narrow* what this file says:
//!   being one of your machines means being written in here, by a `p2pmux pair`
//!   you ran. Nothing a peer sends can add a row.
//! - **Whether this machine accepts work, and what work.** Off by default,
//!   asked once during pairing rather than as a separate configuration step. It
//!   means *accepts work from my own machines*, never *from anyone with the
//!   join code*.
//!
//!   It is now acted on, by [`Pairing::work_decision`], and it is deliberately
//!   not enough on its own: saying yes at pair time is consent to be asked, and
//!   an empty [`WorkPolicy::allow`] still permits nothing. A machine that meant
//!   to hand out a shell has to write that down.
//!
//!   The answer is given on the machine it is about, and there is no channel
//!   back: the only thing that crosses machines is the shared layout, whose
//!   member list is signed and hash-chained, and the inbox is built on never
//!   touching that. So each machine knows its own answer and records `None` for
//!   everyone else, which the fleet list prints as `—` rather than as a refusal
//!   nobody made. That is not a gap to be closed: a policy that travelled could
//!   be believed, and the point is that it never has to be.
//!
//! It deliberately holds no keys of its own. The ticket is the session's
//! existing cryptographic address, and a machine that can read this file could
//! already read the session store next to it.

use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{layout::MemberKind, tui::PairedMachine};

static PAIRING_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How long after `p2pmux pair` an arriving machine is taken to be the invited
/// one. Long enough to walk to another room and type a command, short enough
/// that a code left on screen overnight does not adopt whoever turns up.
const PAIRING_WINDOW_SECONDS: u64 = 10 * 60;

const PAIRING_HEADER: &str = "# p2pmux pairing\n#\n# Machines you have paired with this one, and the session they share.\n# Written by `p2pmux pair`. Delete a [[machine]] block to unpair it.\n#\n# What your other machines may start here is [work.allow], below. It is empty\n# until you write something in it, and an empty list allows nothing even when\n# accepts_work is true. Each entry is a whole command, matched in full:\n#\n#   [work]\n#   allow = [\"hermes chat\", \"claude\"]\n#   confirm = true          # ask here first, every time\n#\n# The entry \"shell\" allows a plain login shell, which is everything this user\n# account can do. It is spelled out so nobody grants it by accident.\n\n";

#[derive(Debug)]
pub enum PairingError {
    Io(io::Error),
    MissingHome,
    Invalid(String),
}

impl fmt::Display for PairingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::MissingHome => {
                write!(formatter, "no HOME or XDG_CONFIG_HOME to store pairing in")
            }
            Self::Invalid(reason) => write!(formatter, "invalid pairing file: {reason}"),
        }
    }
}

impl std::error::Error for PairingError {}

impl From<io::Error> for PairingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Everything this machine remembers about the machines it is paired with.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Pairing {
    /// The session every paired machine rejoins. Absent until the first pairing
    /// completes, which is exactly when bare `p2pmux` stops needing a code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Whether this machine agreed to let your other machines start work on
    /// it. Recorded during pairing, and the first of the two gates a remote
    /// terminal has to pass — see [`Pairing::work_decision`].
    #[serde(default)]
    pub accepts_work: bool,
    /// Unix seconds until which an arriving machine is the one being paired.
    ///
    /// `p2pmux pair` prints a code and returns, so the machine that offered it
    /// is not watching when the other one turns up — the node is. Something has
    /// to tell the node that the next arrival was invited, and this is it: a
    /// window the user opened deliberately, that closes on the first machine
    /// admitted through it and expires on its own if nobody comes.
    ///
    /// Without it the node had no way to tell an invited machine from a guest,
    /// and resolved that by treating every peer as fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_until: Option<u64>,
    /// Whether the user has already said no to installing the fleet agent.
    ///
    /// Pairing is where the offer belongs — it is the moment somebody decided
    /// this box is fleet — but a person who declined once should not be asked
    /// again by every machine they pair afterwards.
    #[serde(default)]
    pub daemon_declined: bool,
    #[serde(default, rename = "machine")]
    pub machines: Vec<PairedMachine>,
    /// What your other machines may start on this one. See [`WorkPolicy`].
    #[serde(default)]
    pub work: WorkPolicy,
    /// The standing invitation for machines you own. See [`EnrolToken`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrol: Option<EnrolToken>,
}

/// A long-lived, revocable secret that admits a machine to this fleet.
///
/// `p2pmux pair` is a code one human types on one machine within ten minutes.
/// That is right for two laptops and wrong for a VM: the box you want in your
/// fleet is created by a script, has nobody sitting at it, and is often gone by
/// the end of the week. Asking a provisioning run to hold a ten-minute window
/// open makes the fleet something you assemble by hand.
///
/// So a machine can be enrolled with a token you baked into its cloud-init
/// instead. The trade is deliberate and worth stating plainly: this is a
/// credential, and anyone who holds it can put a machine in your fleet until
/// you revoke it. What that buys them is *membership*, which grants nothing on
/// its own — starting anything on one of your machines still has to pass that
/// machine's own `[work].allow`, which is written on it and travels nowhere.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnrolToken {
    /// Hex, 32 bytes of randomness. Compared whole.
    pub secret: String,
    /// When it was minted, for `p2pmux enroll` to print.
    #[serde(default)]
    pub created_at: u64,
}

/// What a machine allows other machines of yours to run on it.
///
/// Read only on the machine it governs, written only by whoever owns that
/// machine, and never carried over the wire in either direction. That is the
/// whole design: consent about a machine is given on it, so no message can
/// grant it and no coordinator can be believed about it.
///
/// **This is not a filter on what someone may type into a terminal.** You
/// cannot reliably filter commands typed into an interactive pty — shell
/// aliases, `\sudo`, base64 piped to sh, `:!sh` from inside vim, and any REPL
/// that shells out all defeat it, and so does everything nobody has thought of
/// yet. What is enforced here is *what may be launched*, which is a different
/// and answerable question. Allow a shell and you have allowed everything that
/// user can do; the allowlist is honest about that rather than pretending a
/// blocklist could help.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkPolicy {
    /// Complete commands that may be launched, each written as you would type
    /// it: `"hermes chat"`. Matched in full, not as a prefix, so an allowed
    /// command cannot be extended with arguments nobody agreed to.
    ///
    /// The one special entry is `"shell"`, which allows a plain login shell.
    /// It has to be written down, because it is the entry that gives away
    /// everything the others were carefully not giving away.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Ask on this machine before each remote pane, even an allowed one.
    ///
    /// The natural extension of `accepts_work`: consent given once at pair
    /// time, then again at the moment it is used.
    #[serde(default)]
    pub confirm: bool,
}

/// What this machine will do about a remote pane it has been asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkDecision {
    Allow,
    /// Hold the request and ask the person at this machine.
    Ask,
    Refuse,
}

/// The entry that means a login shell rather than a named program.
pub const SHELL_ENTRY: &str = "shell";

/// How a command is written in the allowlist and on a command line.
///
/// One function so the entry `p2pmux work allow` writes, the entry
/// [`WorkPolicy::permits`] matches, and the entry a refusal tells you to add
/// cannot drift apart — a policy you were told to add and that then does not
/// match is worse than no policy at all.
pub fn work_entry(command: &[String]) -> String {
    if command.is_empty() {
        return String::from(SHELL_ENTRY);
    }
    command.join(" ")
}

impl WorkPolicy {
    /// Add an entry, and say whether it was not already there.
    ///
    /// Written through [`work_entry`] and compared the way [`Self::permits`]
    /// compares, so `allow "claude  "` and `allow claude` are one entry.
    pub fn allow(&mut self, command: &[String]) -> bool {
        if self.permits(command) {
            return false;
        }
        self.allow.push(work_entry(command));
        true
    }

    /// Drop an entry, and say whether there was one.
    pub fn deny(&mut self, command: &[String]) -> bool {
        let before = self.allow.len();
        let words = if command.is_empty() {
            vec![String::from(SHELL_ENTRY)]
        } else {
            command.to_vec()
        };
        self.allow.retain(|entry| {
            entry.split_whitespace().collect::<Vec<_>>() != words.iter().collect::<Vec<_>>()
        });
        before != self.allow.len()
    }

    /// Whether this exact command may be launched here.
    ///
    /// Exact, because "starts with an allowed command" is not a permission
    /// anyone gave: `claude` and `claude --dangerously-skip-permissions` are
    /// different decisions, and p2pmux only ever asks for the commands in its
    /// own capability table, so there is nothing to be gained by being loose.
    pub fn permits(&self, command: &[String]) -> bool {
        if command.is_empty() {
            return self.allow.iter().any(|entry| entry.trim() == SHELL_ENTRY);
        }
        self.allow.iter().any(|entry| {
            entry.split_whitespace().collect::<Vec<_>>() == command.iter().collect::<Vec<_>>()
        })
    }
}

impl Pairing {
    /// What to do about a machine of yours asking for a pane here.
    ///
    /// Default closed twice over: `accepts_work` is off until this machine
    /// says otherwise, and an empty allowlist permits nothing even once it is
    /// on. A yes given at pair time is consent to be asked, not a free shell.
    pub fn work_decision(&self, command: &[String]) -> WorkDecision {
        if !self.accepts_work || !self.work.permits(command) {
            return WorkDecision::Refuse;
        }
        if self.work.confirm {
            return WorkDecision::Ask;
        }
        WorkDecision::Allow
    }
}

impl Pairing {
    /// Whether bare `p2pmux` can rejoin without being handed a code.
    pub fn can_rejoin(&self) -> bool {
        self.ticket.is_some()
    }

    /// Record a machine, or update the one already there.
    ///
    /// Pairing twice with the same machine is a re-pair, not a second machine:
    /// a user who re-runs `p2pmux pair` after moving house should end up with
    /// one desktop, not two. Identity is the peer id where there is one, so a
    /// machine that was renamed is still the same machine, and two machines
    /// that chose the same name are still two.
    pub fn remember(&mut self, name: &str, machine_id: Option<String>, accepts_work: Option<bool>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let existing = match machine_id.as_deref() {
            Some(machine_id) => self
                .machines
                .iter_mut()
                .position(|machine| machine.machine_id.as_deref() == Some(machine_id))
                // A machine paired before identities were recorded is that same
                // machine turning up with its identity for the first time, not
                // a new one. Adopt the row rather than growing a duplicate.
                .or_else(|| {
                    self.machines
                        .iter()
                        .position(|machine| machine.machine_id.is_none() && machine.name == name)
                }),
            None => self
                .machines
                .iter()
                .position(|machine| machine.name == name),
        };
        match existing {
            Some(index) => {
                let machine = &mut self.machines[index];
                machine.name = name.to_owned();
                if machine_id.is_some() {
                    machine.machine_id = machine_id;
                }
                // Only ever upgraded from "never said" to an answer. A machine
                // that told us once must not be silently un-told by a later
                // sighting that carried nothing.
                if accepts_work.is_some() {
                    machine.accepts_work = accepts_work;
                }
            }
            None => self.machines.push(PairedMachine {
                name: name.to_owned(),
                machine_id,
                accepts_work,
            }),
        }
        self.machines
            .sort_by(|left, right| left.name.cmp(&right.name));
    }

    /// Whether a member of a session is one of the machines in this fleet.
    ///
    /// The whole ownership decision, in one place, and it is answered locally:
    /// nothing a peer sends can make this return `true`, because the peer id is
    /// the transport's own authenticated identity and the fleet list is a file
    /// only this machine writes.
    ///
    /// The member's own claim can only ever take ownership away. A peer that
    /// says it has a person at it is taken at its word — the claim costs it
    /// access rather than winning it — while a peer that says it is a machine
    /// still has to be in this file to count. A peer that says nothing is
    /// judged on this file alone, which is all there was before it could speak.
    pub fn owns(&self, machine_id: &str, name: &str, kind: MemberKind) -> bool {
        owns_machine(&self.machines, machine_id, name, kind)
    }

    /// Whether a machine arriving now was invited by a recent `p2pmux pair`.
    pub fn pairing_window_open(&self, now: u64) -> bool {
        self.pending_until.is_some_and(|until| now < until)
    }

    /// Open the window an arriving machine may join the fleet through.
    pub fn open_pairing_window(&mut self, now: u64) {
        self.pending_until = Some(now.saturating_add(PAIRING_WINDOW_SECONDS));
    }

    /// Whether `secret` is this fleet's standing invitation.
    ///
    /// Constant-time-ish by construction rather than by care: the secret is
    /// compared whole and the caller reaches this only over an authenticated
    /// transport, so there is no oracle to time. An empty secret never matches,
    /// which is what makes a revoked token a refusal rather than a wildcard.
    pub fn enrolment_admits(&self, secret: &str) -> bool {
        !secret.is_empty()
            && self
                .enrol
                .as_ref()
                .is_some_and(|token| token.secret == secret)
    }

    /// Forget a paired machine — and, with the last of them, the fleet itself.
    ///
    /// Dropping the row is not the whole of an unpair. The ticket is what makes
    /// this machine fleet at all: bare `p2pmux` dials it, the node declares
    /// itself a machine on the strength of it, and [`pin_peers`] records
    /// arrivals only while it is there. An unpair that left it behind left the
    /// machine in a fleet of nobody — `p2pmux machines` printed "No other
    /// machines paired yet" while every bare `p2pmux` still spent half a minute
    /// dialling the session the last machine took with it.
    ///
    /// Only the last one does this. Unpairing one machine of three is leaving
    /// that machine, and the fleet it leaves is still a fleet.
    pub fn forget(&mut self, name: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|machine| machine.name != name);
        if before == self.machines.len() {
            return false;
        }
        if self.machines.is_empty() {
            self.ticket = None;
            self.pending_until = None;
        }
        true
    }
}

/// Whether a session member is one of the machines in a fleet.
///
/// Free-standing because the client holds the fleet as a plain list rather than
/// as a [`Pairing`] — it is handed the machines and never the ticket — and the
/// one question this file exists to answer must not have two implementations.
pub fn owns_machine(fleet: &[PairedMachine], peer_id: &str, name: &str, kind: MemberKind) -> bool {
    kind.could_be_machine()
        && fleet.iter().any(|machine| match &machine.machine_id {
            Some(known) => known == peer_id,
            // A record from before peer ids: matching on the name is what it
            // has always done, and it upgrades to the line above the first time
            // [`pin_peers`] sees this machine in a session.
            None => machine.name == name,
        })
}

pub fn pairing_path() -> Result<PathBuf, PairingError> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or(PairingError::MissingHome)?;
    Ok(base.join("p2pmux").join("pairing.toml"))
}

/// Read the pairing record, treating a missing file as "nothing paired yet".
///
/// A corrupt file is *not* treated that way. Silently forgetting every paired
/// machine because one line failed to parse would look exactly like an unpair
/// the user never asked for.
pub fn load_from(path: &Path) -> Result<Pairing, PairingError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Pairing::default()),
        Err(error) => return Err(error.into()),
    };
    toml::from_str(&text).map_err(|error| PairingError::Invalid(error.to_string()))
}

pub fn save_to(path: &Path, pairing: &Pairing) -> Result<(), PairingError> {
    let body = toml::to_string_pretty(pairing)
        .map_err(|error| PairingError::Invalid(error.to_string()))?;
    atomic_write(path, &format!("{PAIRING_HEADER}{body}"))
}

pub fn load() -> Result<Pairing, PairingError> {
    load_from(&pairing_path()?)
}

pub fn save(pairing: &Pairing) -> Result<(), PairingError> {
    save_to(&pairing_path()?, pairing)
}

/// Record the session that machines paired from here will rejoin.
///
/// The one write that turns handing out a code into a pairing: without a
/// ticket, bare `p2pmux` has nothing to rejoin and [`remember_peers`] declines
/// to record the machines that turn up, so the fleet forgets a machine the
/// moment it goes to sleep.
///
/// It deliberately leaves `accepts_work` alone. That question is asked once,
/// its answer is default-deny, and a path that never asks it must not answer
/// it — least of all in the permissive direction.
pub fn offer(ticket: &str) -> Result<(), PairingError> {
    let path = pairing_path()?;
    let mut pairing = load_from(&path)?;
    if !offer_into(&mut pairing, ticket, now_unix()) {
        return Ok(());
    }
    save_to(&path, &pairing)
}

/// What opening the add-machine panel does to the record. The pure half of
/// [`offer`], and `false` when the file already says this.
///
/// The window is half of the offer, not a detail of the CLI that made one. This
/// panel is older than [`Pairing::pending_until`], and when the window arrived
/// only `p2pmux pair` learned to open it — so a machine invited from the inbox
/// arrived to a fleet that admits nobody, was never written down by
/// [`pin_into`], and left this machine holding a ticket for a pairing that
/// could not complete. Bare `p2pmux` then dialled that ticket, for half a
/// minute, on every run.
///
/// Each visit to the panel opens a window, exactly as each `p2pmux pair` does:
/// the window admits one machine and closes, so a second machine is a second
/// invitation. One already open is left to run out on its own schedule rather
/// than extended by a redraw.
fn offer_into(pairing: &mut Pairing, ticket: &str, now: u64) -> bool {
    if pairing.ticket.as_deref() == Some(ticket) && pairing.pairing_window_open(now) {
        return false;
    }
    pairing.ticket = Some(ticket.to_owned());
    pairing.open_pairing_window(now);
    true
}

/// Attach identities and current names to machines already in the fleet.
///
/// Called by the node when the member list changes. It **never adds a machine**,
/// and that is the point. It used to: any peer of a session, once this machine
/// was paired at all, was written in as fleet — so a guest who joined with a
/// code you handed out became one of your machines, inherited whatever the
/// fleet is allowed to do, and stayed after they left. The guard that was
/// supposed to prevent it only asked whether *this* machine was paired, which
/// is true in exactly the situation the guest turns up in.
///
/// Machines enter the fleet through `p2pmux pair` and no other way. What this
/// does is keep the records honest once they are in it: pin the peer id of a
/// machine paired before ids were recorded, and follow a machine that was
/// renamed.
///
/// Machines are never removed either. A paired machine that is switched off
/// stops being a member and must keep its row — saying `asleep` is the entire
/// reason the record exists. Unpairing is `p2pmux unpair`, an explicit act.
pub fn pin_peers(seen: &[SeenMachine]) -> Result<(), PairingError> {
    if seen.is_empty() {
        return Ok(());
    }
    let path = pairing_path()?;
    let mut pairing = load_from(&path)?;
    if !pairing.can_rejoin() {
        return Ok(());
    }
    let before = pairing.clone();
    pin_into(&mut pairing, seen, now_unix());
    if pairing == before {
        return Ok(());
    }
    save_to(&path, &pairing)
}

/// What a session's member list does to a fleet record. The pure half of
/// [`pin_peers`], so the rules can be tested without a filesystem.
fn pin_into(pairing: &mut Pairing, seen: &[SeenMachine], now: u64) {
    for machine in seen {
        // Writing to the fleet asks the strict question. Silence is enough to
        // be *recognized* as a machine you own; it is not enough to be written
        // in as one, and neither is a peer that says a person is driving it.
        if !machine.kind.declared_machine() {
            continue;
        }
        if pairing.owns(&machine.machine_id, &machine.name, machine.kind) {
            pairing.remember(&machine.name, machine.identity(), None);
            continue;
        }
        // Not in the fleet. The only way in is through a window `p2pmux pair`
        // opened, and it admits one machine and then closes: a second arrival
        // has to be invited by a second `p2pmux pair`.
        if pairing.pairing_window_open(now) {
            pairing.pending_until = None;
            pairing.remember(&machine.name, machine.identity(), None);
        }
    }
}

/// Seconds since the epoch, or `0` on a clock that will not answer.
///
/// Zero closes the pairing window rather than opening it, which is the right
/// way for this to fail: a machine that cannot tell the time should not be
/// admitting members to a fleet on the strength of a deadline.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// One member of a live session, as the fleet record cares about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeenMachine {
    pub name: String,
    /// Hex-encoded machine id, already verified against the peer id the
    /// transport authenticated. Empty when that member proved nothing.
    pub machine_id: String,
    /// What that peer said it is. See [`Pairing::owns`].
    pub kind: MemberKind,
}

impl SeenMachine {
    /// The identity to record, or `None` when this member proved none.
    ///
    /// Recording an empty one would write a row that matches nothing and
    /// blocks the real identity from ever being pinned to it.
    pub fn identity(&self) -> Option<String> {
        (!self.machine_id.is_empty()).then(|| self.machine_id.clone())
    }
}

/// Best-effort read for the paths where a failure must not stop the UI.
///
/// The inbox draws its machines rail from this. A pairing file that cannot be
/// read is worth a missing strip, never a client that refuses to start.
pub fn load_or_empty() -> Pairing {
    load().unwrap_or_default()
}

fn atomic_write(path: &Path, text: &str) -> Result<(), PairingError> {
    let parent = path.parent().ok_or(PairingError::MissingHome)?;
    fs::create_dir_all(parent)?;
    let file_name = path.file_name().ok_or(PairingError::MissingHome)?;
    let temporary = parent.join(format!(
        ".{}.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        PAIRING_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    fs::write(&temporary, text)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MemberKind, PAIRING_WINDOW_SECONDS, Pairing, SeenMachine, WorkDecision, load_from,
        now_unix, offer_into, pin_into, save_to,
    };
    use crate::tui::PairedMachine;

    fn temporary_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("p2pmux-pairing-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        path.join("pairing.toml")
    }

    #[test]
    fn a_missing_file_is_nothing_paired_rather_than_an_error() {
        let path = temporary_path("missing");
        let pairing = load_from(&path).expect("a missing file reads as empty");

        assert_eq!(pairing, Pairing::default());
        assert!(!pairing.can_rejoin());
    }

    #[test]
    fn a_corrupt_file_is_an_error_rather_than_a_silent_unpair() {
        // Forgetting every paired machine because one line failed to parse
        // would look exactly like an unpair the user never asked for.
        let path = temporary_path("corrupt");
        std::fs::write(&path, "this is not toml = = =").expect("write");

        assert!(load_from(&path).is_err());
    }

    #[test]
    fn a_saved_record_round_trips() {
        let path = temporary_path("round-trip");
        let mut pairing = Pairing {
            ticket: Some(String::from("p2pmux-ticket")),
            accepts_work: true,
            pending_until: None,
            daemon_declined: false,
            machines: Vec::new(),
            work: Default::default(),
            enrol: None,
        };
        pairing.remember("desktop", Some(String::from("aa")), Some(false));
        pairing.remember("droplet", Some(String::from("bb")), Some(true));

        save_to(&path, &pairing).expect("save");
        assert_eq!(load_from(&path).expect("load"), pairing);
        assert!(pairing.can_rejoin());
    }

    #[test]
    fn pairing_twice_with_a_machine_updates_it_rather_than_duplicating_it() {
        let mut pairing = Pairing::default();
        pairing.remember("desktop", None, None);
        pairing.remember("desktop", None, Some(true));

        assert_eq!(
            pairing.machines,
            vec![PairedMachine {
                name: String::from("desktop"),
                machine_id: None,
                accepts_work: Some(true),
            }],
            "a later sighting upgrades an unanswered machine and never downgrades one"
        );
        assert!(pairing.forget("desktop"));
        assert!(!pairing.forget("desktop"));
    }

    #[test]
    fn a_machine_invited_from_the_inbox_is_admitted_like_one_invited_by_pair() {
        // The panel is older than the window, and when the window arrived only
        // `p2pmux pair` learned to open one. So the machine that accepted an
        // invitation from the inbox was judged a guest and never written down.
        let mut pairing = Pairing::default();
        assert!(offer_into(&mut pairing, "p2pmux-ticket", 1_000));

        pin_into(
            &mut pairing,
            &[SeenMachine {
                name: String::from("desktop"),
                machine_id: String::from("beef"),
                kind: MemberKind::Machine,
            }],
            1_001,
        );

        assert_eq!(
            pairing.machines.len(),
            1,
            "the machine that answered the invitation is in the fleet"
        );
        assert!(!pairing.pairing_window_open(1_001), "and the window shuts");
    }

    #[test]
    fn opening_the_panel_again_is_a_second_invitation() {
        let mut pairing = Pairing::default();
        assert!(offer_into(&mut pairing, "p2pmux-ticket", 1_000));

        assert!(
            !offer_into(&mut pairing, "p2pmux-ticket", 1_100),
            "a window still open is not re-opened by a redraw"
        );
        assert!(
            offer_into(
                &mut pairing,
                "p2pmux-ticket",
                1_000 + PAIRING_WINDOW_SECONDS + 1
            ),
            "an expired one is, because a second machine needs a second invitation"
        );
        assert!(pairing.pairing_window_open(1_000 + PAIRING_WINDOW_SECONDS + 2));
    }

    #[test]
    fn unpairing_the_last_machine_gives_up_the_session_it_took_with_it() {
        // The ticket outliving the last machine is what made `p2pmux machines`
        // and bare `p2pmux` disagree: nothing paired, half a minute of dialling.
        let mut pairing = Pairing {
            ticket: Some(String::from("p2pmux-ticket")),
            ..Pairing::default()
        };
        pairing.open_pairing_window(1_000);
        pairing.remember("desktop", Some(String::from("aa")), None);

        assert!(pairing.forget("desktop"));

        assert_eq!(pairing.ticket, None, "the fleet's session goes with it");
        assert_eq!(
            pairing.pending_until, None,
            "and so does an open invitation"
        );
        assert!(!pairing.can_rejoin());
    }

    #[test]
    fn unpairing_one_of_two_machines_keeps_the_session_the_other_rejoins() {
        let mut pairing = Pairing {
            ticket: Some(String::from("p2pmux-ticket")),
            ..Pairing::default()
        };
        pairing.remember("desktop", Some(String::from("aa")), None);
        pairing.remember("droplet", Some(String::from("bb")), None);

        assert!(pairing.forget("desktop"));

        assert_eq!(pairing.ticket.as_deref(), Some("p2pmux-ticket"));
        assert!(pairing.can_rejoin(), "a fleet of one is still a fleet");
    }

    #[test]
    fn forgetting_a_machine_that_was_never_paired_changes_nothing() {
        let mut pairing = Pairing {
            ticket: Some(String::from("p2pmux-ticket")),
            ..Pairing::default()
        };
        pairing.remember("desktop", Some(String::from("aa")), None);

        assert!(!pairing.forget("laptop"));

        assert_eq!(
            pairing.ticket.as_deref(),
            Some("p2pmux-ticket"),
            "a typo'd name is not an unpair, least of all of the whole fleet"
        );
        assert_eq!(pairing.machines.len(), 1);
    }

    #[test]
    fn a_machine_paired_before_peer_ids_is_adopted_rather_than_duplicated() {
        // The upgrade path. A record written by an older build has only a name;
        // the first sighting of that machine in a session attaches its identity
        // to the row that is already there.
        let mut pairing = Pairing::default();
        pairing.remember("droplet", None, Some(true));
        pairing.remember("droplet", Some(String::from("beef")), None);

        assert_eq!(
            pairing.machines,
            vec![PairedMachine {
                name: String::from("droplet"),
                machine_id: Some(String::from("beef")),
                accepts_work: Some(true),
            }],
            "the identity lands on the existing row, and the answer it already gave survives"
        );
    }

    #[test]
    fn a_renamed_machine_is_still_the_same_machine() {
        let mut pairing = Pairing::default();
        pairing.remember("droplet", Some(String::from("beef")), None);
        pairing.remember("fra1", Some(String::from("beef")), None);

        assert_eq!(
            pairing.machines.len(),
            1,
            "identity is the peer id, not the name"
        );
        assert_eq!(pairing.machines[0].name, "fra1");
    }

    #[test]
    fn a_stranger_cannot_pass_for_a_machine_you_own() {
        // The whole ownership question. Being in a session with someone, and
        // even declaring yourself a machine, is not being one of their machines.
        let mut pairing = Pairing::default();
        pairing.remember("droplet", Some(String::from("beef")), None);

        assert!(pairing.owns("beef", "droplet", MemberKind::Machine));
        assert!(
            !pairing.owns("cafe", "droplet", MemberKind::Machine),
            "a peer that took the same name is still not the machine that was paired"
        );
        assert!(
            !pairing.owns("cafe", "laptop", MemberKind::Machine),
            "declaring yourself a machine wins nothing without being in the file"
        );
    }

    #[test]
    fn a_peer_that_says_a_person_is_driving_is_never_one_of_your_machines() {
        // Self-declaration narrows and never widens. A machine in the fleet
        // that reports a human at it stops being offered as compute, which is
        // the only direction this claim is allowed to move anything.
        let mut pairing = Pairing::default();
        pairing.remember("droplet", Some(String::from("beef")), None);

        assert!(!pairing.owns("beef", "droplet", MemberKind::Person));
    }

    #[test]
    fn a_machine_that_proved_nothing_is_never_matched_against_one_that_did() {
        // The failure this could have had: an empty machine id compared equal
        // to every record that has none, so a member which proved nothing would
        // be recognized as whichever machine happened to be listed.
        let mut pairing = Pairing::default();
        pairing.remember("droplet", Some(String::from("beef")), None);

        assert!(
            !pairing.owns("", "droplet", MemberKind::Machine),
            "proving nothing must not match a record that names an identity"
        );
    }

    /// The reason this is a machine id and not the node's peer id: a node's key
    /// is generated per process, so the same machine in a second session is a
    /// different peer. Ownership has to survive that, and only an identity that
    /// outlives the process does.
    #[test]
    fn the_same_machine_in_another_session_is_still_yours() {
        let mut pairing = Pairing::default();
        pairing.remember("droplet", Some(String::from("beef")), None);

        // Same machine, new node, new peer id — and the record still knows it.
        assert!(pairing.owns("beef", "droplet", MemberKind::Machine));
        assert!(
            pairing.owns("beef", "droplet-renamed", MemberKind::Machine),
            "and a machine that was renamed is still the machine you paired"
        );
    }

    #[test]
    fn a_machine_that_says_nothing_is_still_yours() {
        // An older build, or a node that started before this box was paired.
        // Silence must read as it always did — the fleet record alone — or an
        // upgrade would quietly empty someone's fleet.
        let mut pairing = Pairing::default();
        pairing.remember("droplet", Some(String::from("beef")), None);

        assert!(pairing.owns("beef", "droplet", MemberKind::Unspecified));
    }

    #[test]
    fn a_guest_of_a_session_never_joins_the_fleet() {
        // The hole this issue was really about. Every peer of a session used to
        // be written in as one of your machines the moment this machine was
        // paired at all, so a collaborator you handed a code to became fleet
        // and stayed there after they left.
        let path = temporary_path("guest");
        std::fs::write(&path, "ticket = \"t\"\n").expect("write");
        let seen = vec![SeenMachine {
            name: String::from("their-laptop"),
            machine_id: String::from("cafe"),
            kind: MemberKind::Machine,
        }];

        let mut pairing = load_from(&path).expect("load");
        assert!(!pairing.pairing_window_open(now_unix()));
        pin_into(&mut pairing, &seen, now_unix());

        assert!(
            pairing.machines.is_empty(),
            "a peer of a session is not a machine you own"
        );
    }

    #[test]
    fn the_window_opened_by_pairing_admits_one_machine_and_closes() {
        let mut pairing = Pairing {
            ticket: Some(String::from("t")),
            ..Pairing::default()
        };
        pairing.open_pairing_window(1_000);
        let seen = vec![
            SeenMachine {
                name: String::from("droplet"),
                machine_id: String::from("beef"),
                kind: MemberKind::Machine,
            },
            SeenMachine {
                name: String::from("their-laptop"),
                machine_id: String::from("cafe"),
                kind: MemberKind::Machine,
            },
        ];

        pin_into(&mut pairing, &seen, 1_001);

        assert_eq!(
            pairing.machines.len(),
            1,
            "one `p2pmux pair` invites one machine"
        );
        assert_eq!(pairing.machines[0].name, "droplet");
        assert!(!pairing.pairing_window_open(1_001), "and then it is shut");
    }

    #[test]
    fn an_expired_window_admits_nobody() {
        let mut pairing = Pairing {
            ticket: Some(String::from("t")),
            ..Pairing::default()
        };
        pairing.open_pairing_window(1_000);
        let seen = vec![SeenMachine {
            name: String::from("droplet"),
            machine_id: String::from("beef"),
            kind: MemberKind::Machine,
        }];

        pin_into(&mut pairing, &seen, 1_000 + PAIRING_WINDOW_SECONDS + 1);

        assert!(pairing.machines.is_empty());
    }

    #[test]
    fn a_machine_already_in_the_fleet_has_its_identity_pinned_without_a_window() {
        let mut pairing = Pairing {
            ticket: Some(String::from("t")),
            ..Pairing::default()
        };
        pairing.remember("droplet", None, Some(true));
        let seen = vec![SeenMachine {
            name: String::from("droplet"),
            machine_id: String::from("beef"),
            kind: MemberKind::Machine,
        }];

        pin_into(&mut pairing, &seen, 9_999);

        assert_eq!(pairing.machines[0].machine_id.as_deref(), Some("beef"));
    }

    #[test]
    fn a_machine_that_never_said_anything_allows_nothing() {
        // Default closed, twice over. This is the criterion the whole issue
        // turns on: `accepts_work` off refuses, and `accepts_work` on with
        // nothing written down still refuses, because a yes at pair time is
        // consent to be asked and not a free shell.
        let mut pairing = Pairing::default();
        assert_eq!(
            pairing.work_decision(&[String::from("hermes"), String::from("chat")]),
            WorkDecision::Refuse
        );
        assert_eq!(pairing.work_decision(&[]), WorkDecision::Refuse);

        pairing.accepts_work = true;
        assert_eq!(
            pairing.work_decision(&[String::from("hermes"), String::from("chat")]),
            WorkDecision::Refuse,
            "saying yes to being asked is not saying yes to everything"
        );
    }

    #[test]
    fn the_allowlist_is_the_whole_command_and_not_a_prefix() {
        let mut pairing = Pairing {
            accepts_work: true,
            ..Pairing::default()
        };
        pairing.work.allow = vec![String::from("hermes chat")];

        assert_eq!(
            pairing.work_decision(&[String::from("hermes"), String::from("chat")]),
            WorkDecision::Allow
        );
        assert_eq!(
            pairing.work_decision(&[
                String::from("hermes"),
                String::from("chat"),
                String::from("--yolo"),
            ]),
            WorkDecision::Refuse,
            "an allowed command must not be extendable with arguments nobody agreed to"
        );
        assert_eq!(
            pairing.work_decision(&[String::from("hermes")]),
            WorkDecision::Refuse
        );
    }

    #[test]
    fn a_free_shell_has_to_be_asked_for_by_name() {
        // The entry that gives away everything the others were carefully not
        // giving away. It is spelled out so that nobody grants it by accident
        // while listing the agent commands they meant to allow.
        let mut pairing = Pairing {
            accepts_work: true,
            ..Pairing::default()
        };
        pairing.work.allow = vec![String::from("hermes chat")];
        assert_eq!(pairing.work_decision(&[]), WorkDecision::Refuse);

        pairing.work.allow.push(String::from("shell"));
        assert_eq!(pairing.work_decision(&[]), WorkDecision::Allow);
    }

    /// The entry `p2pmux work allow` writes has to be the entry
    /// `work_decision` then reads. A grant that does not take is worse than no
    /// grant: the user is told they opened the gate and the gate stays shut.
    #[test]
    fn what_allow_writes_is_what_the_decision_reads() {
        let mut pairing = Pairing {
            accepts_work: true,
            ..Pairing::default()
        };
        let claude = vec![String::from("claude")];
        let attach = vec![
            String::from("p2pmux"),
            String::from("attach"),
            String::from("dakar"),
        ];

        assert!(pairing.work.allow(&[]));
        assert!(pairing.work.allow(&claude));
        assert!(pairing.work.allow(&attach));
        assert!(
            !pairing.work.allow(&claude),
            "the same grant twice is one entry, not two"
        );

        assert_eq!(pairing.work_decision(&[]), WorkDecision::Allow);
        assert_eq!(pairing.work_decision(&claude), WorkDecision::Allow);
        assert_eq!(pairing.work_decision(&attach), WorkDecision::Allow);
        assert_eq!(
            pairing.work_decision(&[String::from("claude"), String::from("--dangerously")]),
            WorkDecision::Refuse,
            "matched in full: an allowed command cannot be extended with arguments"
        );

        // `deny` reaches the same entry whether the shell is spelled out or
        // left implicit, because both spellings reach it on the way in.
        assert!(pairing.work.deny(&[String::from("shell")]));
        assert_eq!(pairing.work_decision(&[]), WorkDecision::Refuse);
        assert!(!pairing.work.deny(&[]), "and denying twice is not an error");
        assert_eq!(
            pairing.work_decision(&claude),
            WorkDecision::Allow,
            "denying one entry leaves the rest alone"
        );
    }

    /// The token is meant to live in a provisioning script, so printing it
    /// twice has to give the same string — one that rotated on being looked at
    /// would strand every machine image built from the last look.
    #[test]
    fn an_enrolment_token_is_stable_until_it_is_revoked() {
        let mut pairing = Pairing {
            ticket: Some(String::from("p2pmux-v3:abc")),
            ..Pairing::default()
        };

        let first = super::enrolment_token(&mut pairing, 10).expect("mint");
        let again = super::enrolment_token(&mut pairing, 20).expect("mint");
        assert_eq!(first, again, "looking at it must not rotate it");
        assert!(pairing.enrolment_admits(&first));

        assert!(super::revoke_enrolment(&mut pairing));
        assert!(!super::revoke_enrolment(&mut pairing));
        assert!(
            !pairing.enrolment_admits(&first),
            "a revoked token is a refusal"
        );
        assert!(
            !pairing.enrolment_admits(""),
            "and an empty secret is never a wildcard, whatever is recorded"
        );

        let rotated = super::enrolment_token(&mut pairing, 30).expect("mint");
        assert_ne!(first, rotated);
        assert!(!pairing.enrolment_admits(&first));
    }

    /// The invitation carries both halves in one string, because a form that
    /// needs two values pasted in the right order is a form that gets pasted
    /// wrong. Anything else is refused rather than half-understood: on a box
    /// with nobody at it, "enrolled into nothing, reported success" is
    /// invisible until you go looking for a machine that is not there.
    #[test]
    fn an_enrolment_invite_round_trips_and_refuses_everything_else() {
        let invite = super::EnrolInvite {
            ticket: String::from("p2pmux-v3:abc"),
            secret: String::from("f00d"),
        };
        let encoded = invite.encode();
        assert!(encoded.starts_with(super::ENROL_PREFIX));
        assert_eq!(
            super::EnrolInvite::decode(&encoded).expect("decode"),
            invite
        );
        // Survives the whitespace a YAML block or a copy-paste adds.
        assert_eq!(
            super::EnrolInvite::decode(&format!("  {encoded}\n")).expect("decode"),
            invite
        );

        for refused in [
            "",
            "p2pmux-v3:abc",
            super::ENROL_PREFIX,
            &format!("{}!!!not-base64", super::ENROL_PREFIX),
            &super::EnrolInvite {
                ticket: String::new(),
                secret: String::from("f00d"),
            }
            .encode(),
            &super::EnrolInvite {
                ticket: String::from("p2pmux-v3:abc"),
                secret: String::new(),
            }
            .encode(),
        ] {
            assert!(
                super::EnrolInvite::decode(refused).is_err(),
                "must refuse {refused:?}"
            );
        }
    }

    #[test]
    fn confirm_turns_an_allowed_command_into_a_question() {
        let mut pairing = Pairing {
            accepts_work: true,
            ..Pairing::default()
        };
        pairing.work.allow = vec![String::from("shell")];
        pairing.work.confirm = true;

        assert_eq!(pairing.work_decision(&[]), WorkDecision::Ask);
        assert_eq!(
            pairing.work_decision(&[String::from("claude")]),
            WorkDecision::Refuse,
            "asking is only ever about a command that was already allowed"
        );
    }

    #[test]
    fn accepts_work_defaults_to_off() {
        // Default-deny. "Accepts work from you" is the consent primitive that
        // will later make remote spawn legal; a default of yes would make a
        // join code into remote code execution on someone's desktop.
        let path = temporary_path("default-deny");
        std::fs::write(&path, "ticket = \"t\"\n").expect("write");

        let pairing = load_from(&path).expect("load");
        assert!(!pairing.accepts_work);
    }
}

/// The printable form of an enrolment invitation: the fleet's session ticket
/// and the secret that admits a machine to it, in one string.
///
/// One string because it is meant to be pasted into a provisioning script, and
/// a form that needs two values pasted in the right order is a form that gets
/// pasted wrong. Same shape as a join ticket for the same reason — a prefix so
/// it can be recognized, base64 so it survives YAML.
pub const ENROL_PREFIX: &str = "p2pmux-enrol-v1:";

/// The two halves of an enrolment invitation, as they travel.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnrolInvite {
    /// The session every machine in this fleet rejoins.
    pub ticket: String,
    /// The standing secret. See [`EnrolToken`].
    pub secret: String,
}

impl EnrolInvite {
    pub fn encode(&self) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{ENROL_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// Parse one, refusing anything that is not exactly this shape.
    ///
    /// Strict on purpose: the failure this has to avoid is a half-understood
    /// token that enrols a machine into nothing and reports success, which on a
    /// box with nobody sitting at it is invisible until you go looking for a
    /// machine that is not there.
    pub fn decode(input: &str) -> Result<Self, PairingError> {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let body = input
            .trim()
            .strip_prefix(ENROL_PREFIX)
            .ok_or_else(|| PairingError::Invalid(String::from("not an enrolment token")))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| PairingError::Invalid(String::from("malformed enrolment token")))?;
        let invite: Self = serde_json::from_slice(&bytes)
            .map_err(|_| PairingError::Invalid(String::from("malformed enrolment token")))?;
        if invite.ticket.is_empty() || invite.secret.is_empty() {
            return Err(PairingError::Invalid(String::from(
                "incomplete enrolment token",
            )));
        }
        Ok(invite)
    }
}

/// Mint a secret for this fleet, or return the one already minted.
///
/// Stable across calls, because the token is meant to live in a provisioning
/// script: one that changed every time you printed it would silently strand
/// every machine image built from the last one. Replacing it is
/// [`revoke_enrolment`] followed by printing a new one, which is an act with a
/// visible consequence rather than a side effect of looking.
pub fn enrolment_token(pairing: &mut Pairing, now: u64) -> Result<String, PairingError> {
    if let Some(token) = &pairing.enrol {
        return Ok(token.secret.clone());
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| PairingError::Invalid(error.to_string()))?;
    let secret = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    pairing.enrol = Some(EnrolToken {
        secret: secret.clone(),
        created_at: now,
    });
    Ok(secret)
}

/// Withdraw the standing invitation. Machines already in the fleet stay: they
/// were enrolled, and unenrolling one is `p2pmux unpair`.
pub fn revoke_enrolment(pairing: &mut Pairing) -> bool {
    pairing.enrol.take().is_some()
}

/// Write a machine that presented this fleet's enrolment secret into the fleet.
///
/// The one path into a pairing record that is not a human typing a code, and
/// it is deliberately narrow: the caller has already checked the secret against
/// this machine's own record and that the joiner declared itself a machine.
/// What is left here is the write, and the guard that there is a fleet to write
/// to at all — a box with no ticket has no fleet, and a machine "enrolled" into
/// nothing would be a row nobody could reach.
pub fn enrol_machine(name: &str, machine_id: &str) -> Result<(), PairingError> {
    let path = pairing_path()?;
    let mut pairing = load_from(&path)?;
    if !pairing.can_rejoin() || pairing.enrol.is_none() {
        return Ok(());
    }
    let before = pairing.clone();
    let identity = (!machine_id.is_empty()).then(|| machine_id.to_owned());
    pairing.remember(name, identity, None);
    if pairing == before {
        return Ok(());
    }
    save_to(&path, &pairing)
}
