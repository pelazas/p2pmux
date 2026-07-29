use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
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
