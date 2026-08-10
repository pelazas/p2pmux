//! Where an agent that p2pmux did not start leaves its status.
//!
//! A hook running inside a p2pmux pane reports over the pane's node socket, and
//! that is the whole story for an agent the mux launched. An agent started in
//! some other terminal — or under systemd — has the same hooks installed and
//! nowhere to send them: there is no pane id to report for, and no node socket
//! in its environment. Before this module those agents reached the inbox as a
//! process the scan had found and could say nothing about, which is exactly
//! what `state unknown — no hooks` means to a user who *has* run `p2pmux setup`.
//!
//! So the producer writes one small file instead, named after the agent's own
//! process, in the same private per-user directory the session sockets live in.
//! The scan that already walks this machine's processes reads it back.
//!
//! Three properties this shape buys, none of which a socket would:
//!
//! - **No listener has to exist.** The status is written while no p2pmux is
//!   running at all, and the first inbox opened afterwards still sees it.
//! - **It dies with its agent.** A record is only ever read for a pid the scan
//!   just saw running, and [`sweep`] deletes the rest.
//! - **It stays on this machine.** Same directory, same `0700`, same reasoning
//!   as the agent's message on the pane path: a session is shared, and what an
//!   agent is saying is not the mux's to publish.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::agent_detect::{AgentKind, AgentState};

/// Directory name under the per-user runtime directory.
pub const DIRECTORY_NAME: &str = "agents";

/// One agent's last reported status, as it sits on disk.
///
/// `kind` and `state` are wire strings rather than enums: this file is written
/// by whatever `p2pmux` is on the agent's `PATH` and read by whatever is
/// running the inbox, and those are routinely different builds. An unknown
/// string is refused on read, which is the same rule the socket path applies.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatusRecord {
    pub kind: String,
    pub state: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub working_since_unix_ms: u64,
    /// The agent process's start time, in whole seconds, as the process table
    /// reports it.
    ///
    /// Pids are reused. Without this a record left by an agent that exited
    /// between two scans could be read back as the status of whatever process
    /// inherited its number. `0` means "not known", which is accepted rather
    /// than refused so a producer that could not read its own start time still
    /// reports.
    #[serde(default)]
    pub start_time: u64,
}

impl AgentStatusRecord {
    /// The typed kind and state, or `None` if this build does not know one of
    /// them.
    pub fn parsed(&self) -> Option<(AgentKind, AgentState)> {
        Some((
            AgentKind::from_wire(&self.kind)?,
            AgentState::from_wire(&self.state)?,
        ))
    }
}

/// This user's status directory, resolved once per process.
///
/// Cached because finding it shells out for the uid, and both callers are on a
/// path that would otherwise pay for that repeatedly: the producer runs on every
/// tool call an agent makes, and the reader runs on every process scan.
pub fn default_dir() -> Option<&'static Path> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            crate::session_store::SessionStore::for_current_user()
                .ok()
                .map(|store| store.agent_status_dir())
        })
        .as_deref()
}

fn record_path(directory: &Path, pid: u32) -> PathBuf {
    directory.join(format!("{pid}.json"))
}

/// Write `record` as the status of `pid`, replacing whatever was there.
///
/// Through a temporary file and a rename, so a reader never sees half a record
/// — the scan reads these while hooks are writing them.
pub fn write(directory: &Path, pid: u32, record: &AgentStatusRecord) -> std::io::Result<()> {
    fs::create_dir_all(directory)?;
    fs::set_permissions(
        directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let destination = record_path(directory, pid);
    let temporary = directory.join(format!(".{pid}.{}.tmp", std::process::id()));
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    fs::write(&temporary, &bytes)?;
    fs::rename(&temporary, &destination).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// Read the status of `pid`, if one was written and this build understands it.
///
/// `start_time` is the start time the caller just saw for that pid; a record
/// stamped with a different one belonged to a process that has since exited and
/// is ignored.
pub fn read(directory: &Path, pid: u32, start_time: Option<u64>) -> Option<AgentStatusRecord> {
    let bytes = fs::read(record_path(directory, pid)).ok()?;
    let record: AgentStatusRecord = serde_json::from_slice(&bytes).ok()?;
    record.parsed()?;
    match (record.start_time, start_time) {
        (0, _) | (_, None) => Some(record),
        (recorded, Some(seen)) if recorded == seen => Some(record),
        _ => None,
    }
}

/// Fill in what each of these agents last reported about itself.
///
/// The scan answers which agents are running and this answers what they are
/// doing — the same division the pane path draws between the process scan and a
/// pushed status. An agent with no record keeps [`AgentState::Unknown`], which
/// is what a row with no hooks behind it honestly says.
pub fn attach(directory: &Path, agents: &mut [crate::agent_detect::LooseAgent]) {
    for agent in agents {
        let Some(record) = read(directory, agent.pid, agent.start_time) else {
            continue;
        };
        // A record filed under another kind was written by whatever held this
        // pid before, and it is not this agent's status.
        let Some(state) = record
            .parsed()
            .filter(|(kind, _)| *kind == agent.kind)
            .map(|(_, state)| state)
        else {
            continue;
        };
        agent.state = state;
        agent.message = record.message;
        agent.working_since_unix_ms = record.working_since_unix_ms;
    }
}

/// How long a record is too young to be judged by a process scan.
///
/// A scan is a snapshot taken up to its own interval ago, so a record written
/// since it was taken belongs to a process the scan could not have seen. Longer
/// than the scan interval, so a real agent's first status is never the one that
/// gets deleted — that status can be the only `needs you` it ever sends.
pub const SWEEP_GRACE: Duration = Duration::from_secs(30);

/// Delete every record whose process `is_live` no longer recognizes, ignoring
/// any written within `grace`.
///
/// The only expiry there is. A pushed status on the pane path decays once the
/// pane is back at its shell prompt, because a pane outlives the agent that ran
/// in it; a loose agent *is* its process, so the process being gone is the
/// whole signal — and it means a `needs you` from an agent that has been
/// waiting an hour is still there, which is the point.
pub fn sweep(directory: &Path, grace: Duration, is_live: impl Fn(u32) -> bool) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if is_live(pid) || written_within(&entry, grace) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }
}

