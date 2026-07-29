//! Local, short-code rendezvous records for same-Mac dogfooding.

use std::{
    env, fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::ticket::{JoinTicket, TicketError};

pub const SHORT_CODE_LEN: usize = 10;
const SHORT_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct LocalRendezvous {
    directory: PathBuf,
}

#[derive(Debug)]
pub struct RendezvousEntry {
    code: String,
    path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RendezvousError {
    InvalidCode,
    NotFound,
    InvalidRecord,
    CacheDirectoryUnavailable,
    Random,
    Io,
}

impl fmt::Display for RendezvousError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidCode => "invalid local join code",
            Self::NotFound => "join code was not found on this Mac",
            Self::InvalidRecord => "local join code is invalid",
            Self::CacheDirectoryUnavailable | Self::Io => "local rendezvous store is unavailable",
            Self::Random => "could not create a local join code",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RendezvousError {}

#[derive(Deserialize, Serialize)]
struct RendezvousRecord {
    session_id: Vec<u8>,
    endpoint_addr: EndpointAddr,
}

impl LocalRendezvous {
    pub fn for_current_user() -> Result<Self, RendezvousError> {
        let cache_home = env::var_os("XDG_CACHE_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|path| !path.is_empty())
                    .map(|home| PathBuf::from(home).join(".cache"))
            })
            .ok_or(RendezvousError::CacheDirectoryUnavailable)?;
        Ok(Self::at(cache_home.join("p2pmux").join("rendezvous")))
    }

    pub fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn publish(&self, ticket: &JoinTicket) -> Result<RendezvousEntry, RendezvousError> {
        self.ensure_directory()?;
        let record = serde_json::to_vec(&RendezvousRecord {
            session_id: ticket.session_id().to_vec(),
            endpoint_addr: ticket.endpoint_addr().clone(),
        })
        .map_err(|_| RendezvousError::InvalidRecord)?;

        for _ in 0..16 {
            let code = mint_code()?;
            let path = self.path_for(&code);
            match create_restricted_file(&path) {
                Ok(mut file) => {
                    file.write_all(&record).map_err(|_| RendezvousError::Io)?;
                    file.sync_all().map_err(|_| RendezvousError::Io)?;
                    return Ok(RendezvousEntry { code, path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(RendezvousError::Io),
            }
        }
        Err(RendezvousError::Random)
    }

    pub fn resolve(&self, code: &str) -> Result<JoinTicket, RendezvousError> {
        validate_code(code)?;
        let bytes = match fs::read(self.path_for(code)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RendezvousError::NotFound);
            }
            Err(_) => return Err(RendezvousError::Io),
        };
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(RendezvousError::InvalidRecord);
        }
        let record: RendezvousRecord =
            serde_json::from_slice(&bytes).map_err(|_| RendezvousError::InvalidRecord)?;
        JoinTicket::from_parts(record.session_id, record.endpoint_addr)
            .map_err(ticket_error_to_rendezvous_error)
    }

    /// The join codes published on this Mac, sorted.
    ///
    /// A missing directory means no session has been created here yet, which is an ordinary
    /// state rather than a failure. Names that are not codes are ignored so an unrelated file
    /// dropped in the cache cannot make the whole listing unusable.
    pub fn codes(&self) -> Result<Vec<String>, RendezvousError> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(RendezvousError::Io),
        };
        let mut codes: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| validate_code(name).is_ok())
            .collect();
        codes.sort();
        Ok(codes)
    }

    fn ensure_directory(&self) -> Result<(), RendezvousError> {
        fs::create_dir_all(&self.directory).map_err(|_| RendezvousError::Io)?;
        restrict_directory(&self.directory).map_err(|_| RendezvousError::Io)
    }

    fn path_for(&self, code: &str) -> PathBuf {
        self.directory.join(code)
    }
}

impl RendezvousEntry {
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remove(&self) -> Result<(), RendezvousError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(RendezvousError::Io),
        }
    }
}

impl Drop for RendezvousEntry {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn mint_code() -> Result<String, RendezvousError> {
    let mut bytes = [0_u8; SHORT_CODE_LEN];
    getrandom::fill(&mut bytes).map_err(|_| RendezvousError::Random)?;
    Ok(bytes
        .into_iter()
        .map(|byte| char::from(SHORT_CODE_ALPHABET[usize::from(byte) % SHORT_CODE_ALPHABET.len()]))
        .collect())
}

fn validate_code(code: &str) -> Result<(), RendezvousError> {
    if code.len() == SHORT_CODE_LEN && code.bytes().all(|byte| SHORT_CODE_ALPHABET.contains(&byte))
    {
        Ok(())
    } else {
        Err(RendezvousError::InvalidCode)
    }
}

fn ticket_error_to_rendezvous_error(_error: TicketError) -> RendezvousError {
    RendezvousError::InvalidRecord
}

#[cfg(unix)]
fn create_restricted_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_restricted_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
