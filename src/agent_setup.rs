//! `p2pmux setup` and `p2pmux doctor` — wiring an agent's hooks up, and saying
//! whether they are wired.
//!
//! Every status in the inbox comes from a hook. Nothing is inferred
//! from output timing any more, because timing could never tell a thinking agent
//! from one blocked on a permission prompt. That makes hook installation the
//! difference between an inbox that works and one that lists processes it
//! knows nothing about — far too important to leave as six JSON objects for a
//! human to paste into the right place in `settings.json`.
//!
//! **Marker-owned, not merged.** Every entry this writes carries
//! [`MARKER`]. Installing replaces exactly the entries carrying it and touches
//! nothing else; uninstalling removes exactly those. A user's own hooks on the
//! same events — a completion chime on `Stop`, say — survive both, because this
//! module never has to guess which entries were its own.

use std::{error::Error, fs, io, path::PathBuf};

use serde_json::{Map, Value};

/// Stamped into every hook entry this module owns.
///
/// `claude` ignores unknown keys in a hook entry, so this rides along in the
/// settings file and makes ownership a fact to read rather than a guess.
const MARKER: &str = "p2pmux";

/// The hook events p2pmux registers, and the status each reports.
///
/// `PreToolUse`/`PostToolUse` fire on every tool call and are what keeps a long
/// turn showing as working rather than decaying into silence. `Notification` is
/// the one that could never be inferred: only the agent knows it is blocked on a
/// human.
const CLAUDE_HOOKS: &[(&str, &str, bool)] = &[
    ("UserPromptSubmit", "running", false),
    ("PreToolUse", "running", true),
    ("PostToolUse", "running", true),
    ("Notification", "pending", false),
    ("Stop", "done", false),
    ("SessionEnd", "idle", false),
];

/// What `doctor` found for one agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Wiring {
    /// Hooks present and owned by p2pmux.
    Wired,
    /// The agent is installed but nothing is reporting to p2pmux.
    Unwired,
    /// The agent is not installed on this machine — not a problem to fix.
    Absent,
}

impl Wiring {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Wired => "wired",
            Self::Unwired => "not wired",
            Self::Absent => "not installed",
        }
    }
}

pub fn claude_settings_path() -> Option<PathBuf> {
    Some(home()?.join(".claude").join("settings.json"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Whether a binary is on `PATH`.
fn installed(binary: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(binary).is_file())
    })
}

fn read_settings(path: &PathBuf) -> Result<Map<String, Value>, Box<dyn Error>> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => Ok(map),
            // An empty file is a fresh install; anything else is a file we must
            // not overwrite, because it is the user's and we cannot merge it.
            Ok(Value::Null) => Ok(Map::new()),
            Ok(_) => Err(Box::from(format!(
                "{} is not a JSON object — refusing to rewrite it",
                path.display()
            ))),
            Err(error) if bytes.iter().all(u8::is_ascii_whitespace) => {
                let _ = error;
                Ok(Map::new())
            }
            Err(error) => Err(Box::from(format!(
                "{} is not valid JSON ({error}) — refusing to rewrite it",
                path.display()
            ))),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(Box::new(error)),
    }
}

/// Write `settings` to `path` through a temporary file and a rename.
///
/// This is the file an agent reads on every launch and the user edits by hand.
/// A half-written one would break both, so it is never truncated in place.
fn write_settings(path: &PathBuf, settings: &Map<String, Value>) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.p2pmux-tmp");
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(settings.clone()))?;
    bytes.push(b'\n');
    fs::write(&temporary, &bytes)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

/// Whether this entry is one of ours: it carries the marker, or it runs
/// `p2pmux notify`.
///
/// The second half is not belt and braces. Installs from before the marker
/// existed are still out there working perfectly, and `doctor` calling them
/// "not wired" sends someone to fix hooks that are already reporting — while
/// `setup` would leave the old entry beside the new one and report everything
/// twice. Matching on the command it runs recognizes a hand-written
/// registration too, which is the same hook by any other name.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("owner").and_then(Value::as_str) == Some(MARKER)
                    || hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(runs_our_producer)
            })
        })
}

