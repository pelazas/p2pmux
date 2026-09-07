//! `p2pmux ctl` talks to a live node without taking the TUI seat.
//!
//! The node here is given its own `HOME` and its own socket, so these never
//! see a session the developer is sitting in.

use std::{
    io::{BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use p2pmux::{
    client,
    ctl::{self, CTL_PROTOCOL_PIN, CtlFromClient, CtlToClient},
    local_ipc::{ClientMessage, NodeMessage},
    node::{NodeBootstrap, NodeBootstrapKind, Tether, write_bootstrap},
    session_store::{SessionDescriptor, SessionRole, generate_id},
};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(8);
/// One node at a time while it opens its first PTY. Six of these in parallel
/// on Linux CI was enough for a later spawn to come back as a reservation
/// failure.
static START: Mutex<()> = Mutex::new(());

struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    node: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.node.kill();
        let _ = self.node.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn start(name: &str) -> Self {
        let _start = START.lock().expect("fixture start");
        let root = PathBuf::from(format!("/tmp/p2pmux-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        let socket = root.join("n.sock");
        let bootstrap = root.join("n.bootstrap");
        let descriptor = SessionDescriptor::new(
            generate_id().unwrap(),
            "lisbon".into(),
            socket.clone(),
            1,
            SessionRole::Coordinator,
        );
        write_bootstrap(
            &bootstrap,
            &NodeBootstrap {
                descriptor,
                kind: NodeBootstrapKind::Create {
                    display_name: "Test User".into(),
                    cols: 80,
                    rows: 24,
                },
                tether: Tether::Detached,
                supervisor: None,
            },
        )
        .unwrap();
        let node = Command::new(env!("CARGO_BIN_EXE_p2pmux"))
            .arg("__node")
            .arg("--bootstrap")
            .arg(&bootstrap)
            .env("HOME", root.join("home"))
            .env_remove("P2PMUX_PANE_ID")
            .env_remove("P2PMUX_SOCK")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self { root, socket, node };
        let deadline = Instant::now() + READY_TIMEOUT;
        while !fixture.socket.exists() {
            assert!(
                fixture.node.try_wait().unwrap().is_none(),
                "node exited before binding its socket"
            );
            assert!(Instant::now() < deadline, "node never bound its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture
    }

    fn running(&mut self) -> bool {
        self.node.try_wait().unwrap().is_none()
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn wait_until_ctl_answers(&self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if ping_machines(&self.socket) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "node never answered ctl machines"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn ping_machines(socket: &Path) -> bool {
    let Ok(stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(mut writer) = stream.try_clone() else {
        return false;
    };
    let mut reader = BufReader::new(stream);
    if ctl::write_json(
        &mut writer,
        &CtlFromClient::CtlHello {
            pin: CTL_PROTOCOL_PIN,
        },
    )
    .is_err()
    {
        return false;
    }
    match ctl::receive_json::<CtlToClient>(&mut reader) {
        Ok(Some(CtlToClient::CtlHelloAck { pin })) if pin == CTL_PROTOCOL_PIN => {}
        _ => return false,
    }
    if ctl::write_json(&mut writer, &CtlFromClient::Machines).is_err() {
        return false;
    }
    matches!(
        ctl::receive_json::<CtlToClient>(&mut reader),
        Ok(Some(CtlToClient::Machines { .. }))
    )
}

fn ctl_cli(fixture: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .env("HOME", fixture.home())
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("p2pmux binary should run")
}

fn connect_ctl(socket: &Path) -> (UnixStream, BufReader<UnixStream>) {
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let writer = stream.try_clone().unwrap();
    (writer, BufReader::new(stream))
}

fn hello(socket: &Path, pin: u32) -> (UnixStream, BufReader<UnixStream>, CtlToClient) {
    let (mut writer, mut reader) = connect_ctl(socket);
    ctl::write_json(&mut writer, &CtlFromClient::CtlHello { pin }).unwrap();
    let reply = ctl::receive_json(&mut reader)
        .unwrap()
        .expect("node should answer hello");
    (writer, reader, reply)
}

fn attach(socket: &Path) -> (UnixStream, BufReader<UnixStream>, u64) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    send_client(&mut stream, &ClientMessage::Hello { cols: 80, rows: 24 });
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let generation = match receive_node(&mut reader) {
        NodeMessage::AttachAccepted { generation, .. } => generation,
        message => panic!("unexpected attach response: {message:?}"),
    };
    (stream, reader, generation)
}

fn send_client(stream: &mut UnixStream, message: &ClientMessage) {
    let mut frame = serde_json::to_vec(message).unwrap();
    frame.push(b'\n');
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn receive_node(reader: &mut BufReader<UnixStream>) -> NodeMessage {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        match client::read_message(reader) {
            Ok(Some(message)) => return message,
            Ok(None) => panic!("the node closed its socket"),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("reading from the node failed: {error}"),
        }
        assert!(Instant::now() < deadline, "the node said nothing in time");
    }
}

#[test]
fn a_wrong_ctl_pin_is_refused_and_does_not_take_the_tui_seat() {
    let mut fixture = Fixture::start("ctl-pin");
    fixture.wait_until_ctl_answers();

    let (_writer, _reader, reply) = hello(&fixture.socket, CTL_PROTOCOL_PIN + 1);
    match reply {
        CtlToClient::CtlHelloRejected { ours, theirs } => {
            assert_eq!(ours, CTL_PROTOCOL_PIN);
            assert_eq!(theirs, CTL_PROTOCOL_PIN + 1);
        }
        other => panic!("expected a pin refusal, got {other:?}"),
    }

    let (_stream, mut reader, _) = attach(&fixture.socket);
    let mut saw_snapshot = false;
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    while Instant::now() < deadline {
        if let NodeMessage::Snapshot { .. } = receive_node(&mut reader) {
            saw_snapshot = true;
            break;
        }
    }
    assert!(saw_snapshot, "the TUI should still be able to attach");
    assert!(
        fixture.running(),
        "a refused ctl client must not end the node"
    );
}

#[test]
fn ctl_and_the_tui_share_a_node() {
    let mut fixture = Fixture::start("ctl-tui");
    fixture.wait_until_ctl_answers();
    let (_stream, _reader, _) = attach(&fixture.socket);

    let output = ctl_cli(&fixture, &["ctl", "machines"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ctl should work while a TUI is attached: {stderr}"
    );
    assert!(stdout.contains("machines"), "{stdout}");
    assert!(
        stdout.contains("Test User"),
        "this machine should be listed: {stdout}"
    );
    assert!(fixture.running());
}

#[test]
fn spawn_send_and_focus_drive_a_local_pane() {
    let mut fixture = Fixture::start("ctl-spawn");
    fixture.wait_until_ctl_answers();

    let nested = ctl_cli(
        &fixture,
        &["ctl", "spawn", "--machine", "Test User", "--", "p2pmux"],
    );
    let nested_err = String::from_utf8_lossy(&nested.stderr);
    assert!(!nested.status.success(), "{nested_err}");
    assert!(
        nested_err.contains("p2pmux does not nest p2pmux"),
        "{nested_err}"
    );

    let missing = ctl_cli(
        &fixture,
        &["ctl", "spawn", "--machine", "nobody", "--", "sleep", "30"],
    );
    let missing_err = String::from_utf8_lossy(&missing.stderr);
    assert!(!missing.status.success(), "{missing_err}");
    assert!(
        missing_err.contains("no paired machine named nobody"),
        "{missing_err}"
    );

    let first = ctl_cli(
        &fixture,
        &[
            "ctl",
            "spawn",
            "--machine",
            "Test User",
            "--",
            "sleep",
            "120",
        ],
    );
    let first_out = String::from_utf8_lossy(&first.stdout);
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert!(first.status.success(), "spawn failed: {first_err}");
    let first_json: serde_json::Value = serde_json::from_str(first_out.trim()).unwrap();
    let pane_id = first_json["pane_id"].as_u64().expect("pane_id");
    assert_eq!(first_json["reused"], false, "{first_out}");
    assert_eq!(first_json["visible_to_guests"], false, "{first_out}");

    let second = ctl_cli(
        &fixture,
        &[
            "ctl",
            "spawn",
            "--machine",
            "Test User",
            "--",
            "sleep",
            "120",
        ],
    );
    let second_out = String::from_utf8_lossy(&second.stdout);
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "reuse spawn failed: {second_err}");
    let second_json: serde_json::Value = serde_json::from_str(second_out.trim()).unwrap();
    assert_eq!(second_json["pane_id"], pane_id, "{second_out}");
    assert_eq!(second_json["reused"], true, "{second_out}");

    let third = ctl_cli(
        &fixture,
        &[
            "ctl",
            "spawn",
            "--new",
            "--machine",
            "Test User",
            "--",
            "sleep",
            "120",
        ],
    );
    let third_out = String::from_utf8_lossy(&third.stdout);
    let third_err = String::from_utf8_lossy(&third.stderr);
    assert!(third.status.success(), "new spawn failed: {third_err}");
    let third_json: serde_json::Value = serde_json::from_str(third_out.trim()).unwrap();
    let new_pane = third_json["pane_id"].as_u64().expect("pane_id");
    assert_ne!(new_pane, pane_id, "{third_out}");
    assert_eq!(third_json["reused"], false, "{third_out}");

    let send = ctl_cli(&fixture, &["ctl", "send", &pane_id.to_string(), "x"]);
    let send_err = String::from_utf8_lossy(&send.stderr);
    assert!(send.status.success(), "send failed: {send_err}");

    let focus = ctl_cli(&fixture, &["ctl", "focus", &pane_id.to_string()]);
    let focus_err = String::from_utf8_lossy(&focus.stderr);
    assert!(focus.status.success(), "focus failed: {focus_err}");

    let agents = ctl_cli(&fixture, &["ctl", "agents"]);
    let agents_out = String::from_utf8_lossy(&agents.stdout);
    assert!(
        agents.status.success(),
        "{}",
        String::from_utf8_lossy(&agents.stderr)
    );
    assert!(agents_out.contains("agents"), "{agents_out}");

    assert!(fixture.running());
}

#[test]
fn events_stream_without_a_tui() {
    let mut fixture = Fixture::start("ctl-events");
    fixture.wait_until_ctl_answers();

    // The node already hosts pane 1. Opening another PTY here raced the other
    // ctl tests on Linux CI and came back as a reservation failure.
    let pane_id = 1;

    let (mut writer, mut reader, reply) = hello(&fixture.socket, CTL_PROTOCOL_PIN);
    assert!(
        matches!(reply, CtlToClient::CtlHelloAck { pin } if pin == CTL_PROTOCOL_PIN),
        "{reply:?}"
    );
    ctl::write_json(&mut writer, &CtlFromClient::Events).unwrap();

    let mut status = UnixStream::connect(&fixture.socket).unwrap();
    send_client(
        &mut status,
        &ClientMessage::AgentStatus {
            pane_id,
            kind: "claude".into(),
            status: "pending".into(),
            cwd: "/tmp".into(),
            message: "shall I continue?".into(),
        },
    );
    drop(status);

    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    let mut saw_needs_you = false;
    while Instant::now() < deadline {
        match ctl::receive_json::<CtlToClient>(&mut reader) {
            Ok(Some(CtlToClient::Event { state, message, .. })) => {
                if state == "needs_you" {
                    assert_eq!(message, "shall I continue?");
                    saw_needs_you = true;
                    break;
                }
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("events stream failed: {error}"),
        }
    }
    assert!(saw_needs_you, "events should report the local needs_you");
    assert!(fixture.running());
}

#[test]
fn a_bad_ctl_client_does_not_end_the_session() {
    let mut fixture = Fixture::start("ctl-bad");
    fixture.wait_until_ctl_answers();

    let mut stream = UnixStream::connect(&fixture.socket).unwrap();
    stream.write_all(b"{{{{ not json\n").unwrap();
    stream.flush().unwrap();
    drop(stream);

    fixture.wait_until_ctl_answers();
    assert!(fixture.running());
}

#[test]
fn an_old_node_is_reported_when_hello_is_ignored() {
    // A first line the node does not understand is dropped the same way an
    // older build would drop `ctl_hello`. The client must say so, not hang.
    let mut fixture = Fixture::start("ctl-old");
    fixture.wait_until_ctl_answers();

    let (mut writer, mut reader) = connect_ctl(&fixture.socket);
    writer.write_all(b"{\"type\":\"nope\"}\n").unwrap();
    writer.flush().unwrap();
    let started = Instant::now();
    let reply = ctl::receive_json::<CtlToClient>(&mut reader);
    match reply {
        Ok(None) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
        other => panic!("an unknown first line must not look like ctl: {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the client should notice quickly that this is not a ctl node"
    );
    assert!(fixture.running());
}
