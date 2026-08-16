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
use p2pmux::{
    session_store::{SessionDescriptor, SessionRole, SessionStore},
    ticket::JoinTicket,
};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .output()
        .expect("p2pmux binary should run")
}

/// Run against an isolated session store so tests never read the user's live sessions.
///
/// `XDG_STATE_HOME` outranks `HOME` where it is honoured, so a developer who has
/// one set would otherwise send the child straight back to their own sessions.
/// `XDG_CONFIG_HOME` outranks it the same way for `pairing.toml` and the display
/// name, which is how a machine that is genuinely in a fleet would leak into a
/// test asserting that this one is not.
fn run_with_home(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
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
    // Neither a ticket nor a code by shape, so it is refused without a network round trip.
    assert!(stderr.contains("that is not a join code"));
    assert!(!stdout.contains("not-a-ticket"));
    assert!(!stderr.contains("not-a-ticket"));
}

#[test]
fn join_rejects_a_malformed_ticket_without_echoing_it() {
    let output = run(&["join", "p2pmux-v3:$$$notbase64$$$"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("invalid ticket format"));
    assert!(!stderr.contains("notbase64"));
}

#[test]
fn join_reports_an_unreachable_rendezvous_without_echoing_the_code() {
    // A code-shaped argument is exchanged for a ticket at the rendezvous, so this is the one
    // join path that can fail on the network. The code is a credential and the derived index
    // is one hash away from it, so neither may appear in what the user sees.
    let code = "4KP7Q-M2XRW";
    let output = Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(["join", code])
        .env("P2PMUX_RENDEZVOUS_URL", "https://127.0.0.1:1")
        .output()
        .expect("p2pmux binary should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(
        stderr.contains("rendezvous service is unreachable"),
        "unexpected stderr: {stderr}"
    );
    assert!(!stderr.contains(code));
    assert!(!stderr.contains("4KP7QM2XRW"));
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
    assert!(stdout.contains("join code or a reusable shared-session ticket"));
    assert!(stdout.contains("ticket"));
    assert!(stdout.contains("full reusable join ticket"));
}

/// The listing is spelled `list`, and `ls` still reaches it.
///
/// Renaming the command a user types every day is only worth doing if the old
/// spelling keeps working: `ls` is in scripts, in shell history and in three
/// releases' worth of documentation, and a rename that breaks those buys a
/// nicer name at the price of every one of them.
#[test]
fn list_and_its_ls_alias_print_the_same_sessions() {
    let session = FakeSession::hosted("lisbon");

    let long = run_with_home(session.home(), &["list"]);
    let short = run_with_home(session.home(), &["ls"]);

    assert!(long.status.success());
    assert!(short.status.success());
    let listed = String::from_utf8(long.stdout).expect("stdout should be UTF-8");
    assert!(listed.contains("lisbon"), "{listed}");
    assert!(listed.contains("coordinator"), "{listed}");
    assert_eq!(
        listed,
        String::from_utf8(short.stdout).expect("stdout should be UTF-8")
    );
}

/// `list` is the one the help text teaches, so the rename reaches the place a
/// user goes to find out what the commands are.
#[test]
fn help_names_the_listing_command_list() {
    let output = run(&["--help"]);

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("list"), "{stdout}");
    assert!(stdout.contains("List the live sessions"), "{stdout}");
}

#[test]
fn ticket_prints_a_pasteable_ticket_that_join_would_accept() {
    let session = FakeSession::hosted("lisbon");
    let ticket = session.ticket.clone().expect("hosted sessions carry one");

    for args in [vec!["ticket"], vec!["ticket", "lisbon"]] {
        let output = run_with_home(session.home(), &args);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
        let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
        // stdout carries the ticket alone, so `p2pmux ticket | pbcopy` is directly pasteable.
        assert_eq!(
            JoinTicket::from_str(stdout.trim()).expect("printed ticket should parse"),
            JoinTicket::from_str(&ticket).expect("fixture ticket should parse")
        );
        assert!(stderr.contains("TRUST WARNING"));
        assert!(!stdout.contains("TRUST WARNING"));
    }
}

#[test]
fn ticket_reports_an_unknown_session_without_echoing_it() {
    let empty = FakeSession::empty();
    let unknown = "atlantis";
    for (args, expected) in [
        (vec!["ticket"], "no session was created on this machine"),
        (vec!["ticket", unknown], "no live session by that name"),
    ] {
        let output = run_with_home(empty.home(), &args);

        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
        assert!(stderr.contains(expected), "expected {expected} in {stderr}");
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains(unknown));
    }
}

