//! Optional performance logging shared by the client and background node.

use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const LOG_NAME: &str = "p2pmux-perf.log";

pub(crate) fn enabled() -> bool {
    std::env::var_os("P2PMUX_PERF").is_some_and(|value| value == "1")
}

pub(crate) fn log(message: &str) {
    let Ok(mut logger) = logger().lock() else {
        return;
    };
    logger.log(&log_path(), message);
}

#[derive(Default)]
struct PerfLogger {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl PerfLogger {
    fn log(&mut self, path: &Path, message: &str) {
        if self.path.as_deref() != Some(path) {
            self.path = None;
            self.file = None;
            let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
                return;
            };
            if file.is_terminal() {
                return;
            }
            self.path = Some(path.to_path_buf());
            self.file = Some(file);
        }
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{message}");
        }
    }
}

fn logger() -> &'static Mutex<PerfLogger> {
    static LOGGER: OnceLock<Mutex<PerfLogger>> = OnceLock::new();
    LOGGER.get_or_init(|| Mutex::new(PerfLogger::default()))
}

fn log_path() -> PathBuf {
    log_path_from(std::env::var_os("P2PMUX_PERF_LOG"))
}

fn log_path_from(path: Option<OsString>) -> PathBuf {
    path.map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(LOG_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_uses_override_or_temp_default() {
        assert_eq!(
            log_path_from(Some(OsString::from("/tmp/custom-perf.log"))),
            PathBuf::from("/tmp/custom-perf.log"),
        );
        assert_eq!(log_path_from(None), std::env::temp_dir().join(LOG_NAME),);
    }

    #[test]
    fn logger_reuses_the_open_file() {
        let directory = std::env::temp_dir().join(format!("p2pmux-perf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("perf.log");
        let renamed = directory.join("renamed.log");
        let mut logger = PerfLogger::default();

        logger.log(&path, "first");
        std::fs::rename(&path, &renamed).unwrap();
        logger.log(&path, "second");

        assert_eq!(
            std::fs::read_to_string(&renamed).unwrap(),
            "first\nsecond\n"
        );
        assert!(!path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
