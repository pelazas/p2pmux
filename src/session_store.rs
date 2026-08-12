//! Small, durable finder records for locally running sessions.
//!
//! The record intentionally contains only enough information to find a live node.  Session
//! state (PTYs, tickets, screens, and focus) belongs to the node process, never to disk.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use getrandom::fill;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

const CITY_NAMES: &[&str] = &[
    "abu-dhabi",
    "accra",
    "amsterdam",
    "ankara",
    "athens",
    "auckland",
    "bangkok",
    "barcelona",
    "beijing",
    "beirut",
    "belgrade",
    "berlin",
    "bogota",
    "boston",
    "brasilia",
    "brussels",
    "buenos-aires",
    "cairo",
    "calgary",
    "cape-town",
    "caracas",
    "chicago",
    "copenhagen",
    "dakar",
    "dallas",
    "delhi",
    "denver",
    "dhaka",
    "doha",
    "dublin",
    "edinburgh",
    "firenze",
    "frankfurt",
    "geneva",
    "guangzhou",
    "guatemala-city",
    "hanoi",
    "helsinki",
    "hong-kong",
    "honolulu",
    "houston",
    "istanbul",
    "jakarta",
    "jerusalem",
    "johannesburg",
    "kathmandu",
    "kuala-lumpur",
    "kyiv",
    "lagos",
    "lima",
    "lisbon",
    "london",
    "los-angeles",
    "madrid",
    "manila",
    "melbourne",
    "mexico-city",
    "miami",
    "milan",
    "montreal",
    "mumbai",
    "munich",
    "nairobi",
    "new-york",
    "osaka",
    "oslo",
    "ottawa",
    "paris",
    "perth",
    "philadelphia",
    "prague",
    "reykjavik",
    "rio-de-janeiro",
    "riyadh",
    "rome",
    "san-diego",
    "san-francisco",
    "san-jose",
    "santiago",
    "sao-paulo",
    "seattle",
    "seoul",
    "shanghai",
    "singapore",
    "stockholm",
    "sydney",
    "taipei",
    "tallinn",
    "tehran",
    "tel-aviv",
    "tokyo",
    "toronto",
    "vancouver",
    "vienna",
    "warsaw",
    "washington",
    "wellington",
    "zurich",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Coordinator,
    Member,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDescriptor {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub socket_path: PathBuf,
    pub node_pid: u32,
    pub role: SessionRole,
    pub created_at: u64,
    /// The coordinator's printable join ticket, so `p2pmux ticket <name>` can read it back
    /// out of process. Members never mint one, and the record is deleted with the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// The short code the ticket was published under, when the rendezvous accepted it.
    /// Absent on a member, and on a coordinator that could not reach the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_code: Option<String>,
    /// The ticket this machine joined, for a session it did not create.
    ///
    /// A coordinator's own ticket is above; this is the mirror image, and it
    /// exists so that a machine following an invitation can tell whether it is
    /// already in that session. Invitations are re-announced on a timer — a
    /// machine that was asleep has to hear about a session started while it was
    /// — so "am I already there" is asked constantly and has to survive the
    /// node being restarted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_ticket: Option<String>,
    /// The machines currently in this session, with how many agents each is
    /// running. Written by the node whenever membership changes.
    ///
    /// This exists so a command can answer "is that machine awake" without
    /// attaching: attaching would bump the generation and detach whatever
    /// client is already on the session, which is far too high a price for a
    /// question as small as a status column.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<SessionPeer>,
}

/// One machine in a live session, as the record describes it out of process.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionPeer {
    pub name: String,
    pub agents: usize,
    /// Whether this row is the machine holding the record.
    #[serde(default)]
    pub this_machine: bool,
    /// Hex-encoded machine id, already verified. Written so that out-of-process
    /// readers — `p2pmux machines`, and `p2pmux pair` deciding which arrival to
    /// record — can ask the fleet record about a member without attaching to
    /// the session. Empty when that member proved no identity.
    #[serde(default)]
    pub machine_id: String,
    /// What that member said it is, carried through from the member list.
    #[serde(default)]
    pub kind: crate::layout::MemberKind,
}

