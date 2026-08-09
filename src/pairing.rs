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
//! - **Whether this machine accepts work.** Off by default, asked once during
//!   pairing rather than as a separate configuration step. Nothing acts on it
//!   yet: it is the consent primitive that will later make starting a terminal
//!   on another machine legal without widening the trust model, and it means
//!   *accepts work from me*, never *from anyone with the join code*.
//!
//!   The answer is given on the machine it is about, and there is no channel
//!   back: the only thing that crosses machines is the shared layout, whose
//!   member list is signed and hash-chained, and the inbox is built on never
//!   touching that. So each machine knows its own answer and records `None` for
//!   everyone else, which the fleet list prints as `—` rather than as a refusal
//!   nobody made. Carrying it between machines is a protocol change, and a
//!   deliberate non-goal until something actually acts on the flag.
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

const PAIRING_HEADER: &str = "# p2pmux pairing\n#\n# Machines you have paired with this one, and the session they share.\n# Written by `p2pmux pair`. Delete a [[machine]] block to unpair it.\n\n";

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
    /// it. Recorded during pairing and, for now, only recorded.
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
    #[serde(default, rename = "machine")]
    pub machines: Vec<PairedMachine>,
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
    pub fn remember(&mut self, name: &str, peer_id: Option<String>, accepts_work: Option<bool>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let existing = match peer_id.as_deref() {
            Some(peer_id) => self
                .machines
                .iter_mut()
                .position(|machine| machine.peer_id.as_deref() == Some(peer_id))
                // A machine paired before peer ids were recorded is that same
                // machine turning up with its identity for the first time, not
                // a new one. Adopt the row rather than growing a duplicate.
                .or_else(|| {
                    self.machines
                        .iter()
                        .position(|machine| machine.peer_id.is_none() && machine.name == name)
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
                if peer_id.is_some() {
                    machine.peer_id = peer_id;
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
                peer_id,
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
    pub fn owns(&self, peer_id: &str, name: &str, kind: MemberKind) -> bool {
        owns_machine(&self.machines, peer_id, name, kind)
    }

    /// Whether a machine arriving now was invited by a recent `p2pmux pair`.
    pub fn pairing_window_open(&self, now: u64) -> bool {
        self.pending_until.is_some_and(|until| now < until)
    }

    /// Open the window an arriving machine may join the fleet through.
    pub fn open_pairing_window(&mut self, now: u64) {
        self.pending_until = Some(now.saturating_add(PAIRING_WINDOW_SECONDS));
    }

    pub fn forget(&mut self, name: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|machine| machine.name != name);
        before != self.machines.len()
    }
}

/// Whether a session member is one of the machines in a fleet.
///
/// Free-standing because the client holds the fleet as a plain list rather than
/// as a [`Pairing`] — it is handed the machines and never the ticket — and the
/// one question this file exists to answer must not have two implementations.
pub fn owns_machine(fleet: &[PairedMachine], peer_id: &str, name: &str, kind: MemberKind) -> bool {
    kind.could_be_machine()
        && fleet.iter().any(|machine| match &machine.peer_id {
            Some(known) => known == peer_id,
            // A record from before peer ids: matching on the name is what it
            // has always done, and it upgrades to the line above the first time
            // [`pin_peers`] sees this machine in a session.
            None => machine.name == name,
        })
}

/// The stable text form of a peer id, used everywhere the fleet record and the
/// session store have to talk about the same machine.
pub fn peer_id_hex(peer_id: &[u8]) -> String {
    peer_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
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
    if pairing.ticket.as_deref() == Some(ticket) {
        return Ok(());
    }
    pairing.ticket = Some(ticket.to_owned());
    save_to(&path, &pairing)
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
        if pairing.owns(&machine.peer_id, &machine.name, machine.kind) {
            pairing.remember(&machine.name, Some(machine.peer_id.clone()), None);
            continue;
        }
        // Not in the fleet. The only way in is through a window `p2pmux pair`
        // opened, and it admits one machine and then closes: a second arrival
        // has to be invited by a second `p2pmux pair`.
        if pairing.pairing_window_open(now) {
            pairing.pending_until = None;
            pairing.remember(&machine.name, Some(machine.peer_id.clone()), None);
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
    /// Hex-encoded peer id, which the transport authenticated.
    pub peer_id: String,
    /// What that peer said it is. See [`Pairing::owns`].
    pub kind: MemberKind,
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
        MemberKind, PAIRING_WINDOW_SECONDS, Pairing, SeenMachine, load_from, now_unix, pin_into,
        save_to,
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
            machines: Vec::new(),
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
                peer_id: None,
                accepts_work: Some(true),
            }],
            "a later sighting upgrades an unanswered machine and never downgrades one"
        );
        assert!(pairing.forget("desktop"));
        assert!(!pairing.forget("desktop"));
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
                peer_id: Some(String::from("beef")),
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
            peer_id: String::from("cafe"),
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
                peer_id: String::from("beef"),
                kind: MemberKind::Machine,
            },
            SeenMachine {
                name: String::from("their-laptop"),
                peer_id: String::from("cafe"),
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
            peer_id: String::from("beef"),
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
            peer_id: String::from("beef"),
            kind: MemberKind::Machine,
        }];

        pin_into(&mut pairing, &seen, 9_999);

        assert_eq!(pairing.machines[0].peer_id.as_deref(), Some("beef"));
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
