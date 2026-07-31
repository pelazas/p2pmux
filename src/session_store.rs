//! Small, durable finder records for locally running sessions.
//!
//! The record intentionally contains only enough information to find a live node.  Session
//! state (PTYs, tickets, screens, and focus) belongs to the node process, never to disk.

use std::{
    collections::HashSet,
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
}

impl SessionStore {
    pub fn for_current_user() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let uid = current_uid()?;
        Ok(Self::at(
            PathBuf::from(home).join("Library/Application Support/p2pmux/sessions"),
            PathBuf::from("/tmp").join(format!("p2pmux-{uid}")),
        ))
    }

    /// Test-friendly constructor.  Directories are created lazily with private permissions.
    pub fn at(sessions_dir: PathBuf, socket_dir: PathBuf) -> Self {
        Self {
            sessions_dir,
            socket_dir,
        }
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

    pub fn remove(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.sessions_dir.join(format!("{id}.json"))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Returns only connectable descriptors. Failed probes are the sole condition under which a
    /// finder record is removed, avoiding accidental deletion of a node that is merely starting.
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
                Ok(descriptor) if probe(&descriptor.socket_path) => sessions.push(descriptor),
                Ok(descriptor) => {
                    let _ = fs::remove_file(&descriptor.socket_path);
                    let _ = fs::remove_file(path);
                }
                Err(_) => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        self.sweep_dead_sockets();
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
    fn sweep_dead_sockets(&self) {
        let Ok(entries) = fs::read_dir(&self.socket_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("sock") {
                continue;
            }
            if UnixStream::connect(&path).is_err() {
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
fn probe(path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    if stream.write_all(b"{\"type\":\"probe\"}\n").is_err()
        || stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .is_err()
    {
        return false;
    }
    // Read one newline-delimited ack. Do not use read_to_string: the node keeps the
    // connection open, so waiting for EOF falsely marks live sessions as dead and
    // list_live would delete their descriptors.
    let mut reader = io::BufReader::new(stream);
    let mut response = String::new();
    matches!(reader.read_line(&mut response), Ok(n) if n > 0) && response.contains("\"probe_ack\"")
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
