use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::SocketAddr,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use iroh::{EndpointAddr, SecretKey};
use p2pmux::{rendezvous::LocalRendezvous, ticket::JoinTicket};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .output()
        .expect("p2pmux binary should run")
}

/// Run against an isolated rendezvous cache so tests never read the user's live sessions.
fn run_with_cache(cache_home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .env("XDG_CACHE_HOME", cache_home)
        .output()
        .expect("p2pmux binary should run")
}

#[test]
fn join_rejects_an_invalid_ticket_without_echoing_it() {
    let output = run(&["join", "not-a-ticket"]);

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stdout.contains("TRUST WARNING"));
    assert!(stdout.contains("fully trusted shared-shell session"));
    assert!(stderr.contains("invalid ticket format"));
    assert!(!stdout.contains("not-a-ticket"));
    assert!(!stderr.contains("not-a-ticket"));
}

#[test]
fn join_requires_a_ticket_argument() {
    let output = run(&["join"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("<TICKET>"));
}

#[test]
fn help_lists_the_local_terminal_command() {
    let output = run(&["--help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("local"));
    assert!(stdout.contains("local interactive shell"));
    assert!(stdout.contains("reusable shared-session ticket"));
    assert!(stdout.contains("remote fixed-grid shared pane"));
    assert!(stdout.contains("ticket"));
    assert!(stdout.contains("full reusable join ticket"));
}

#[test]
fn ticket_prints_a_pasteable_ticket_that_join_would_accept() {
    let cache_home = temporary_cache_home();
    let store = LocalRendezvous::at(rendezvous_directory(&cache_home));
    let ticket = minted_ticket();
    let entry = store.publish(&ticket).expect("ticket should publish");

    for args in [vec!["ticket"], vec!["ticket", entry.code()]] {
        let output = run_with_cache(&cache_home, &args);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
        let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
        // stdout carries the ticket alone, so `p2pmux ticket | pbcopy` is directly pasteable.
        assert_eq!(
            JoinTicket::from_str(stdout.trim()).expect("printed ticket should parse"),
            ticket
        );
        assert!(stderr.contains("TRUST WARNING"));
        assert!(!stdout.contains("TRUST WARNING"));
    }

    drop(entry);
    fs::remove_dir_all(cache_home).expect("temporary cache should remove");
}

#[test]
fn ticket_reports_an_unresolvable_code_without_echoing_it() {
    let cache_home = temporary_cache_home();
    let unknown = "ABCDEFGHJK";

    let cases = [
        (vec!["ticket"], "no session was created on this Mac"),
        (vec!["ticket", unknown], "join code was not found"),
        (vec!["ticket", "not-a-code"], "invalid ticket format"),
    ];
    for (args, expected) in cases {
        let output = run_with_cache(&cache_home, &args);

        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
        assert!(stderr.contains(expected), "expected {expected} in {stderr}");
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains(unknown));
        assert!(!stderr.contains("not-a-code"));
    }
}

#[test]
fn ticket_asks_for_a_code_when_several_sessions_are_live() {
    let cache_home = temporary_cache_home();
    let store = LocalRendezvous::at(rendezvous_directory(&cache_home));
    let ticket = minted_ticket();
    let first = store.publish(&ticket).expect("first should publish");
    let second = store.publish(&ticket).expect("second should publish");

    let output = run_with_cache(&cache_home, &["ticket"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("several sessions are live"));
    assert!(output.stdout.is_empty());

    drop(first);
    drop(second);
    fs::remove_dir_all(cache_home).expect("temporary cache should remove");
}

/// Run `p2pmux notify` with a hook payload on stdin and the pane environment a
/// hosted PTY would have provided.
fn run_notify(env: &[(&str, &str)], args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("p2pmux binary should run");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("hook payload should write");
    child.wait_with_output().expect("notify should exit")
}

#[test]
fn notify_reports_a_blocked_agent_to_the_pane_socket() {
    let directory = temporary_cache_home();
    fs::create_dir_all(&directory).expect("temporary directory");
    let socket = directory.join("node.sock");
    let listener = UnixListener::bind(&socket).expect("socket should bind");

    let accepted = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("producer should connect");
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("producer should write one line");
        line
    });

    // A turn that ends by asking a question: Claude fires `Stop`, which would
    // otherwise show as a finished agent nobody needs to look at.
    let output = run_notify(
        &[
            ("P2PMUX_PANE_ID", "7"),
            ("P2PMUX_SOCK", socket.to_str().expect("utf-8 path")),
        ],
        &["notify", "claude", "--status", "done"],
        r#"{"hook_event_name":"Stop","last_assistant_message":"Done. Should I push?","cwd":"/repo"}"#,
    );
    assert!(output.status.success(), "a hook must never fail its agent");

    let line = accepted.join().expect("listener thread");
    let message: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(message["type"], "agent_status");
    assert_eq!(message["pane_id"], 7);
    assert_eq!(message["kind"], "claude");
    assert_eq!(message["status"], "pending");
    assert_eq!(message["cwd"], "/repo");
    // The assistant's message decided the status but must not ride along: a
    // session is shared with every member.
    assert!(message.get("msg").is_none());
    assert!(!line.contains("Should I push"));

    fs::remove_dir_all(&directory).expect("temporary directory should remove");
}

#[test]
fn notify_is_a_silent_no_op_outside_a_pane() {
    // Registered globally in a user's Claude config, this runs everywhere. A
    // non-zero exit or any stderr would surface as a hook failure in sessions
    // that have nothing to do with p2pmux.
    let output = run_notify(
        &[],
        &["notify", "claude", "--status", "running"],
        r#"{"hook_event_name":"PreToolUse"}"#,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A pane id with no socket, and a socket nothing is listening on, are the
    // same silent no-op rather than a hang or an error.
    let directory = temporary_cache_home();
    fs::create_dir_all(&directory).expect("temporary directory");
    let output = run_notify(
        &[
            ("P2PMUX_PANE_ID", "3"),
            (
                "P2PMUX_SOCK",
                directory.join("absent.sock").to_str().expect("utf-8 path"),
            ),
        ],
        &["notify", "claude", "--status", "running"],
        r#"{"hook_event_name":"PreToolUse"}"#,
    );
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    fs::remove_dir_all(&directory).expect("temporary directory should remove");
}

fn minted_ticket() -> JoinTicket {
    JoinTicket::mint(
        EndpointAddr::new(SecretKey::from_bytes(&[9; 32]).public())
            .with_ip_addr(SocketAddr::from(([127, 0, 0, 1], 4242))),
    )
    .expect("ticket should mint")
}

/// Mirror the layout `LocalRendezvous::for_current_user` builds under `XDG_CACHE_HOME`.
fn rendezvous_directory(cache_home: &Path) -> PathBuf {
    cache_home.join("p2pmux").join("rendezvous")
}

static TEMPORARY_CACHE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_cache_home() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let sequence = TEMPORARY_CACHE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "p2pmux-cli-test-{}-{nonce}-{sequence}",
        std::process::id()
    ))
}
