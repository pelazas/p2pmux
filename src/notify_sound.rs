use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

/// The sound an agent's completion plays when the config names none.
///
/// macOS ships one with the system. On Linux this is the freedesktop sound
/// theme's completion chime, which desktop installs have and servers do not —
/// see [`play_sound`] for what happens when it is absent.
#[cfg(target_os = "macos")]
pub const DEFAULT_SOUND_PATH: &str = "/System/Library/Sounds/Tink.aiff";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_SOUND_PATH: &str = "/usr/share/sounds/freedesktop/stereo/complete.oga";

/// Command-line players to try in order, and the arguments each needs before
/// the file name.
///
/// macOS has one and it is always installed. Linux has a stack of sound servers
/// and a machine answers to whichever it happens to be running, so the list is
/// tried top to bottom: PipeWire, then PulseAudio, then libcanberra, then raw
/// ALSA. `aplay` is last because it only understands WAV.
#[cfg(target_os = "macos")]
const PLAYERS: &[(&str, &[&str])] = &[("afplay", &[])];
#[cfg(not(target_os = "macos"))]
const PLAYERS: &[(&str, &[&str])] = &[
    ("pw-play", &[]),
    ("paplay", &[]),
    ("canberra-gtk-play", &["-f"]),
    ("aplay", &["-q"]),
];

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

/// Play `path` through the first player on this machine that will take it.
///
/// A headless Linux box has no sound theme and often no sound server either, so
/// every player can legitimately be missing. Falling back to the terminal bell
/// keeps the notification audible there: the terminal emulator on the human's
/// own machine is the one thing guaranteed to be present.
fn play_sound(path: &Path, reported_failures: &Mutex<BTreeSet<PathBuf>>) {
    let mut last_failure = None;
    for (program, arguments) in PLAYERS {
        let status = Command::new(program)
            .args(*arguments)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => return,
            Ok(status) => last_failure = Some(format!("{program} exited with {status}")),
            Err(error) => last_failure = Some(format!("could not run {program}: {error}")),
        }
    }
    ring_terminal_bell();
    report_failure(
        path,
        reported_failures,
        &last_failure.unwrap_or_else(|| String::from("no sound player is installed")),
    );
}

/// Ask the terminal to make the noise instead.
///
/// Written to stderr, not stdout: the TUI renders through stdout and a stray
/// byte in that stream lands in ratatui's picture of the screen.
fn ring_terminal_bell() {
    use std::io::Write as _;

    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(b"\x07");
    let _ = stderr.flush();
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