impl SessionDescriptor {
    pub fn new(
        id: String,
        name: String,
        socket_path: PathBuf,
        node_pid: u32,
        role: SessionRole,
    ) -> Self {
        Self {
            version: VERSION,
            id,
            name,
            socket_path,
            node_pid,
            role,
            created_at: now_secs(),
            ticket: None,
            join_code: None,
            joined_ticket: None,
            peers: Vec::new(),
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION
            || !valid_id(&self.id)
            || !valid_name(&self.name)
            || self.node_pid == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed session descriptor",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    socket_dir: PathBuf,
    /// How this store asks whether a recorded node is still running.
    ///
    /// A field rather than a direct call so a test can describe a live node
    /// without having to start one: the real answer comes from the process
    /// table, which a test cannot fabricate.
    node_alive: fn(u32) -> bool,
}

impl SessionStore {
    pub fn for_current_user() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let uid = current_uid()?;
        Ok(Self::at(
            sessions_dir(Path::new(&home), std::env::var_os("XDG_STATE_HOME")),
            socket_dir(std::env::var_os("XDG_RUNTIME_DIR"), &uid),
        ))
    }

    /// The sessions directory a p2pmux started with this `HOME`, and with no XDG
    /// overrides in its environment, would read and write.
    ///
    /// Exposed so a test can point its fixture wherever the binary under test is
    /// about to look, rather than restating a platform's layout and drifting.
    pub fn sessions_dir_for_home(home: &Path) -> PathBuf {
        sessions_dir(home, None)
    }

    /// Test-friendly constructor.  Directories are created lazily with private permissions.
    pub fn at(sessions_dir: PathBuf, socket_dir: PathBuf) -> Self {
        Self {
            sessions_dir,
            socket_dir,
            node_alive: crate::agent_detect::node_process_is_alive,
        }
    }

    /// Where an agent running outside every pane leaves its status.
    ///
    /// Beside the session sockets rather than in the state directory: these
    /// records are about processes that are running *now*, they are worthless
    /// once the machine reboots, and this is the directory that already gets
    /// `0700` and already gets cleared with the sockets. See [`crate::agent_status`].
    pub fn agent_status_dir(&self) -> PathBuf {
        self.socket_dir.join(crate::agent_status::DIRECTORY_NAME)
    }

    pub fn socket_path(&self, id: &str) -> io::Result<PathBuf> {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid session id",
            ));
        }
        self.ensure_dirs()?;
        Ok(self.socket_dir.join(format!("{id}.sock")))
    }

    pub fn write(&self, descriptor: &SessionDescriptor) -> io::Result<()> {
        descriptor.validate()?;
        self.ensure_dirs()?;
        let destination = self.sessions_dir.join(format!("{}.json", descriptor.id));
        let temporary =
            self.sessions_dir
                .join(format!(".{}.{}.tmp", descriptor.id, std::process::id()));
        let bytes = serde_json::to_vec(descriptor).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, destination)?;
        Ok(())
    }

    pub fn read(&self, id: &str) -> io::Result<SessionDescriptor> {
        let descriptor: SessionDescriptor =
            serde_json::from_slice(&fs::read(self.sessions_dir.join(format!("{id}.json")))?)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "malformed session descriptor")
                })?;
        descriptor.validate()?;
        if descriptor.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed session descriptor",
            ));
        }
        Ok(descriptor)
    }

    /// Every recorded session's node pid and name, read and nothing more.
    ///
    /// Deliberately not [`Self::list_live`], which probes every socket and
    /// deletes the records that do not answer. That is right for a person
    /// asking which sessions exist and wrong for anything on a redraw path:
    /// the probe blocks, and under load a busy node misses its deadline — see
    /// the tests around [`PROBE_ACK_TIMEOUT`]. A caller that already holds a
    /// process snapshot has a better liveness test than a socket probe anyway,
    /// and a name for a session that has since exited costs nothing.
    ///
    /// Unreadable and malformed records are skipped rather than swept, because
    /// having no name for a session is a missing label and deleting its record
    /// would strand it.
    pub fn names_by_node_pid(&self) -> io::Result<HashMap<u32, String>> {
        let entries = match fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(error),
        };
        let mut names = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if let Ok(descriptor) = self.read(id) {
                names.insert(descriptor.node_pid, descriptor.name);
            }
        }
        Ok(names)
    }

    pub fn remove(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.sessions_dir.join(format!("{id}.json"))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Returns the sessions that still have a node. A record is removed only when the socket
    /// refuses a connection *and* the process that wrote the record is gone, avoiding accidental
    /// deletion of a node that is merely starting -- or merely busy.
    ///
    /// The second half of that test is not belt and braces. A unix socket refuses connections
    /// once its listener's backlog is full, exactly as it does when the listener has gone: on
    /// macOS the 129th connection to a live, bound socket is answered `ECONNREFUSED`. A node
    /// that is not accepting for a moment -- a long drain, a machine under a build -- therefore
    /// looked dead to every `p2pmux ls`, `attach`, `ticket`, `kill` and `--resume` on the
    /// machine, each of which would then delete the only record of a running session and unlink
    /// the socket it was still listening on. The operating system knows the difference.
    ///
    /// A node that accepts the connection but does not answer within [`PROBE_ACK_TIMEOUT`] is
    /// still listed and, above all, still kept: it is alive and loaded, and deleting its record
    /// would strand a running session with no way to `attach` or `kill` it by name. Load spikes
    /// long enough to blow that deadline are ordinary on a machine building software, which is
    /// exactly when a shared session is most likely to be open.
    pub fn list_live(&self) -> io::Result<Vec<SessionDescriptor>> {
        let entries = match fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut sessions = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let id = match path.file_stem().and_then(|value| value.to_str()) {
                Some(value) => value,
                None => continue,
            };
            match self.read(id) {
                Ok(descriptor) => match probe(&descriptor.socket_path) {
                    Liveness::Acked | Liveness::Listening => sessions.push(descriptor),
                    // Refused, which the process table gets the last word on.
                    // Still listed when the node is alive: the session exists,
                    // and the next connection -- once the backlog drains -- will
                    // reach it.
                    Liveness::Gone if (self.node_alive)(descriptor.node_pid) => {
                        sessions.push(descriptor)
                    }
                    Liveness::Gone => {
                        let _ = fs::remove_file(&descriptor.socket_path);
                        let _ = fs::remove_file(path);
                    }
                },
                Err(_) => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        let held = sessions
            .iter()
            .map(|session| session.socket_path.clone())
            .collect::<HashSet<_>>();
        self.sweep_dead_sockets(&held);
        sessions.sort_by_key(|session| session.created_at);
        Ok(sessions)
    }

    /// Unlinks socket files whose node is gone.
    ///
    /// A node only removes its own socket on the way out of `run_background`, so anything
    /// that dies without unwinding — a crash, a SIGKILL, a reboot leaving /tmp behind —
    /// leaks one, and they accumulate indefinitely.
    ///
    /// Failure to connect is the test, not absence of a descriptor: `run_background` binds
    /// the listener before it writes the record, so a session that is still starting up has
    /// a live socket and no record yet, and must not be swept.
    ///
    /// `held` names the sockets [`Self::list_live`] just accounted for, whose nodes are known
    /// to be running. They are skipped rather than re-tested, because the connection this would
    /// open is refused by a node whose backlog is full -- and unlinking a live session's socket
    /// leaves it running with no way in.
    ///
    /// A socket nobody claims is still given a second chance a moment later, and then asked
    /// about of the process table. The directory is shared by every p2pmux this user runs,
    /// whatever `HOME` each was started with -- so a store in a sandbox `HOME`, which is how
    /// every test harness and probe script isolates itself, finds sockets it has no record of.
    /// Some of them belong to the sessions somebody is working in. The last word therefore
    /// belongs to the nodes themselves, which name the socket they own on their command line.
    fn sweep_dead_sockets(&self, held: &HashSet<PathBuf>) {
        let Ok(entries) = fs::read_dir(&self.socket_dir) else {
            return;
        };
        // Only paid for by a sweep that has found something to delete, which is rare: one
        // pass of the process table against a directory that is almost always all live.
        let mut owned_elsewhere: Option<HashSet<PathBuf>> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("sock") {
                continue;
            }
            if held.contains(&path) {
                continue;
            }
            if UnixStream::connect(&path).is_ok() {
                continue;
            }
            std::thread::sleep(SWEEP_RETRY_DELAY);
            if UnixStream::connect(&path).is_ok() {
                continue;
            }
            let owned = owned_elsewhere.get_or_insert_with(crate::agent_detect::node_socket_paths);
            if !owned.contains(&path) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    pub fn rename(&self, old: &str, new: &str) -> io::Result<SessionDescriptor> {
        if !valid_name(new) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid session name",
            ));
        }
        let mut descriptor = self.read(old)?;
        descriptor.name = new.to_owned();
        self.write(&descriptor)?;
        Ok(descriptor)
    }

    fn ensure_dirs(&self) -> io::Result<()> {
        for directory in [&self.sessions_dir, &self.socket_dir] {
            fs::create_dir_all(directory)?;
            fs::set_permissions(
                directory,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )?;
        }
        Ok(())
    }
}