/// Whether this file was last written less than `grace` ago. A file whose age
/// cannot be read is treated as new, which errs toward keeping a status.
fn written_within(entry: &fs::DirEntry, grace: Duration) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|age| age < grace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("p2pmux-agent-status-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    fn record(state: &str) -> AgentStatusRecord {
        AgentStatusRecord {
            kind: String::from("claude"),
            state: String::from(state),
            cwd: String::from("/repo"),
            message: String::from("running the tests"),
            working_since_unix_ms: 1_700_000_000_000,
            start_time: 42,
        }
    }

    #[test]
    fn a_written_record_reads_back() {
        let directory = scratch("roundtrip");
        write(&directory, 4321, &record("working")).expect("write");

        assert_eq!(read(&directory, 4321, Some(42)), Some(record("working")));
        // A record left behind by a process that has since exited, whose pid a
        // new one now carries, is not that new process's status.
        assert_eq!(read(&directory, 4321, Some(43)), None);
        // A caller that cannot see a start time takes the record as it stands:
        // refusing it would lose every status on a platform that does not
        // report one.
        assert_eq!(read(&directory, 4321, None), Some(record("working")));
        assert_eq!(read(&directory, 9999, Some(42)), None);

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_record_this_build_cannot_read_is_no_record_at_all() {
        let directory = scratch("unknown");
        fs::create_dir_all(&directory).expect("directory");
        // A kind or a state from a newer build is refused rather than coerced,
        // exactly as the socket path refuses them.
        for raw in [
            r#"{"kind":"gemini","state":"working"}"#,
            r#"{"kind":"claude","state":"compacting"}"#,
            "not json at all",
        ] {
            fs::write(record_path(&directory, 7), raw).expect("write");
            assert_eq!(read(&directory, 7, None), None, "{raw} must not be read");
        }

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn sweeping_keeps_the_living_and_drops_the_rest() {
        let directory = scratch("sweep");
        write(&directory, 11, &record("working")).expect("write");
        write(&directory, 22, &record("pending")).expect("write");
        fs::write(directory.join("notes.txt"), "not ours").expect("write");

        sweep(&directory, Duration::ZERO, |pid| pid == 11);

        assert!(read(&directory, 11, None).is_some());
        assert_eq!(read(&directory, 22, None), None);
        // Anything that is not a record is left alone: this directory is
        // shared with the session sockets' parent and guessing is how a sweep
        // deletes something it did not write.
        assert!(directory.join("notes.txt").exists());

        let _ = fs::remove_dir_all(&directory);
    }

    /// A scan is a snapshot of the process table taken up to its own interval
    /// ago, so it cannot be asked about a process that started since. Without
    /// this, an agent's very first status — which may be the only `needs you`
    /// it ever sends — is the one most likely to be swept.
    #[test]
    fn a_record_written_since_the_scan_survives_it() {
        let directory = scratch("young");
        write(&directory, 33, &record("pending")).expect("write");

        sweep(&directory, Duration::from_secs(30), |_| false);

        assert!(read(&directory, 33, None).is_some());

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_directory_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;

        let directory = scratch("mode");
        write(&directory, 5, &record("working")).expect("write");

        let mode = fs::metadata(&directory)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "an agent's words are not world-readable"
        );

        let _ = fs::remove_dir_all(&directory);
    }
}