#[test]
fn ticket_refuses_a_session_this_machine_only_joined() {
    // A member's node never minted a ticket, so there is nothing to hand out — and saying so
    // beats printing the coordinator's ticket, which this machine does not have either.
    let session = FakeSession::joined("oslo");

    let output = run_with_home(session.home(), &["ticket", "oslo"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("joined, not created here"));
    assert!(output.stdout.is_empty());
}

#[test]
fn ticket_asks_for_a_name_when_several_sessions_are_hosted() {
    let mut session = FakeSession::hosted("porto");
    session.add_hosted("vienna");

    let output = run_with_home(session.home(), &["ticket"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("several sessions are hosted"));
    assert!(output.stdout.is_empty());
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
    // One line of the assistant's message rides along to the node that owns the
    // pane — this socket. That is as far as it goes: the roster the node then
    // publishes to peers has no field for it. See
    // `agent_roster_entry_never_carries_the_agents_message`.
    assert_eq!(message["message"], "Done. Should I push?");

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

/// A fleet is something a pairing makes, and the error has to say so.
///
/// It used to offer "or start a session with `p2pmux`" as an alternative, which
/// is a route that does not exist: only `p2pmux pair` and the add-machine panel
/// write the fleet ticket `enroll` hands out. Somebody provisioning a VM would
/// start a session, run `enroll` again, get the same error, and have nothing
/// left to try.
#[test]
fn enroll_without_a_fleet_names_pairing_and_offers_no_other_route() {
    let session = FakeSession::hosted("lisbon");

    let output = run_with_home(session.home(), &["enroll"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("not in a fleet yet"), "{stderr}");
    assert!(stderr.contains("p2pmux pair"), "{stderr}");
    assert!(
        !stderr.contains("start a session"),
        "the error offers a route that writes no fleet ticket: {stderr}"
    );
}

fn minted_ticket() -> JoinTicket {
    JoinTicket::mint(
        EndpointAddr::new(SecretKey::from_bytes(&[9; 32]).public())
            .with_ip_addr(SocketAddr::from(([127, 0, 0, 1], 4242))),
    )
    .expect("ticket should mint")
}

/// A session store the binary will read through `HOME`, holding descriptors whose sockets
/// answer a liveness probe.
///
/// The probe responders matter: `list_live` treats a socket that does not reply `probe_ack` as a
/// dead session and deletes the record, so a fixture without them reports nothing at all.
struct FakeSession {
    home: PathBuf,
    sockets: PathBuf,
    store: SessionStore,
    served: usize,
    ticket: Option<String>,
}

impl FakeSession {
    fn empty() -> Self {
        let home = temporary_cache_home();
        // A Unix socket path has to fit in `sun_path` (104 bytes on macOS), which the descriptive
        // per-test home blows through on its own. Sockets get their own short directory.
        let sockets = std::env::temp_dir().join(format!(
            "p2ps{}-{}",
            std::process::id(),
            TEMPORARY_CACHE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = SessionStore::at(SessionStore::sessions_dir_for_home(&home), sockets.clone());
        fs::create_dir_all(&sockets).expect("socket directory");
        Self {
            home,
            sockets,
            store,
            served: 0,
            ticket: None,
        }
    }

    fn hosted(name: &str) -> Self {
        let mut value = Self::empty();
        value.ticket = Some(value.write(name, SessionRole::Coordinator));
        value
    }

    fn joined(name: &str) -> Self {
        let mut value = Self::empty();
        value.write(name, SessionRole::Member);
        value
    }

    fn add_hosted(&mut self, name: &str) {
        self.write(name, SessionRole::Coordinator);
    }

    /// Writes one descriptor and returns the ticket it carries, if any.
    fn write(&mut self, name: &str, role: SessionRole) -> String {
        let id = p2pmux::session_store::generate_id().expect("session id");
        let socket = self.sockets.join(format!("{}.sock", self.served));
        self.served += 1;
        serve_probes(UnixListener::bind(&socket).expect("session socket should bind"));
        let ticket = minted_ticket().to_string();
        let mut descriptor = SessionDescriptor::new(
            id,
            name.to_owned(),
            socket,
            std::process::id(),
            role.clone(),
        );
        if role == SessionRole::Coordinator {
            descriptor.ticket = Some(ticket.clone());
        }
        self.store
            .write(&descriptor)
            .expect("descriptor should write");
        ticket
    }

    fn home(&self) -> &Path {
        &self.home
    }
}

/// Answer `list_live`'s probe on a background thread for as long as the test binary runs.
///
/// The thread is deliberately detached: removing the socket file in `Drop` is what retires it,
/// and a blocked `accept` on an unlinked path costs nothing until the process exits.
fn serve_probes(listener: UnixListener) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.write_all(b"{\"type\":\"probe_ack\"}\n");
            let _ = stream.flush();
        }
    });
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
        let _ = fs::remove_dir_all(&self.sockets);
    }
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
