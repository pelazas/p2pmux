//! Whether a newer p2pmux has been released, and what to run to get it.
//!
//! p2pmux installs four different ways and updates none of them for you, so
//! until now the only way to learn a release had happened was to go and look.
//! People do not go and look.
//!
//! Three rules shape what this does and, mostly, what it refuses to do:
//!
//! - **It never delays a launch.** The check runs on its own thread and the
//!   answer arrives whenever it arrives; a network that is down or slow costs
//!   nothing but a notice that does not appear.
//! - **It never installs anything.** Replacing the binary you are running,
//!   under whichever package manager put it there, is not something to do
//!   behind a `y/n` prompt at startup. It tells you the one command that fits
//!   how *this* copy was installed, and that is all.
//! - **It asks at most once a day.** The answer is cached with the time it was
//!   fetched, so a person who opens ten sessions makes one request.
//!
//! GitHub Releases is the source rather than crates.io or the Homebrew tap:
//! tagging publishes there automatically, and the other two are updated by hand
//! afterwards — so they are the ones that lag, and a check against a lagging
//! source is a check that says nothing on the day it matters.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Where GitHub redirects to the newest release.
///
/// The web page, not the REST API, and asked with `HEAD` so nothing is
/// downloaded: all this needs is the URL it lands on, which ends in the tag.
/// The API would be tidier to read and is rate-limited to sixty requests an
/// hour per *IP* — which a developer's `gh` has usually spent already, and
/// which one office behind one NAT shares. A check that quietly stops working
/// on the machines most likely to have this installed is not a check.
const LATEST_RELEASE_URL: &str = "https://github.com/pelazas/p2pmux/releases/latest";

/// What the redirect's path looks like before the tag.
const RELEASE_TAG_PATH: &str = "/releases/tag/";

/// How long an answer stands before it is worth asking again.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bound on the whole request. Nothing waits on this, but a connection that
/// hangs forever is a thread that never ends.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A release newer than the one running, and how to get it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateNotice {
    pub version: String,
    pub command: &'static str,
}

/// What one update check learned about the release this binary is running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Check {
    Newer(UpdateNotice),
    Current { latest: String },
    Unknown,
}

impl UpdateNotice {
    /// The one line `p2pmux doctor` prints: versions, and the command to type.
    ///
    /// Doctor is a sentence on stdout, not a screen with keys, so the command
    /// is the whole of what it can offer. The inbox has its own line.
    pub fn line(&self) -> String {
        format!(
            "p2pmux {} is out — you have {}. Update with `{}`",
            self.version,
            env!("CARGO_PKG_VERSION"),
            self.command
        )
    }

    /// The one line the inbox shows.
    ///
    /// Same three facts as [`Self::line`], plus the key that takes the update
    /// without leaving to paste. The command stays on the line because a notice
    /// that only says "an update is available" is one people learn to skip.
    pub fn inbox_line(&self) -> String {
        format!(
            "p2pmux {} is out — you have {}. u update · `{}`",
            self.version,
            env!("CARGO_PKG_VERSION"),
            self.command
        )
    }

    /// Argv that runs [`Self::command`] in a pane.
    ///
    /// The upgrade line is a shell snippet (`&&`, a pipe), and a pane launches
    /// an argv, not a string. `sh -c` is the one translation that keeps the
    /// command the notice already named.
    pub fn argv(&self) -> Vec<String> {
        vec![
            String::from("sh"),
            String::from("-c"),
            self.command.to_owned(),
        ]
    }
}

/// What the last check found, and when.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedCheck {
    latest: String,
    checked_unix_ms: u64,
}

/// The three-number core of a version, or `None` for anything else.
///
/// A tag with a pre-release suffix — `v0.2.0-rc1` — is deliberately unparsed
/// rather than truncated to `0.2.0`: nobody running a stable build should be
/// sent to a release candidate by a string comparison that dropped the part
/// that said so.
fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let mut parts = raw.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Whether `latest` is a release after `current`.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(current), Some(latest)) => latest > current,
        // Either one unreadable means no notice. A version this build cannot
        // compare is not one it should be nagging about.
        _ => false,
    }
}

/// The upgrade command that fits the copy of p2pmux at `exe`.
///
/// Told from where the binary lives, which is the only evidence there is: no
/// install path records how it was installed. The installer's own directory is
/// the fallback, and it is also what an unrecognized path gets — re-running the
/// installer is the one command that works wherever the binary came from.
pub fn upgrade_command(exe: &Path) -> &'static str {
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/homebrew/") || path.contains("/linuxbrew/") {
        "brew update && brew upgrade p2pmux"
    } else if path.contains("/.cargo/bin/") {
        "cargo install p2pmux --locked --force"
    } else {
        "curl -fsSL https://p2pmux.com/install.sh | sh"
    }
}