pub fn generate_id() -> io::Result<String> {
    random_hex(16)
}

pub fn generate_name() -> io::Result<String> {
    generate_name_from_store(&SessionStore::for_current_user()?)
}

fn generate_name_from_store(store: &SessionStore) -> io::Result<String> {
    let live_names = store
        .list_live()?
        .into_iter()
        .map(|session| session.name)
        .collect();
    let mut bytes = [0u8; 8];
    fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    let index = u64::from_le_bytes(bytes) as usize % CITY_NAMES.len();
    Ok(available_city_name(&live_names, index))
}

fn available_city_name(live_names: &HashSet<String>, start: usize) -> String {
    for offset in 0..CITY_NAMES.len() {
        let city = CITY_NAMES[(start + offset) % CITY_NAMES.len()];
        if !live_names.contains(city) {
            return city.to_owned();
        }
    }

    let city = CITY_NAMES[start % CITY_NAMES.len()];
    for suffix in 2.. {
        let name = format!("{city}-{suffix}");
        if !live_names.contains(&name) {
            return name;
        }
    }
    unreachable!("an unbounded suffix range always yields a name")
}

/// Returns a locally available version of `preferred`, or `None` when the caller should use a
/// generated fallback name instead.
pub fn unique_local_name(preferred: &str, live_names: &HashSet<String>) -> Option<String> {
    if !valid_name(preferred) {
        return None;
    }
    if !live_names.contains(preferred) {
        return Some(preferred.to_owned());
    }

    for suffix in 2.. {
        let candidate = format!("{preferred}-{suffix}");
        if !valid_name(&candidate) {
            return None;
        }
        if !live_names.contains(&candidate) {
            return Some(candidate);
        }
    }
    unreachable!("a valid suffix is found before the name length limit")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn random_hex(length: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; length];
    fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
/// What a liveness probe learned about a session socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Liveness {
    /// The node answered the probe. Live, beyond doubt.
    Acked,
    /// The connection was accepted but no ack arrived in time. Something is holding this
    /// socket open -- it is a busy node, not a dead one.
    Listening,
    /// The socket is missing, or connecting was refused.
    ///
    /// Refused is *not* the same as gone, whatever this name suggests: a listener whose
    /// backlog is full refuses connections too. Callers settle it with the process table
    /// rather than treating this as a verdict -- see [`SessionStore::list_live`].
    Gone,
}

