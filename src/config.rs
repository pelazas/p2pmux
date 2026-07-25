use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum ConfigError {
    MissingHome,
    InvalidName(&'static str),
    Io(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => f.write_str("could not resolve config directory"),
            Self::InvalidName(message) => f.write_str(message),
            Self::Io(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
            Self::Serialize(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ConfigFile {
    display_name: Option<String>,
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or(ConfigError::MissingHome)?;
    Ok(base.join("p2pmux").join("config.toml"))
}

pub fn validate_display_name(name: &str) -> Result<String, ConfigError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ConfigError::InvalidName("display name must not be empty"));
    }
    if name.chars().count() > 32 {
        return Err(ConfigError::InvalidName(
            "display name must be at most 32 characters",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ConfigError::InvalidName(
            "display name must not contain control characters",
        ));
    }
    Ok(name.to_owned())
}

pub fn load_from(path: &Path) -> Result<Option<String>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let config: ConfigFile = toml::from_str(&text).map_err(ConfigError::Parse)?;
    config
        .display_name
        .map(|name| validate_display_name(&name))
        .transpose()
}

pub fn save_to(path: &Path, name: &str) -> Result<String, ConfigError> {
    let display_name = validate_display_name(name)?;
    let config = ConfigFile {
        display_name: Some(display_name.clone()),
    };
    let text = toml::to_string_pretty(&config).map_err(ConfigError::Serialize)?;
    let parent = path.parent().ok_or(ConfigError::MissingHome)?;
    fs::create_dir_all(parent)?;
    fs::write(path, text)?;
    Ok(display_name)
}

pub fn load() -> Result<Option<String>, ConfigError> {
    load_from(&config_path()?)
}

pub fn save(name: &str) -> Result<String, ConfigError> {
    save_to(&config_path()?, name)
}