fn cache_path() -> Option<PathBuf> {
    crate::config::config_path()
        .ok()?
        .parent()
        .map(|directory| directory.join("update-check.json"))
}

fn read_cache(path: &Path, now_unix_ms: u64) -> Option<String> {
    let cached: CachedCheck = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let age = now_unix_ms.saturating_sub(cached.checked_unix_ms);
    // A clock that has gone backwards leaves a check stamped in the future.
    // Treat that as stale rather than as valid for the next fifty years.
    (cached.checked_unix_ms <= now_unix_ms && age < CHECK_INTERVAL.as_millis() as u64)
        .then_some(cached.latest)
}

fn write_cache(path: &Path, latest: &str, now_unix_ms: u64) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let record = CachedCheck {
        latest: latest.to_owned(),
        checked_unix_ms: now_unix_ms,
    };
    if let Ok(bytes) = serde_json::to_vec(&record) {
        let _ = std::fs::write(path, bytes);
    }
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// The tag in a `…/releases/tag/vX.Y.Z` URL.
///
/// Anything else — a redirect that landed somewhere unexpected, a repository
/// with no releases at all, a login wall — is `None`, and `None` is a p2pmux
/// that says nothing about updates.
fn tag_from_release_url(url: &str) -> Option<String> {
    let tag = url.split_once(RELEASE_TAG_PATH)?.1;
    (!tag.is_empty() && !tag.contains('/')).then(|| tag.to_owned())
}

/// Ask GitHub which release is newest.
async fn fetch_latest() -> Option<String> {
    // Same reasoning as the rendezvous client: reqwest is built with no default
    // crypto provider so it rides the `ring` build iroh already pulls in, and
    // the process default has to be installed before the first client exists.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Named, so the request is identifiable in a log as a version check
        // from a p2pmux rather than as an anonymous scrape.
        .user_agent(concat!("p2pmux/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?
        .head(LATEST_RELEASE_URL)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    // The answer is which page it landed on, not the page.
    tag_from_release_url(response.url().as_str())
}

/// The latest released version, from the cache when it is fresh enough.
fn latest_version() -> Option<String> {
    let path = cache_path()?;
    let now = unix_ms_now();
    if let Some(cached) = read_cache(&path, now) {
        return Some(cached);
    }
    let latest = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?
        .block_on(fetch_latest())?;
    // Stamped whatever it says, including a version this build cannot parse:
    // the point of the stamp is to stop asking, and a tag we did not
    // understand is not a reason to ask again in a minute.
    write_cache(&path, &latest, now);
    Some(latest)
}

/// Start the check. The answer arrives on the channel, or never.
///
/// Never blocks and never fails: a machine with no network, a rate limit, a
/// release feed that changed shape — all of them are a channel that stays
/// empty, which is a p2pmux that says nothing about updates.
pub fn spawn() -> Receiver<UpdateNotice> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        if let Some(notice) = check() {
            let _ = sender.send(notice);
        }
    });
    receiver
}

/// The check itself, run on the calling thread. `None` when this is the latest
/// release, or when nothing could be learned.
pub fn check() -> Option<UpdateNotice> {
    match status() {
        Check::Newer(notice) => Some(notice),
        Check::Current { .. } | Check::Unknown => None,
    }
}

