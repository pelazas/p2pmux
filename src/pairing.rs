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
//! - **The names of the machines paired with this one**, so the inbox can say
//!   `asleep` about a machine that is switched off rather than forgetting it.
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

use crate::tui::PairedMachine;

static PAIRING_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    #[serde(default, rename = "machine")]
    pub machines: Vec<PairedMachine>,
}

impl Pairing {
    /// Whether bare `p2pmux` can rejoin without being handed a code.
    pub fn can_rejoin(&self) -> bool {
        self.ticket.is_some()
    }

    /// Record a machine, or update the one already under that name.
    ///
    /// Pairing twice with the same machine is a re-pair, not a second machine:
    /// a user who re-runs `p2pmux pair` after moving house should end up with
    /// one desktop, not two.
    pub fn remember(&mut self, name: &str, accepts_work: Option<bool>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        match self
            .machines
            .iter_mut()
            .find(|machine| machine.name == name)
        {
            Some(machine) => {
                // Only ever upgraded from "never said" to an answer. A machine
                // that told us once must not be silently un-told by a later
                // sighting that carried nothing.
                if accepts_work.is_some() {
                    machine.accepts_work = accepts_work;
                }
            }
            None => self.machines.push(PairedMachine {
                name: name.to_owned(),
                accepts_work,
            }),
        }
        self.machines
            .sort_by(|left, right| left.name.cmp(&right.name));
    }

    pub fn forget(&mut self, name: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|machine| machine.name != name);
        before != self.machines.len()
    }
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

/// Record the machines currently in the session, if this machine is paired.
///
/// Called by the node when the member list changes. It is a no-op on a session
/// pairing knows nothing about, and that guard is the whole security of it: a
/// guest who joined with a code you handed out is a collaborator, not a machine
/// you own, and must never end up in your fleet or inherit an `accepts work`
/// answer you gave about your own desktop.
///
/// Machines are only ever added. A paired machine that is switched off stops
/// being a member and must keep its row — saying `asleep` is the entire reason
/// the record exists. Unpairing is `p2pmux unpair`, an explicit act.
pub fn remember_peers(names: &[String]) -> Result<(), PairingError> {
    if names.is_empty() {
        return Ok(());
    }
    let path = pairing_path()?;
    let mut pairing = load_from(&path)?;
    if !pairing.can_rejoin() {
        return Ok(());
    }
    let before = pairing.machines.clone();
    for name in names {
        if !pairing.machines.iter().any(|machine| &machine.name == name) {
            pairing.remember(name, None);
        }
    }
    if pairing.machines == before {
        return Ok(());
    }
    save_to(&path, &pairing)
}

/// Best-effort read for the paths where a failure must not stop the UI.
///
/// The inbox draws a machine strip from this. A pairing file that cannot be
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
    use super::{Pairing, load_from, save_to};
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
            machines: Vec::new(),
        };
        pairing.remember("desktop", Some(false));
        pairing.remember("droplet", Some(true));

        save_to(&path, &pairing).expect("save");
        assert_eq!(load_from(&path).expect("load"), pairing);
        assert!(pairing.can_rejoin());
    }

    #[test]
    fn pairing_twice_with_a_machine_updates_it_rather_than_duplicating_it() {
        let mut pairing = Pairing::default();
        pairing.remember("desktop", None);
        pairing.remember("desktop", Some(true));

        assert_eq!(
            pairing.machines,
            vec![PairedMachine {
                name: String::from("desktop"),
                accepts_work: Some(true),
            }],
            "a later sighting upgrades an unanswered machine and never downgrades one"
        );
        assert!(pairing.forget("desktop"));
        assert!(!pairing.forget("desktop"));
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
