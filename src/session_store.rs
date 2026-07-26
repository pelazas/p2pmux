//! Small, durable finder records for locally running sessions.
//!
//! The record intentionally contains only enough information to find a live node.  Session
//! state (PTYs, tickets, screens, and focus) belongs to the node process, never to disk.

use std::{
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
                Ok(_) | Err(_) => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        sessions.sort_by_key(|session| session.created_at);
        Ok(sessions)
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
    let mut bytes = [0u8; 8];
    fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    let index = u64::from_le_bytes(bytes) as usize % CITY_NAMES.len();
    Ok(CITY_NAMES[index].to_owned())
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
            "amber-otter-01".into(),
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
                "amber-otter-01".into(),
                socket,
                42,
                SessionRole::Coordinator,
            ))
            .unwrap();
        assert!(store.list_live().unwrap().is_empty());
        assert!(store.read(&id).is_err());
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
                "amber-otter-01".into(),
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
}
