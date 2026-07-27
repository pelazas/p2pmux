//! Optional performance logging shared by the client and background node.

use std::{ffi::OsString, fs::OpenOptions, io::Write, path::PathBuf};

const LOG_NAME: &str = "p2pmux-perf.log";

pub(crate) fn enabled() -> bool {
    std::env::var_os("P2PMUX_PERF").is_some_and(|value| value == "1")
}

pub(crate) fn log(message: &str) {
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    else {
        return;
    };
    let _ = file.write_all(format!("{message}\n").as_bytes());
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
}