/// How long a node gets to answer before it is merely `Listening` rather than `Acked`.
///
/// Short on purpose: `p2pmux ls` waits this out once per session, and missing it is no
/// longer expensive now that only [`Liveness::Gone`] deletes anything.
const PROBE_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// How long an unclaimed socket is left alone before a second connection decides it.
///
/// Only ever paid for a socket about to be deleted, and only for one that no live record
/// claims. It exists for the node that has bound its listener and not yet written its
/// record: that gap is microseconds, and this is comfortably longer than it.
const SWEEP_RETRY_DELAY: Duration = Duration::from_millis(50);

fn probe(path: &Path) -> Liveness {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return Liveness::Gone;
    };
    if stream.write_all(b"{\"type\":\"probe\"}\n").is_err()
        || stream.set_read_timeout(Some(PROBE_ACK_TIMEOUT)).is_err()
    {
        return Liveness::Listening;
    }
    // Read one newline-delimited ack. Do not use read_to_string: the node keeps the
    // connection open, so waiting for EOF would never see one.
    let mut reader = io::BufReader::new(stream);
    let mut response = String::new();
    if matches!(reader.read_line(&mut response), Ok(n) if n > 0)
        && response.contains("\"probe_ack\"")
    {
        Liveness::Acked
    } else {
        Liveness::Listening
    }
}

