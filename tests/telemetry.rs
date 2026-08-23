//! What p2pmux says about itself, and to whom.
//!
//! The unit tests in `src/telemetry.rs` cover the rules — what an answer means,
//! when a send is due, which environment variables silence it. These cover the
//! part that has to be true of the shipped binary: that a machine nobody asked
//! sends nothing, that the command which claims to print the payload prints the
//! payload, and that turning it off actually turns it off.
//!
//! Every test runs against its own `HOME`, because the thing under test writes to
//! the config directory and a developer's own consent must not decide whether
//! their test suite passes.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .env("HOME", home)
        // `XDG_CONFIG_HOME` outranks `HOME`, so a developer who has one set
        // would otherwise send the child straight into their own state file --
        // and a test that granted consent there would turn the suite into a
        // thing that opts its author in.
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("DO_NOT_TRACK")
        .env_remove("CI")
        .env_remove("P2PMUX_TELEMETRY")
        .output()
        .expect("p2pmux binary should run")
}

fn temporary_home(label: &str) -> PathBuf {
    let home =
        std::env::temp_dir().join(format!("p2pmux-telemetry-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).expect("temporary home");
    home
}

fn state(home: &Path) -> Option<serde_json::Value> {
    let raw = fs::read(home.join(".config").join("p2pmux").join("telemetry.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The state a fresh install is in, and the one that matters most: nothing has
/// been decided, so nothing is sent. A regression here would be p2pmux quietly
/// collecting from people who were never asked.
#[test]
fn an_untouched_machine_has_agreed_to_nothing() {
    let home = temporary_home("untouched");

    let output = run(&home, &["telemetry"]);

    assert!(output.status.success());
    let said = stdout(&output);
    assert!(said.contains("Not asked yet"), "{said}");
    assert!(said.contains("nothing is sent"), "{said}");
    assert!(
        state(&home).is_none(),
        "asking what the setting is must not decide it"
    );

    let _ = fs::remove_dir_all(&home);
}

/// `telemetry show` is the command that has to be trustworthy, because it is the
/// one anybody suspicious will run. It prints the payload on a machine that has
/// said no, too — answering "what would this send about me" with nothing reads as
/// evasion rather than as reassurance.
#[test]
fn show_prints_the_payload_and_says_whether_it_would_be_sent() {
    let home = temporary_home("show");

    let before = stdout(&run(&home, &["telemetry", "show"]));
    let payload: serde_json::Value = serde_json::from_str(
        before
            .split_once('}')
            .map(|(head, _)| format!("{head}}}"))
            .expect("a json object")
            .as_str(),
    )
    .expect("show prints json");

    let mut keys: Vec<&str> = payload
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "activated",
            "agents",
            "id",
            "os",
            "peers",
            "sessions",
            "version"
        ],
        "show must print the whole payload and nothing beyond it"
    );
    assert!(
        before.contains("Not sent"),
        "a machine that has not agreed must say so: {before}"
    );

    let _ = fs::remove_dir_all(&home);
}

/// On, then off, then on again. The id survives the round trip on purpose: a
/// person who pauses for a week and comes back is one install, and minting a new
/// id would report them as two.
#[test]
fn consent_can_be_given_taken_back_and_given_again_without_becoming_a_new_install() {
    let home = temporary_home("roundtrip");

    assert!(run(&home, &["telemetry", "on"]).status.success());
    let granted = state(&home).expect("state written");
    assert_eq!(granted["consent"], "granted");
    let id = granted["id"].as_str().expect("an id").to_owned();
    assert_eq!(id.len(), 32, "{id}");
    assert!(
        id.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    );

    assert!(run(&home, &["telemetry", "off"]).status.success());
    let denied = state(&home).expect("state written");
    assert_eq!(denied["consent"], "denied");

    let said = stdout(&run(&home, &["telemetry"]));
    assert!(said.contains("Off"), "{said}");
    assert!(said.contains("nothing is sent"), "{said}");

    assert!(run(&home, &["telemetry", "on"]).status.success());
    let again = state(&home).expect("state written");
    assert_eq!(again["consent"], "granted");
    assert_eq!(again["id"].as_str(), Some(id.as_str()), "same install");

    let _ = fs::remove_dir_all(&home);
}

/// A machine that declines must not be left holding an identifier for the thing
/// it declined.
#[test]
fn a_refusal_creates_no_identifier() {
    let home = temporary_home("refusal");

    assert!(run(&home, &["telemetry", "off"]).status.success());

    let stored = state(&home).expect("state written");
    assert_eq!(stored["consent"], "denied");
    assert!(
        stored.get("id").is_none_or(serde_json::Value::is_null),
        "a machine that said no has nothing to identify: {stored}"
    );

    let _ = fs::remove_dir_all(&home);
}

/// The prompt fires from `run`, which every screen-taking command goes through,
/// and must not fire from the commands that an agent hook or a script runs. A
/// question written into a pipe is a question nobody can answer, and `notify`
/// runs on every single tool call an agent makes.
#[test]
fn nothing_asks_when_there_is_no_terminal_to_ask_in() {
    let home = temporary_home("piped");

    for args in [
        vec!["config", "get", "name"],
        vec!["list"],
        vec!["telemetry"],
    ] {
        let output = run(&home, &args);
        let said = format!(
            "{}{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !said.contains("Send it?"),
            "`p2pmux {}` asked into a pipe: {said}",
            args.join(" ")
        );
    }
    assert!(
        state(&home).is_none(),
        "a run with no terminal must decide nothing"
    );

    let _ = fs::remove_dir_all(&home);
}

/// The conventional switches, checked against the shipped binary rather than
/// against the function that reads them: this is the promise the README makes to
/// somebody who does not want to be counted, and they will test it exactly this
/// way.
#[test]
fn do_not_track_and_ci_stop_it_at_the_binary() {
    let home = temporary_home("switches");
    run(&home, &["telemetry", "on"]);

    for (name, value) in [
        ("DO_NOT_TRACK", "1"),
        ("CI", "true"),
        ("P2PMUX_TELEMETRY", "0"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_p2pmux"))
            .args(["telemetry"])
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("DO_NOT_TRACK")
            .env_remove("CI")
            .env_remove("P2PMUX_TELEMETRY")
            .env(name, value)
            .output()
            .expect("p2pmux binary should run");
        let said = stdout(&output);
        assert!(
            said.contains("Off") || said.contains("nothing is sent"),
            "{name}={value} did not stop it: {said}"
        );
    }

    let _ = fs::remove_dir_all(&home);
}

/// Put a machine in the state the one-time question is gated on: it agreed, and
/// somebody else has been in a session on it.
fn already_activated(home: &Path) {
    let directory = home.join(".config").join("p2pmux");
    fs::create_dir_all(&directory).expect("config directory");
    fs::write(
        directory.join("telemetry.json"),
        br#"{"consent":"granted","id":"0123456789abcdef0123456789abcdef","activated":true}"#,
    )
    .expect("state file");
}

/// Run p2pmux attached to a real terminal, and return everything it wrote.
///
/// The question only prints to a terminal, so a plain `Command` can prove it
/// stays quiet and nothing else. This is the other half.
fn run_on_a_terminal(home: &Path, args: &[&str]) -> String {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("a pty");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_p2pmux"));
    for argument in args {
        command.arg(argument);
    }
    command.env("HOME", home);
    command.env_remove("XDG_CONFIG_HOME");
    command.env_remove("XDG_STATE_HOME");
    command.env_remove("DO_NOT_TRACK");
    command.env_remove("CI");
    command.env_remove("P2PMUX_TELEMETRY");
    let mut child = pty.slave.spawn_command(command).expect("spawn");
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().expect("reader");
    let mut said = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => said.push_str(&String::from_utf8_lossy(&buffer[..read])),
            Err(_) => break,
        }
    }
    let _ = child.wait();
    said
}

/// The one thing p2pmux ever asks for unprompted. It has to be once, and it has
/// to be after there is something to answer about — a question on install, when
/// nobody has used it yet, is a question that trains people to skip the next one.
#[test]
fn the_one_question_is_asked_once_and_only_after_a_second_person_joined() {
    let home = temporary_home("asked");

    // Agreed to telemetry, but nobody has joined a session yet.
    run(&home, &["telemetry", "on"]);
    let early = run_on_a_terminal(&home, &["config", "get", "name"]);
    assert!(
        !early.contains("p2pmux.com/hi"),
        "asked before there was anything to ask about: {early}"
    );

    already_activated(&home);
    let asked = run_on_a_terminal(&home, &["config", "get", "name"]);
    assert!(asked.contains("p2pmux.com/hi"), "{asked}");
    assert!(
        asked.contains("never again"),
        "a question that does not promise to stop is one people brace for: {asked}"
    );

    let again = run_on_a_terminal(&home, &["config", "get", "name"]);
    assert!(
        !again.contains("p2pmux.com/hi"),
        "asked twice, which is the whole thing it promised not to do: {again}"
    );

    let _ = fs::remove_dir_all(&home);
}

/// A pipe cannot answer a question, and must not burn the one chance to ask it.
/// `p2pmux` inside a script or a CI job would otherwise spend it on nobody.
#[test]
fn a_pipe_is_never_asked_and_never_spends_the_ask() {
    let home = temporary_home("unasked");
    already_activated(&home);

    let output = run(&home, &["config", "get", "name"]);
    let said = format!(
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!said.contains("p2pmux.com/hi"), "{said}");
    let stored = state(&home).expect("state survives");
    assert!(
        stored
            .get("asked_for_a_word")
            .is_none_or(|asked| asked == &serde_json::Value::Bool(false)),
        "a run nobody saw must leave the question unasked: {stored}"
    );

    let _ = fs::remove_dir_all(&home);
}
