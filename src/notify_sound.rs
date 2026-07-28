use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

pub const DEFAULT_SOUND_PATH: &str = "/System/Library/Sounds/Tink.aiff";

/// Plays local agent-completion sounds without blocking the terminal UI.
#[derive(Clone, Debug)]
pub struct NotificationSound {
    path: PathBuf,
    reported_failures: Arc<Mutex<BTreeSet<PathBuf>>>,
}

impl NotificationSound {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path: path.unwrap_or_else(|| PathBuf::from(DEFAULT_SOUND_PATH)),
            reported_failures: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn play(&self) {
        let path = self.path.clone();
        let reported_failures = Arc::clone(&self.reported_failures);
        if thread::Builder::new()
            .name(String::from("p2pmux-notify-sound"))
            .spawn(move || play_sound(&path, &reported_failures))
            .is_err()
        {
            report_failure(
                &self.path,
                &self.reported_failures,
                "could not start sound worker",
            );
        }
    }
}

fn play_sound(path: &Path, reported_failures: &Mutex<BTreeSet<PathBuf>>) {
    let status = Command::new("afplay")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => report_failure(
            path,
            reported_failures,
            &format!("afplay exited with {status}"),
        ),
        Err(error) => report_failure(
            path,
            reported_failures,
            &format!("could not run afplay: {error}"),
        ),
    }
}

fn report_failure(path: &Path, reported_failures: &Mutex<BTreeSet<PathBuf>>, message: &str) {
    let Ok(mut failures) = reported_failures.lock() else {
        return;
    };
    // The TUI owns the terminal (raw mode + alternate screen), so stderr would
    // scribble over it; route through the opt-in UI debug log instead.
    if failures.insert(path.to_owned()) {
        crate::tui::ui_debug_log(
            "notify_sound_failure",
            format_args!("path={} message={message}", path.display()),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{DEFAULT_SOUND_PATH, NotificationSound};

    #[test]
    fn uses_default_sound_path() {
        assert_eq!(
            NotificationSound::new(None).path(),
            PathBuf::from(DEFAULT_SOUND_PATH)
        );
    }

    #[test]
    fn uses_custom_sound_path() {
        assert_eq!(
            NotificationSound::new(Some(PathBuf::from("/tmp/custom.aiff"))).path(),
            PathBuf::from("/tmp/custom.aiff")
        );
    }
}
