//! Which `p2pmux` on PATH actually runs, and whether it is the newest one installed.
//!
//! p2pmux installs four ways and none of them knows about the others. A curl
//! install writes `/usr/local/bin`, Homebrew writes `/opt/homebrew/bin`, cargo
//! writes `~/.cargo/bin`, and the shell runs whichever comes first — which is
//! not, in general, the one installed last. The failure that follows is silent
//! and reads as a product bug: a fix ships, the user updates through one
//! channel, the other channel keeps winning, and the fix appears not to work.
//!
//! So `doctor` answers the question directly: here is every copy on your PATH,
//! here is its version, here is the one that runs. The scan is `which -a`
//! semantics done in-process — walking PATH ourselves rather than shelling out,
//! since the whole subject of this module is that the shell's answer is the
//! thing under suspicion.
//!
//! Reading a version means running a binary someone else installed, and one of
//! them is old or broken often enough that it is the point. Every probe is
//! therefore allowed to fail — non-zero exit, unparsable output, no answer at
//! all — and an unknown version is reported as unknown rather than guessed.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// How long a copy gets to answer `--version`.
///
/// Doctor is a foreground command, so this is a bound on how long it can sit
/// there for a binary that never answers, not a budget anything real needs:
/// printing a version is the first thing a working p2pmux does.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Longest version string worth printing. Anything longer is not a version,
/// and doctor's output is not the place to find out what it is instead.
const MAX_VERSION_LEN: usize = 40;

/// One `p2pmux` install found on PATH.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Install {
    /// The path as PATH spells it, which is the path to tell the user about.
    pub path: PathBuf,
    /// What it said when asked, or `None` if it could not be asked.
    pub version: Option<String>,
}

/// Every `p2pmux` on `path_var`, in the order a shell would try them.
///
/// `probe` reads a copy's version; it is a parameter so the ordering and
/// warning logic can be tested without laying real binaries on disk.
pub fn installs_in(path_var: &OsStr, probe: impl Fn(&Path) -> Option<String>) -> Vec<Install> {
    let mut found: Vec<Install> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for directory in std::env::split_paths(path_var) {
        // An empty PATH entry means the working directory to a shell. Running a
        // p2pmux out of whatever directory the user happens to be in is not a
        // copy anyone installed, and naming it would be noise.
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join("p2pmux");
        if !is_executable_file(&candidate) {
            continue;
        }
        // The same binary reached twice — a PATH that repeats a directory, or
        // two directories symlinked to one — is one copy, and listing it twice
        // would invent a conflict that is not there. Identity is the resolved
        // path; the spelling shown stays the one PATH gave.
        let identity = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let version = probe(&candidate);
        found.push(Install {
            path: candidate,
            version,
        });
    }
    found
}