/// Whether a hook command invokes `p2pmux notify`, however p2pmux was spelled.
///
/// An absolute path is as much ours as a bare name; anything else on the line
/// is not, so a user's own script that merely *mentions* p2pmux keeps its entry.
fn runs_our_producer(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    program == "p2pmux" && words.next() == Some("notify")
}

fn hook_entry(status: &str, matcher: bool) -> Value {
    let mut entry = Map::new();
    if matcher {
        entry.insert("matcher".into(), Value::String("*".into()));
    }
    let mut hook = Map::new();
    hook.insert("type".into(), Value::String("command".into()));
    hook.insert(
        "command".into(),
        Value::String(format!("p2pmux notify claude --status {status}")),
    );
    hook.insert("timeout".into(), Value::Number(5.into()));
    hook.insert("owner".into(), Value::String(MARKER.into()));
    entry.insert("hooks".into(), Value::Array(vec![Value::Object(hook)]));
    Value::Object(entry)
}

/// Drop every p2pmux-owned entry, keeping the user's own, and report whether any went.
fn strip_ours(settings: &mut Map<String, Value>) -> bool {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return false;
    };
    let mut removed = false;
    for value in hooks.values_mut() {
        if let Value::Array(entries) = value {
            let before = entries.len();
            entries.retain(|entry| !is_ours(entry));
            removed |= entries.len() != before;
        }
    }
    hooks.retain(|_, value| !value.as_array().is_some_and(Vec::is_empty));
    if hooks.is_empty() {
        settings.remove("hooks");
    }
    removed
}

pub fn claude_wiring() -> Wiring {
    if !installed("claude") {
        return Wiring::Absent;
    }
    let Some(path) = claude_settings_path() else {
        return Wiring::Unwired;
    };
    let Ok(settings) = read_settings(&path) else {
        return Wiring::Unwired;
    };
    let wired = settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks.values().any(|value| {
                value
                    .as_array()
                    .is_some_and(|entries| entries.iter().any(is_ours))
            })
        });
    if wired {
        Wiring::Wired
    } else {
        Wiring::Unwired
    }
}

/// Install (or remove) the Claude hooks. Idempotent in both directions.
pub fn setup_claude(uninstall: bool, dry_run: bool) -> Result<(), Box<dyn Error>> {
    let Some(path) = claude_settings_path() else {
        return Err(Box::from("cannot locate $HOME"));
    };
    let mut settings = read_settings(&path)?;
    let had_ours = strip_ours(&mut settings);

    if uninstall {
        if !had_ours {
            println!(
                "claude: already removed (no p2pmux hooks in {})",
                path.display()
            );
            return Ok(());
        }
        if dry_run {
            println!(
                "claude: would remove {} p2pmux hooks from {} (dry run)",
                CLAUDE_HOOKS.len(),
                path.display()
            );
            return Ok(());
        }
        write_settings(&path, &settings)?;
        println!("claude: removed the p2pmux hooks from {}", path.display());
        return Ok(());
    }

    if dry_run {
        println!(
            "claude: would write {} p2pmux hooks to {} (dry run)",
            CLAUDE_HOOKS.len(),
            path.display()
        );
        return Ok(());
    }

    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err(Box::from(format!(
            "the `hooks` key in {} is not an object — refusing to rewrite it",
            path.display()
        )));
    };
    for (event, status, matcher) in CLAUDE_HOOKS {
        let entries = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(entries) = entries else {
            return Err(Box::from(format!(
                "hooks.{event} in {} is not an array — refusing to rewrite it",
                path.display()
            )));
        };
        entries.push(hook_entry(status, *matcher));
    }
    write_settings(&path, &settings)?;
    println!(
        "claude: wrote {} p2pmux hooks to {}",
        CLAUDE_HOOKS.len(),
        path.display()
    );
    warn_if_not_on_path("claude");
    println!("claude: new Claude Code sessions pick this up; restart any that are running");
    Ok(())
}

/// The plugin `setup_opencode` writes, verbatim.
///
/// opencode has no hook list to merge into: a plugin is a file it imports, and
/// this file is entirely ours. That makes install a write and uninstall a
/// delete — but only of a file carrying [`MARKER`], so a plugin a user wrote
/// and happened to name `p2pmux.js` is refused rather than removed.
const OPENCODE_PLUGIN: &str = include_str!("assets/opencode_plugin.js");