/// Where the durable session descriptors live.
///
/// Each platform is asked in its own idiom rather than in a shared one: macOS
/// applications keep state under `Application Support`, and Linux ones follow
/// the XDG base directory spec, where `$XDG_STATE_HOME` wins when it is set to
/// an absolute path and `~/.local/state` is the specified fallback.
#[cfg(target_os = "macos")]
fn sessions_dir(home: &Path, _state_home: Option<std::ffi::OsString>) -> PathBuf {
    home.join("Library/Application Support/p2pmux/sessions")
}

#[cfg(not(target_os = "macos"))]
fn sessions_dir(home: &Path, state_home: Option<std::ffi::OsString>) -> PathBuf {
    state_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"))
        .join("p2pmux/sessions")
}

/// Where the per-session control sockets live.
///
/// `$XDG_RUNTIME_DIR` is the better home on Linux than the `/tmp` directory
/// macOS uses: it already belongs to one user, is already `0700`, and is
/// already emptied at logout. `/tmp` is none of those on a shared machine —
/// another user can hold `/tmp/p2pmux-{uid}` before we get there — so it stays
/// the fallback for the sessions that have no runtime directory at all.
#[cfg(target_os = "macos")]
fn socket_dir(_runtime_dir: Option<std::ffi::OsString>, uid: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!("p2pmux-{uid}"))
}

#[cfg(not(target_os = "macos"))]
fn socket_dir(runtime_dir: Option<std::ffi::OsString>, uid: &str) -> PathBuf {
    runtime_dir
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("p2pmux"))
        .unwrap_or_else(|| PathBuf::from("/tmp").join(format!("p2pmux-{uid}")))
}