/// The check itself, with a distinction between a current release and no answer.
pub fn status() -> Check {
    let Some(latest) = latest_version() else {
        return Check::Unknown;
    };
    if is_newer(env!("CARGO_PKG_VERSION"), &latest) {
        return Check::Newer(UpdateNotice {
            version: latest.trim_start_matches('v').to_owned(),
            command: upgrade_command(&std::env::current_exe().unwrap_or_default()),
        });
    }
    Check::Current { latest }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_newer_only_when_its_numbers_are() {
        assert!(is_newer("0.1.6", "0.1.7"));
        assert!(is_newer("0.1.6", "v0.2.0"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("0.1.6", "0.1.6"));
        assert!(!is_newer("0.1.6", "v0.1.5"));
        // Ten is after nine, which a string comparison would get wrong.
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(!is_newer("0.1.10", "0.1.9"));
    }

    /// Nothing is said about a version this build cannot read. A notice
    /// pointing at a release candidate — or at a tag whose shape changed — is
    /// worse than no notice: it sends someone to install something they did not
    /// ask for.
    #[test]
    fn a_version_that_cannot_be_read_raises_nothing() {
        for latest in ["", "v0.2.0-rc1", "nightly", "0.2", "0.1.6.1", "v.1.2"] {
            assert!(!is_newer("0.1.6", latest), "{latest:?} must say nothing");
        }
        assert!(!is_newer("not-a-version", "0.2.0"));
    }

    #[test]
    fn the_command_matches_how_this_copy_was_installed() {
        assert_eq!(
            upgrade_command(Path::new("/opt/homebrew/Cellar/p2pmux/0.1.6/bin/p2pmux")),
            "brew update && brew upgrade p2pmux"
        );
        assert_eq!(
            upgrade_command(Path::new("/home/linuxbrew/.linuxbrew/bin/p2pmux")),
            "brew update && brew upgrade p2pmux"
        );
        assert_eq!(
            upgrade_command(Path::new("/Users/me/.cargo/bin/p2pmux")),
            "cargo install p2pmux --locked --force"
        );
        // The installer's own directory, and anywhere else: re-running the
        // installer is the one command that works whatever put it there.
        assert_eq!(
            upgrade_command(Path::new("/usr/local/bin/p2pmux")),
            "curl -fsSL https://p2pmux.com/install.sh | sh"
        );
        assert_eq!(
            upgrade_command(Path::new("/opt/tools/p2pmux")),
            "curl -fsSL https://p2pmux.com/install.sh | sh"
        );
    }

    #[test]
    fn a_cached_answer_stands_for_a_day_and_no_longer() {
        let directory =
            std::env::temp_dir().join(format!("p2pmux-update-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("update-check.json");
        let day = CHECK_INTERVAL.as_millis() as u64;

        write_cache(&path, "v0.9.9", 1_000_000);

        assert_eq!(read_cache(&path, 1_000_000), Some(String::from("v0.9.9")));
        assert_eq!(
            read_cache(&path, 1_000_000 + day - 1),
            Some(String::from("v0.9.9"))
        );
        assert_eq!(read_cache(&path, 1_000_000 + day), None);
        // A clock that went backwards leaves a stamp in the future. Asking
        // again costs one request; trusting it costs every future release.
        assert_eq!(read_cache(&path, 999_999), None);
        assert_eq!(read_cache(&directory.join("absent.json"), 1_000_000), None);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The notice has to say all three things: what is out, what is running,
    /// and the one command to type. A line missing the last one sends the
    /// reader to a search engine.
    /// The whole answer is which page the redirect landed on, so this is the
    /// one place a change at GitHub's end would break the check — and it has to
    /// break it into silence, not into a wrong version.
    #[test]
    fn the_tag_is_read_off_the_page_the_redirect_lands_on() {
        assert_eq!(
            tag_from_release_url("https://github.com/pelazas/p2pmux/releases/tag/v0.1.6"),
            Some(String::from("v0.1.6"))
        );
        for url in [
            // No releases yet: GitHub stays on the list page.
            "https://github.com/pelazas/p2pmux/releases",
            "https://github.com/pelazas/p2pmux/releases/tag/",
            "https://github.com/pelazas/p2pmux/releases/tag/v1/extra",
            "https://github.com/login?return_to=%2Fpelazas%2Fp2pmux",
            "",
        ] {
            assert_eq!(tag_from_release_url(url), None, "{url} must say nothing");
        }
    }

    #[test]
    fn the_notice_names_the_version_and_the_command() {
        let notice = UpdateNotice {
            version: String::from("0.9.9"),
            command: "brew update && brew upgrade p2pmux",
        };
        let line = notice.line();

        assert!(line.contains("0.9.9"), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.contains("brew upgrade p2pmux"), "{line}");
        assert!(
            line.contains("Update with"),
            "doctor has no key to offer, so it names the command: {line}"
        );
        assert!(
            !line.contains("u update"),
            "the inbox verb does not belong on stdout: {line}"
        );

        let inbox = notice.inbox_line();
        assert!(inbox.contains("0.9.9"), "{inbox}");
        assert!(inbox.contains("u update"), "{inbox}");
        assert!(inbox.contains("brew upgrade p2pmux"), "{inbox}");
        assert_eq!(
            notice.argv(),
            vec![
                String::from("sh"),
                String::from("-c"),
                String::from("brew update && brew upgrade p2pmux"),
            ]
        );
    }
}