pub fn opencode_plugin_path() -> Option<PathBuf> {
    Some(
        home()?
            .join(".config")
            .join("opencode")
            .join("plugin")
            .join("p2pmux.js"),
    )
}

/// Whether a file at `path` is one this module wrote.
fn ours_on_disk(path: &PathBuf) -> bool {
    fs::read_to_string(path).is_ok_and(|body| body.contains(&format!("owner: {MARKER}")))
}

pub fn opencode_wiring() -> Wiring {
    if !installed("opencode") {
        return Wiring::Absent;
    }
    match opencode_plugin_path() {
        Some(path) if ours_on_disk(&path) => Wiring::Wired,
        _ => Wiring::Unwired,
    }
}

/// Install (or remove) the opencode plugin. Idempotent in both directions.
pub fn setup_opencode(uninstall: bool, dry_run: bool) -> Result<(), Box<dyn Error>> {
    let Some(path) = opencode_plugin_path() else {
        return Err(Box::from("cannot locate $HOME"));
    };
    let present = path.exists();

    if uninstall {
        if !present {
            println!(
                "opencode: already removed (no plugin at {})",
                path.display()
            );
            return Ok(());
        }
        if !ours_on_disk(&path) {
            return Err(Box::from(format!(
                "{} was not written by p2pmux — refusing to delete it",
                path.display()
            )));
        }
        if dry_run {
            println!("opencode: would delete {} (dry run)", path.display());
            return Ok(());
        }
        fs::remove_file(&path)?;
        println!(
            "opencode: removed the p2pmux plugin from {}",
            path.display()
        );
        return Ok(());
    }

    if present && !ours_on_disk(&path) {
        return Err(Box::from(format!(
            "{} exists and was not written by p2pmux — refusing to overwrite it",
            path.display()
        )));
    }
    if dry_run {
        println!(
            "opencode: would write the p2pmux plugin to {} (dry run)",
            path.display()
        );
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("js.p2pmux-tmp");
    fs::write(&temporary, OPENCODE_PLUGIN)?;
    fs::rename(&temporary, &path)?;
    println!("opencode: wrote the p2pmux plugin to {}", path.display());
    warn_if_not_on_path("opencode");
    println!("opencode: new opencode sessions pick this up; restart any that are running");
    Ok(())
}

fn warn_if_not_on_path(agent: &str) {
    if !installed("p2pmux") {
        println!(
            "{agent}: warning — `p2pmux` is not on PATH, so the hooks will not run. \
             Install it, or edit the commands to use an absolute path."
        );
    }
}

/// Wire up every agent this build knows how to, for `p2pmux setup` with no
/// agent named — the form the inbox's own nudge tells people to run.
///
/// One agent failing does not stop the others: a machine with an unreadable
/// `settings.json` should still come out of this with its opencode plugin
/// installed, and be told exactly which half did not happen.
pub fn setup_all(uninstall: bool, dry_run: bool) -> Result<(), Box<dyn Error>> {
    let results = [
        setup_claude(uninstall, dry_run),
        setup_opencode(uninstall, dry_run),
    ];
    let mut failures = Vec::new();
    for result in results {
        if let Err(error) = result {
            println!("{error}");
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Box::from(failures.join("; ")))
    }
}

/// Report what is wired, for every agent p2pmux can report on.
pub fn doctor() -> Result<(), Box<dyn Error>> {
    let rows = [
        (
            "claude",
            claude_wiring(),
            claude_settings_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
        (
            "opencode",
            opencode_wiring(),
            opencode_plugin_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
    ];
    for (agent, wiring, path) in &rows {
        println!("{agent:<9} {:<14} {path}", wiring.label());
    }
    let states: Vec<&Wiring> = rows.iter().map(|(_, wiring, _)| wiring).collect();
    if states.iter().any(|wiring| **wiring == Wiring::Wired) {
        println!("\nThe inbox reads agent state from these hooks.");
    }
    if states.iter().any(|wiring| **wiring == Wiring::Unwired) {
        println!(
            "\nThe inbox can see that an agent is running but not what it is doing,\n\
             and can never say `needs you`. Run `p2pmux setup` to fix that."
        );
    }
    if states.iter().all(|wiring| **wiring == Wiring::Absent) {
        println!("\nNothing to wire up on this machine.");
    }
    if !installed("p2pmux") {
        println!("\nwarning: `p2pmux` is not on PATH — hooks invoke it by name and would not run.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registrations from before the marker existed are still installed on real
    /// machines and still reporting. Calling them "not wired" sends someone to
    /// fix what already works, and installing beside them reports twice.
    #[test]
    fn a_registration_without_the_marker_is_still_ours_if_it_runs_the_producer() {
        let ours = serde_json::json!({
            "hooks": [{"type": "command", "command": "p2pmux notify claude --status running"}]
        });
        assert!(is_ours(&ours));
        // Installed somewhere that is not on `PATH`, which is what the install
        // script does when it cannot write to /usr/local/bin.
        let absolute = serde_json::json!({
            "hooks": [{"type": "command", "command": "/opt/homebrew/bin/p2pmux notify claude"}]
        });
        assert!(is_ours(&absolute));

        // A hook of the user's own that merely mentions p2pmux is theirs, and
        // uninstall must leave it exactly where it is.
        for command in [
            "echo 'p2pmux notify' >> /tmp/log",
            "p2pmux ls",
            "notify p2pmux",
            "",
        ] {
            let theirs = serde_json::json!({
                "hooks": [{"type": "command", "command": command}]
            });
            assert!(!is_ours(&theirs), "{command:?} is not ours to remove");
        }
    }

    /// Installing over hooks an older build wrote replaces them rather than
    /// doubling up: every event ends with exactly one p2pmux entry.
    #[test]
    fn installing_over_an_unmarked_registration_leaves_one_entry_per_event() {
        let mut settings: Map<String, Value> = serde_json::from_str(
            r#"{"hooks": {"Stop": [
                 {"hooks": [{"type": "command", "command": "p2pmux notify claude --status done"}]},
                 {"hooks": [{"type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff"}]}
               ]}}"#,
        )
        .expect("fixture parses");

        assert!(strip_ours(&mut settings), "the old entry was recognized");

        let stop = settings["hooks"]["Stop"].as_array().expect("Stop entries");
        assert_eq!(stop.len(), 1, "only the user's own chime is left");
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "afplay /System/Library/Sounds/Glass.aiff"
        );
    }

    fn settings_with_user_hooks() -> Map<String, Value> {
        serde_json::from_str(
            r#"{
              "model": "opus",
              "hooks": {
                "Stop": [
                  {"hooks": [{"type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff"}]}
                ]
              }
            }"#,
        )
        .expect("fixture parses")
    }

    /// The reason entries are marked rather than matched by content: a user's
    /// own hook on the same event has to survive both install and uninstall.
    /// This fixture is the real thing — the completion chime on `Stop` that was
    /// already in the settings file this feature was built against.
    #[test]
    fn installing_and_removing_leaves_a_users_own_hooks_untouched() {
        let mut settings = settings_with_user_hooks();
        strip_ours(&mut settings);

        // Install.
        let hooks = settings
            .entry("hooks")
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(hooks) = hooks else {
            panic!("hooks is an object")
        };
        for (event, status, matcher) in CLAUDE_HOOKS {
            let entries = hooks
                .entry((*event).to_owned())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(entries) = entries else {
                panic!("entries is an array")
            };
            entries.push(hook_entry(status, *matcher));
        }
        let stop = settings["hooks"]["Stop"].as_array().expect("Stop entries");
        assert_eq!(
            stop.len(),
            2,
            "ours joined the user's, it did not replace it"
        );

        // Uninstall.
        assert!(strip_ours(&mut settings));
        let stop = settings["hooks"]["Stop"].as_array().expect("Stop entries");
        assert_eq!(stop.len(), 1);
        assert!(
            stop[0]["hooks"][0]["command"]
                .as_str()
                .expect("command")
                .contains("afplay"),
            "the user's own chime was removed with ours"
        );
        assert_eq!(settings["model"], "opus", "unrelated settings survived");
    }

    /// Installing twice must not leave two copies firing per event.
    #[test]
    fn installing_over_an_existing_install_replaces_rather_than_doubles() {
        let mut settings = Map::new();
        for _ in 0..2 {
            strip_ours(&mut settings);
            let hooks = settings
                .entry("hooks")
                .or_insert_with(|| Value::Object(Map::new()));
            let Value::Object(hooks) = hooks else {
                panic!("object")
            };
            for (event, status, matcher) in CLAUDE_HOOKS {
                let entries = hooks
                    .entry((*event).to_owned())
                    .or_insert_with(|| Value::Array(Vec::new()));
                let Value::Array(entries) = entries else {
                    panic!("array")
                };
                entries.push(hook_entry(status, *matcher));
            }
        }
        for (event, ..) in CLAUDE_HOOKS {
            assert_eq!(
                settings["hooks"][*event].as_array().expect("entries").len(),
                1,
                "{event} would fire twice per event"
            );
        }
    }

    #[test]
    fn a_settings_file_that_is_not_json_is_refused_rather_than_rewritten() {
        let directory = std::env::temp_dir().join(format!("p2pmux-setup-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("settings.json");
        fs::write(&path, b"{ this is not json").expect("write");

        let error = read_settings(&path).expect_err("must refuse");
        assert!(error.to_string().contains("refusing to rewrite"));
        assert_eq!(
            fs::read(&path).expect("still there"),
            b"{ this is not json",
            "the file was rewritten anyway"
        );

        // A missing or blank file is a fresh install, not a refusal.
        fs::write(&path, b"   \n").expect("write");
        assert!(read_settings(&path).expect("blank is fresh").is_empty());
        fs::remove_file(&path).expect("remove");
        assert!(read_settings(&path).expect("missing is fresh").is_empty());

        fs::remove_dir_all(&directory).expect("cleanup");
    }

    #[test]
    fn every_registered_hook_maps_to_a_status_the_producer_accepts() {
        for (event, status, _) in CLAUDE_HOOKS {
            assert!(
                crate::agent_detect::AgentState::from_wire(status).is_some(),
                "{event} registers `{status}`, which `p2pmux notify` would refuse"
            );
        }
    }

    /// The plugin is a whole file rather than entries in a shared one, so
    /// ownership is a line in it. Without the marker, uninstall would refuse to
    /// remove what install had just written.
    #[test]
    fn the_opencode_plugin_says_who_owns_it() {
        assert!(
            OPENCODE_PLUGIN.contains(&format!("owner: {MARKER}")),
            "install writes a file uninstall would then refuse to delete"
        );
    }

    /// The plugin's other half is a shell command line, and the two halves are
    /// in different languages: nothing but this test connects the statuses the
    /// JavaScript pushes to the ones the Rust producer will accept.
    #[test]
    fn every_status_the_opencode_plugin_pushes_is_one_the_producer_accepts() {
        let mut seen = 0;
        for (index, _) in OPENCODE_PLUGIN.match_indices("push(\"") {
            let rest = &OPENCODE_PLUGIN[index + "push(\"".len()..];
            let status = rest.split('"').next().expect("a quoted status");
            assert!(
                crate::agent_detect::AgentState::from_wire(status).is_some(),
                "the plugin pushes `{status}`, which `p2pmux notify` would refuse"
            );
            seen += 1;
        }
        assert!(seen >= 5, "only {seen} pushes found — did the plugin move?");
        assert!(
            OPENCODE_PLUGIN.contains("p2pmux\", [\"notify\", \"opencode\""),
            "the plugin must report as opencode, or its rows file under the wrong agent"
        );
    }

    /// A user's own `p2pmux.js` is not ours to overwrite or delete, and the
    /// only thing that can tell them apart is the marker.
    #[test]
    fn a_plugin_file_we_did_not_write_is_left_alone() {
        let directory = std::env::temp_dir().join(format!("p2pmux-plugin-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("p2pmux.js");

        fs::write(&path, b"export const Mine = async () => ({})").expect("write");
        assert!(!ours_on_disk(&path));

        fs::write(&path, OPENCODE_PLUGIN).expect("write");
        assert!(ours_on_disk(&path));

        fs::remove_dir_all(&directory).expect("cleanup");
    }
}