/// Every `p2pmux` on this process's PATH, versions read by running them.
pub fn installs_on_path() -> Vec<Install> {
    match std::env::var_os("PATH") {
        Some(path) => installs_in(&path, probe_version),
        None => Vec::new(),
    }
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Ask one copy its version, or `None` if it will not say.
fn probe_version(path: &Path) -> Option<String> {
    let mut child = Command::new(path)
        .arg("--version")
        // A probe that inherits stdin could be read from; one that inherits
        // stderr could print a stale build's argument-parsing error into the
        // middle of doctor's report.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            // Either it failed — a build old enough not to know `--version`
            // does exactly this — or it is not going to answer at all.
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let output = child.wait_with_output().ok()?;
    version_from_output(&String::from_utf8_lossy(&output.stdout))
}

/// The version in `p2pmux 0.1.13`, if that is what this looks like.
///
/// Deliberately narrow: the output being parsed comes from a binary this build
/// knows nothing about, and it is about to be printed to a terminal. A token
/// that starts with a digit and carries only version characters is accepted;
/// everything else is unknown, so no copy can put escape sequences into
/// doctor's report by claiming a name.
fn version_from_output(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    let token = line.strip_prefix("p2pmux ").unwrap_or(line).trim();
    let digits = token.strip_prefix('v').unwrap_or(token);
    let plausible = digits.starts_with(|c: char| c.is_ascii_digit())
        && token.len() <= MAX_VERSION_LEN
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'));
    plausible.then(|| token.to_owned())
}

/// The lines `doctor` prints about the copies on PATH.
///
/// Split from the scan so the interesting half — which copy wins, and whether
/// winning is the problem — is testable without a PATH full of real binaries.
pub fn report(installs: &[Install]) -> Vec<String> {
    let Some(winner) = installs.first() else {
        return vec![
            "warning: `p2pmux` is not on PATH — hooks invoke it by name and would not run."
                .to_owned(),
        ];
    };
    let width = installs
        .iter()
        .map(|install| install.path.display().to_string().chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec!["p2pmux on PATH".to_owned()];
    for (index, install) in installs.iter().enumerate() {
        let path = install.path.display().to_string();
        let version = install.version.as_deref().unwrap_or("unknown");
        let mark = if index == 0 {
            "  <- runs as `p2pmux`"
        } else {
            ""
        };
        lines.push(
            format!("  {path:width$}  {version:<8}{mark}")
                .trim_end()
                .to_owned(),
        );
    }
    if let Some(better) = newer_than_the_winner(installs) {
        lines.push(String::new());
        lines.extend(shadow_warning(winner, better));
    }
    lines
}

/// The copy that ought to be running instead, if there is one.
fn newer_than_the_winner(installs: &[Install]) -> Option<&Install> {
    let winner = installs.first()?;
    installs
        .iter()
        .skip(1)
        .find(|install| match &install.version {
            // A copy whose version cannot be read is never grounds for a warning:
            // unknown is unknown, and nagging on it would fire on every machine
            // holding some unrelated program named p2pmux.
            None => false,
            Some(version) => match &winner.version {
                Some(running) => crate::update_check::is_newer(running, version),
                // The one that runs cannot say what it is while another copy can.
                // That is not a version comparison, it is a broken binary winning.
                None => true,
            },
        })
}

fn shadow_warning(winner: &Install, better: &Install) -> Vec<String> {
    let fix = update_command(&winner.path);
    let newer = better.version.as_deref().unwrap_or("another copy");
    // Reordering PATH is the other half of the fix, and only worth saying if
    // there is a directory to name — `better` always has a parent in practice,
    // since it was found by joining one.
    let reorder = match better.path.parent() {
        Some(directory) => format!(", or put {} earlier on your PATH", directory.display()),
        None => String::new(),
    };
    let body = match &winner.version {
        Some(running) => format!(
            "warning: `p2pmux` runs {running} from {}, but {newer} is installed at {}. \
             Everything fixed since {running} looks unshipped from here. Update the copy \
             that runs with `{fix}`{reorder}.",
            winner.path.display(),
            better.path.display(),
        ),
        None => format!(
            "warning: `p2pmux` runs {}, which did not report a version, while {newer} \
             is installed at {}. Update or remove the copy that runs \
             (`{fix}`){reorder}.",
            winner.path.display(),
            better.path.display(),
        ),
    };
    wrapped(&body, "         ")
}

/// The command that replaces the copy at `path` — as opposed to one that
/// installs p2pmux somewhere and leaves that copy exactly where it is.
///
/// Homebrew and cargo update a copy wherever they put it, so their commands
/// stand. The installer does not: it writes `/usr/local/bin`, so telling
/// someone whose winning copy is in `/usr/local/sbin` to pipe it into `sh`
/// rewrites a file that was already newer and changes nothing about the one
/// that runs. Pointing it at the directory that actually wins is the whole
/// difference between advice and a command that does not work.
fn update_command(path: &Path) -> String {
    /// Where the installer writes when nothing tells it otherwise.
    const INSTALLER_DEFAULT: &str = "/usr/local/bin";
    let command = crate::update_check::upgrade_command(path);
    // Compared rather than pattern-matched: this is the answer that module
    // gives for a path no packaging channel owns, whatever it says today.
    let unowned = command == crate::update_check::upgrade_command(Path::new("/"));
    let directory = path.parent().unwrap_or(Path::new(INSTALLER_DEFAULT));
    if !unowned || directory == Path::new(INSTALLER_DEFAULT) {
        return command.to_owned();
    }
    // The variable has to reach `sh`, not `curl`, so it goes on the right of
    // the pipe. If the command ever stops having one, the plain form is still
    // better than a mangled one.
    let piped = command.replace(
        "| sh",
        &format!("| P2PMUX_INSTALL_DIR={} sh", directory.display()),
    );
    if piped == command {
        command.to_owned()
    } else {
        piped
    }
}

/// `body` broken across terminal-width lines, every line after the first
/// starting with `indent`.
///
/// A path is the one thing here with no length bound, so the alternative to
/// wrapping is a warning whose most important sentence has already scrolled off
/// the right edge of the terminal by the time it names the version to update.
fn wrapped(body: &str, indent: &str) -> Vec<String> {
    const WIDTH: usize = 78;
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in atoms(body) {
        let start_of_line = line.is_empty() || line == indent;
        if !start_of_line && line.chars().count() + 1 + word.chars().count() > WIDTH {
            lines.push(std::mem::take(&mut line));
            line.push_str(indent);
        } else if !start_of_line {
            line.push(' ');
        }
        // A word longer than the width — which is to say a path — overflows
        // rather than being broken, since half a path is not a path.
        line.push_str(&word);
    }
    if !line.trim().is_empty() {
        lines.push(line);
    }
    lines
}

/// `body` split into the pieces a line break may fall between.
///
/// Words, except that a `backticked` span holds together however long it is:
/// the warning's whole purpose is to hand over a command to run, and one broken
/// across a line break is one that does not survive being copied.
fn atoms(body: &str) -> Vec<String> {
    let mut atoms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in body.chars() {
        if character == '`' {
            quoted = !quoted;
        }
        if character.is_whitespace() && !quoted {
            if !current.is_empty() {
                atoms.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(character);
    }
    if !current.is_empty() {
        atoms.push(current);
    }
    atoms
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn install(path: &str, version: Option<&str>) -> Install {
        Install {
            path: PathBuf::from(path),
            version: version.map(str::to_owned),
        }
    }

    fn joined(text: &[String]) -> String {
        text.join("\n")
    }

    /// The warning is wrapped to a terminal, so a sentence in it is only
    /// findable once the line breaks are read back as the spaces they replaced.
    fn unwrapped(text: &[String]) -> String {
        text.join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The bug this exists for: a Homebrew copy two releases behind wins the
    /// PATH, so every fix since looks unshipped. Naming the newer one and where
    /// it is, is the whole fix.
    #[test]
    fn an_older_copy_that_wins_the_path_is_named_along_with_the_newer_one() {
        let lines = report(&[
            install("/opt/homebrew/bin/p2pmux", Some("0.1.11")),
            install("/usr/local/bin/p2pmux", Some("0.1.13")),
        ]);
        let text = unwrapped(&lines);
        assert!(
            joined(&lines).contains("/opt/homebrew/bin/p2pmux  0.1.11    <- runs as `p2pmux`"),
            "{}",
            joined(&lines)
        );
        assert!(
            text.contains("warning: `p2pmux` runs 0.1.11"),
            "the version that runs has to be in the warning: {text}"
        );
        assert!(text.contains("0.1.13 is installed at /usr/local/bin/p2pmux"));
        // The fix offered is the one that updates the copy that actually runs,
        // not the one that updates whichever channel doctor was launched from.
        assert!(
            text.contains("brew update && brew upgrade p2pmux"),
            "{text}"
        );
        assert!(
            text.contains("put /usr/local/bin earlier on your PATH"),
            "{text}"
        );
        // Doctor's reader is looking at a terminal, and the sentence naming the
        // version to update comes after a path of unbounded length.
        for line in &lines {
            assert!(
                line.chars().count() <= 78,
                "line is too wide to read: {line}"
            );
        }
        // The command is there to be copied, so it survives the wrap whole.
        assert!(
            lines
                .iter()
                .any(|line| line.contains("`brew update && brew upgrade p2pmux`")),
            "{}",
            joined(&lines)
        );
    }

    /// Every other machine. Two copies of the same release, or one copy, is not
    /// a problem, and a warning there would train people to ignore this one.
    #[test]
    fn a_winner_that_is_the_newest_installed_raises_no_warning() {
        for group in [
            vec![install("/usr/local/bin/p2pmux", Some("0.1.13"))],
            vec![
                install("/usr/local/bin/p2pmux", Some("0.1.13")),
                install("/opt/homebrew/bin/p2pmux", Some("0.1.13")),
            ],
            vec![
                install("/usr/local/bin/p2pmux", Some("0.1.13")),
                install("/opt/homebrew/bin/p2pmux", Some("0.1.11")),
            ],
        ] {
            let text = joined(&report(&group));
            assert!(!text.contains("warning"), "{text}");
            assert!(text.contains("<- runs as `p2pmux`"), "{text}");
        }
    }

    /// A stale build exits non-zero on `--version`, and something else on PATH
    /// named p2pmux may not be p2pmux at all. Neither can be compared, so an
    /// unreadable *loser* is listed and left alone -- while an unreadable
    /// *winner* is the one case worth saying out loud.
    #[test]
    fn an_unreadable_version_only_warns_when_it_is_the_copy_that_runs() {
        let quiet = joined(&report(&[
            install("/usr/local/bin/p2pmux", Some("0.1.13")),
            install("/home/x/.cargo/bin/p2pmux", None),
        ]));
        assert!(!quiet.contains("warning"), "{quiet}");
        assert!(quiet.contains("unknown"), "{quiet}");

        let loud = unwrapped(&report(&[
            install("/home/x/.cargo/bin/p2pmux", None),
            install("/usr/local/bin/p2pmux", Some("0.1.13")),
        ]));
        assert!(loud.contains("did not report a version"), "{loud}");
        assert!(loud.contains("0.1.13 is installed"), "{loud}");
    }

    /// A command that installs p2pmux somewhere is not a command that replaces
    /// the copy winning the PATH. The installer writes /usr/local/bin, so for a
    /// winner anywhere else it has to be told where to write, or doctor is
    /// handing out a command that rewrites a file nobody was running.
    #[test]
    fn the_offered_command_updates_the_copy_that_actually_runs() {
        let unowned = unwrapped(&report(&[
            install("/usr/local/sbin/p2pmux", Some("0.1.11")),
            install("/usr/local/bin/p2pmux", Some("0.1.13")),
        ]));
        assert!(
            unowned.contains("| P2PMUX_INSTALL_DIR=/usr/local/sbin sh"),
            "{unowned}"
        );

        // Where the installer already writes, it needs no telling.
        let owned = unwrapped(&report(&[
            install("/usr/local/bin/p2pmux", Some("0.1.11")),
            install("/home/x/.cargo/bin/p2pmux", Some("0.1.13")),
        ]));
        assert!(
            owned.contains("`curl -fsSL https://p2pmux.com/install.sh | sh`"),
            "{owned}"
        );
        assert!(!owned.contains("P2PMUX_INSTALL_DIR"), "{owned}");

        // And a channel that does own its copy keeps its own command.
        let brewed = unwrapped(&report(&[
            install("/opt/homebrew/bin/p2pmux", Some("0.1.11")),
            install("/usr/local/bin/p2pmux", Some("0.1.13")),
        ]));
        assert!(
            brewed.contains("brew update && brew upgrade p2pmux"),
            "{brewed}"
        );
        assert!(!brewed.contains("P2PMUX_INSTALL_DIR"), "{brewed}");
    }

    /// With nothing on PATH the hooks that invoke `p2pmux` by name never run,
    /// which is what doctor warned about before it could list copies at all.
    #[test]
    fn an_empty_path_still_warns_that_hooks_cannot_run() {
        let text = joined(&report(&[]));
        assert!(text.contains("not on PATH"), "{text}");
        assert!(text.contains("hooks"), "{text}");
    }

    /// PATH repeats directories all the time -- a shell rc that prepends
    /// unconditionally, a login shell sourced twice. One binary reached twice
    /// is one copy; listing it again would invent a conflict with itself.
    #[test]
    fn a_directory_that_appears_twice_on_path_is_one_copy() {
        let root = std::env::temp_dir().join(format!("p2pmux-path-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("temp bin");
        let binary = bin.join("p2pmux");
        std::fs::write(&binary, "#!/bin/sh\n").expect("write");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        // A directory holding no p2pmux, an empty entry meaning the working
        // directory, and the same directory twice: one copy comes out.
        let path = OsString::from(format!(
            "{bin}:{root}::{bin}",
            bin = bin.display(),
            root = root.display()
        ));
        let found = installs_in(&path, |_| Some("0.1.13".to_owned()));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, binary);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A copy is asked its version by being run, so its output reaches doctor's
    /// terminal. Anything that is not shaped like a version is unknown --
    /// including a name carrying escape sequences.
    #[test]
    fn only_something_shaped_like_a_version_is_read_as_one() {
        assert_eq!(
            version_from_output("p2pmux 0.1.13\n"),
            Some("0.1.13".to_owned())
        );
        assert_eq!(
            version_from_output("p2pmux 0.2.0-rc1\n"),
            Some("0.2.0-rc1".to_owned())
        );
        // Some other program that happens to be called p2pmux, and a build
        // whose `--version` prints its name and nothing after it.
        assert_eq!(version_from_output("usage: p2pmux [options]\n"), None);
        assert_eq!(version_from_output("p2pmux\n"), None);
        assert_eq!(version_from_output("\u{1b}[31m0.1.13\u{1b}[0m\n"), None);
        assert_eq!(version_from_output(""), None);
        assert_eq!(version_from_output("p2pmux \n"), None);
    }
}