fn current_uid() -> io::Result<String> {
    let output = std::process::Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(io::Error::other("could not determine current uid"));
    }
    let uid = String::from_utf8(output.stdout).map_err(|_| io::Error::other("invalid uid"))?;
    let uid = uid.trim();
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::other("invalid uid"));
    }
    Ok(uid.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn store() -> SessionStore {
        let root = std::env::temp_dir().join(format!(
            "p2pmux-session-store-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        SessionStore::at(root.join("sessions"), root.join("sockets"))
    }

    #[test]
    fn the_sessions_directory_follows_this_platform() {
        let home = Path::new("/home/example");
        let state_home = Some(std::ffi::OsString::from("/state"));

        #[cfg(target_os = "macos")]
        {
            // XDG variables are not a macOS convention, so one that happens to be
            // set does not move a Mac's session records out from under it.
            assert_eq!(
                sessions_dir(home, state_home),
                home.join("Library/Application Support/p2pmux/sessions")
            );
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(
                sessions_dir(home, state_home),
                PathBuf::from("/state/p2pmux/sessions")
            );
            assert_eq!(
                sessions_dir(home, None),
                home.join(".local/state/p2pmux/sessions")
            );
            // The spec says a relative XDG path is invalid and must be ignored.
            assert_eq!(
                sessions_dir(home, Some(std::ffi::OsString::from("state"))),
                home.join(".local/state/p2pmux/sessions")
            );
        }
    }

    #[test]
    fn the_socket_directory_follows_this_platform() {
        let runtime_dir = Some(std::ffi::OsString::from("/run/user/501"));

        #[cfg(target_os = "macos")]
        assert_eq!(
            socket_dir(runtime_dir, "501"),
            PathBuf::from("/tmp/p2pmux-501")
        );

        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(
                socket_dir(runtime_dir, "501"),
                PathBuf::from("/run/user/501/p2pmux")
            );
            // No runtime directory at all — a cron job, a container, a bare
            // `ssh` command — still has to land somewhere writable.
            assert_eq!(socket_dir(None, "501"), PathBuf::from("/tmp/p2pmux-501"));
        }
    }

    #[test]
    fn writes_private_atomic_descriptor_and_validates_names() {
        let store = store();
        let id = generate_id().unwrap();
        let socket = store.socket_path(&id).unwrap();
        let descriptor = SessionDescriptor::new(
            id.clone(),
            "lisbon".into(),
            socket,
            42,
            SessionRole::Coordinator,
        );
        store.write(&descriptor).unwrap();
        assert_eq!(store.read(&id).unwrap(), descriptor);
        assert_eq!(
            fs::metadata(store.sessions_dir.join(format!("{id}.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!valid_name("Nope") && !valid_name("-nope") && valid_name("good-name-2"));
    }

    #[test]
    fn stale_records_are_removed_only_after_probe_fails() {
        let store = store();
        let id = generate_id().unwrap();
        let socket = store.socket_path(&id).unwrap();
        store
            .write(&SessionDescriptor::new(
                id.clone(),
                "lisbon".into(),
                socket,
                42,
                SessionRole::Coordinator,
            ))
            .unwrap();
        assert!(store.list_live().unwrap().is_empty());
        assert!(store.read(&id).is_err());
    }

    #[test]
    fn dead_sockets_are_swept_but_a_bound_one_without_a_record_survives() {
        use std::os::unix::net::UnixListener;

        let root = PathBuf::from(format!("/tmp/p2pmux-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = SessionStore::at(root.join("s"), root.join("k"));

        // Left behind by a node that died without unwinding: a plain file, nothing bound.
        let abandoned = store.socket_path(&generate_id().unwrap()).unwrap();
        fs::write(&abandoned, b"").unwrap();
        // A node that has bound its listener but has not written its descriptor yet.
        let starting = store.socket_path(&generate_id().unwrap()).unwrap();
        let _listener = UnixListener::bind(&starting).unwrap();
        // Not ours; left alone whatever its state.
        let unrelated = store.socket_dir.join("notes.txt");
        fs::write(&unrelated, b"").unwrap();

        assert!(store.list_live().unwrap().is_empty());

        assert!(!abandoned.exists(), "dead socket should have been swept");
        assert!(starting.exists(), "a bound socket must survive the sweep");
        assert!(
            unrelated.exists(),
            "non-socket files are not ours to delete"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The bug this file's `list_live` doc describes, in one test: a node that
    /// is alive but not accepting -- a full backlog answers `ECONNREFUSED`
    /// exactly as a dead listener does -- must keep both its record and its
    /// socket, or every `p2pmux ls` on the machine strands a running session.
    #[test]
    fn a_node_that_is_running_survives_a_refused_connection() {
        let root = PathBuf::from(format!("/tmp/p2pmux-alive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut store = SessionStore::at(root.join("s"), root.join("k"));
        store.node_alive = |_| true;

        let id = generate_id().unwrap();
        let socket = store.socket_path(&id).unwrap();
        store
            .write(&SessionDescriptor::new(
                id.clone(),
                "helsinki".into(),
                socket.clone(),
                4321,
                SessionRole::Coordinator,
            ))
            .unwrap();
        // Nothing is listening on it, which is what a refused connection looks
        // like from the outside whether the node is gone or merely saturated.
        fs::write(&socket, b"").unwrap();

        let live = store.list_live().unwrap();

        assert_eq!(
            live.iter()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            vec!["helsinki"],
            "a running node's session must still be listed"
        );
        assert!(store.read(&id).is_ok(), "its record must survive");
        assert!(socket.exists(), "its socket must survive the sweep");
        let _ = fs::remove_dir_all(&root);
    }

    /// The other half: once the node really is gone, the record goes with it.
    #[test]
    fn a_session_whose_node_is_gone_is_still_forgotten() {
        let root = PathBuf::from(format!("/tmp/p2pmux-gone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut store = SessionStore::at(root.join("s"), root.join("k"));
        store.node_alive = |_| false;

        let id = generate_id().unwrap();
        let socket = store.socket_path(&id).unwrap();
        store
            .write(&SessionDescriptor::new(
                id.clone(),
                "helsinki".into(),
                socket.clone(),
                4321,
                SessionRole::Coordinator,
            ))
            .unwrap();
        fs::write(&socket, b"").unwrap();

        assert!(store.list_live().unwrap().is_empty());
        assert!(store.read(&id).is_err(), "the record should be gone");
        assert!(!socket.exists(), "the socket should have been swept");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn live_probe_reads_one_ack_without_waiting_for_eof() {
        use std::os::unix::net::UnixListener;
        use std::thread;

        let root = PathBuf::from(format!("/tmp/p2pmux-t-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = SessionStore::at(root.join("s"), root.join("k"));
        let id = generate_id().unwrap();
        // Keep the path short enough for sockaddr_un on macOS.
        let socket = root.join("k").join(format!("{}.s", &id[..8]));
        fs::create_dir_all(root.join("k")).unwrap();
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("probe"));
            stream.write_all(b"{\"type\":\"probe_ack\"}\n").unwrap();
            // Keep the connection open briefly; a correct probe must not require EOF.
            thread::sleep(Duration::from_millis(50));
        });
        store
            .write(&SessionDescriptor::new(
                id.clone(),
                "lisbon".into(),
                socket.clone(),
                42,
                SessionRole::Coordinator,
            ))
            .unwrap();
        let live = store.list_live().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, id);
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_node_too_busy_to_answer_keeps_its_record() {
        use std::os::unix::net::UnixListener;

        // The node is bound but never gets around to accepting -- a machine under enough
        // load to starve its accept loop past the ack deadline. Deleting the record here
        // is how a live session used to lose its name: `attach`, `kill` and `ls` all go
        // through the finder, and the node itself keeps running with no way back to it.
        // CI reproduced this on its own, at random, as a red `tests/cli.rs`.
        let root = PathBuf::from(format!("/tmp/p2pmux-busy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = SessionStore::at(root.join("s"), root.join("k"));
        let id = generate_id().unwrap();
        fs::create_dir_all(root.join("k")).unwrap();
        let socket = root.join("k").join(format!("{}.s", &id[..8]));
        let listener = UnixListener::bind(&socket).unwrap();
        store
            .write(&SessionDescriptor::new(
                id.clone(),
                "lisbon".into(),
                socket.clone(),
                42,
                SessionRole::Coordinator,
            ))
            .unwrap();

        assert_eq!(probe(&socket), Liveness::Listening);
        let live = store.list_live().unwrap();

        assert_eq!(live.len(), 1, "a busy node is still a live one");
        assert_eq!(live[0].id, id);
        assert!(store.read(&id).is_ok(), "its record must survive");
        assert!(socket.exists(), "and so must its socket");

        // Once the listener is gone the connection is refused, and only then is it swept.
        drop(listener);
        assert_eq!(probe(&socket), Liveness::Gone);
        assert!(store.list_live().unwrap().is_empty());
        assert!(store.read(&id).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn generated_names_are_valid_world_cities() {
        let name = generate_name_from_store(&store()).unwrap();

        assert!((80..=150).contains(&CITY_NAMES.len()));
        assert!(CITY_NAMES.iter().all(|city| valid_name(city)));
        assert!(CITY_NAMES.contains(&name.as_str()));
    }

    #[test]
    fn city_name_skips_live_names() {
        let live_names = HashSet::from([CITY_NAMES[0].to_owned()]);

        assert_eq!(available_city_name(&live_names, 0), CITY_NAMES[1]);
    }

    #[test]
    fn city_name_adds_suffix_after_all_cities_are_live() {
        let mut live_names: HashSet<_> = CITY_NAMES.iter().map(|city| (*city).to_owned()).collect();

        assert_eq!(
            available_city_name(&live_names, 0),
            format!("{}-2", CITY_NAMES[0])
        );
        live_names.insert(format!("{}-2", CITY_NAMES[0]));
        assert_eq!(
            available_city_name(&live_names, 0),
            format!("{}-3", CITY_NAMES[0])
        );
    }

    #[test]
    fn unique_local_name_uses_preferred_or_available_suffix() {
        let mut live_names = HashSet::from(["lisbon".to_owned(), "lisbon-2".to_owned()]);

        assert_eq!(
            unique_local_name("lisbon", &live_names),
            Some("lisbon-3".to_owned())
        );
        assert_eq!(
            unique_local_name("tokyo", &live_names),
            Some("tokyo".to_owned())
        );
        live_names.insert("lisbon-3".to_owned());
        assert_eq!(
            unique_local_name("lisbon", &live_names),
            Some("lisbon-4".to_owned())
        );
    }

    #[test]
    fn unique_local_name_requires_a_valid_preferred_and_suffix() {
        assert_eq!(unique_local_name("", &HashSet::new()), None);
        assert_eq!(
            unique_local_name(&"a".repeat(48), &HashSet::from(["a".repeat(48)])),
            None
        );
    }
}
