//! One bad local connection must never end a session.
//!
//! The node's socket loop is single-threaded and sits in front of every pane
//! the session hosts. It also accepts connections from anything on the machine
//! that can reach the socket: `p2pmux ls`, an agent hook, a test harness, a
//! client whose terminal was closed mid-handshake. Every one of those used to
//! be able to end the loop -- and with it the session -- through a `?` on a
//! per-connection call.
//!
//! The node here is given its own `HOME` and its own socket directory, so this
//! never sees, probes, or sweeps a session the developer is sitting in.

use std::{
    io::{BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use p2pmux::{
    client,
    local_ipc::{ClientMessage, NodeMessage},
    node::{NodeBootstrap, NodeBootstrapKind, Supervisor, Tether, write_bootstrap},
    session_store::{SessionDescriptor, SessionRole, SessionStore, generate_id},
};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(8);
/// Comfortably past the 128 a listener's backlog holds, which is the point at
/// which further connections are refused while the node is still very much
/// alive.
const STORM: usize = 400;

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
    /// A node of our own, in a sandbox of our own.
    ///
    /// The path stays short because a unix socket address has to: `sockaddr_un`
    /// is 104 bytes on macOS, and a temp directory under the target directory
    /// would not fit.
    fn start(name: &str) -> Self {
        Self::start_with(name, Tether::Detached, None)
    }

    fn start_with(name: &str, tether: Tether, supervisor: Option<Supervisor>) -> Self {
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
                tether,
                // Untethered: this node stands in for one a person started.
                supervisor,
            },
        )
        .unwrap();
        let node = Command::new(env!("CARGO_BIN_EXE_p2pmux"))
            .arg("__node")
            .arg("--bootstrap")
            .arg(&bootstrap)
            .env("HOME", root.join("home"))
            // A pane of this node would otherwise inherit the pane and socket
            // of whatever session the developer ran `cargo test` from.
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
}

#[test]
fn an_unattached_interactive_node_stops_with_its_launcher() {
    let mut launcher = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let supervisor = Supervisor {
        pid: launcher.id(),
        started_at: p2pmux::agent_detect::process_start_time(launcher.id()).unwrap(),
    };
    let mut fixture = Fixture::start_with(
        "until-first-attach",
        Tether::UntilFirstAttach,
        Some(supervisor),
    );

    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    while fixture.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !fixture.running(),
        "the unattached node outlived its launcher"
    );
    assert!(
        !fixture.socket.exists(),
        "the stopped node left its socket behind"
    );
}

#[test]
fn a_detached_node_outlives_its_launcher_before_any_attachment() {
    let mut launcher = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let supervisor = Supervisor {
        pid: launcher.id(),
        started_at: p2pmux::agent_detect::process_start_time(launcher.id()).unwrap(),
    };
    let mut fixture = Fixture::start_with("detached", Tether::Detached, Some(supervisor));

    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    while Instant::now() < deadline {
        assert!(fixture.running(), "the detached node stopped with its launcher");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fixture.running(), "the detached node stopped with its launcher");
}

#[test]
fn a_lost_accepted_client_stops_its_node() {
    let mut fixture = Fixture::start("lost-accepted-client");
    let (stream, mut reader, _) = attach(&fixture.socket);
    receive_until_snapshot(&mut reader);
    drop(reader);
    drop(stream);

    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    while fixture.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !fixture.running(),
        "the node outlived an accepted client that closed without detaching"
    );
    assert!(
        !fixture.socket.exists(),
        "the stopped node left its socket behind"
    );
}

#[test]
fn a_session_outlives_every_kind_of_bad_local_client() {
    let mut fixture = Fixture::start("rude");

    // The client the user is sitting in front of.
    let (mut stream, mut reader, _) = attach(&fixture.socket);
    receive_until_snapshot(&mut reader);

    // Enough connections to fill the listener's backlog and have the next ones
    // refused -- what any p2pmux command on the machine does to a busy node.
    let storm = (0..STORM)
        .filter_map(|_| UnixStream::connect(&fixture.socket).ok())
        .collect::<Vec<_>>();
    assert!(fixture.running(), "a connection storm ended the node");
    drop(storm);
    assert!(fixture.running(), "the storm hanging up ended the node");

    // A client that says hello and is gone before the node can configure the
    // connection. macOS answers EINVAL to `setsockopt` on a socket whose peer
    // has left, which used to leave the socket loop through a `?`.
    for _ in 0..100 {
        if let Ok(mut gone) = UnixStream::connect(&fixture.socket) {
            let _ = gone.write_all(b"{\"type\":\"hello\",\"cols\":80,\"rows\":24}\n");
            let _ = gone.flush();
        }
    }
    assert!(fixture.running(), "a client that hung up ended the node");

    // The same, one step later: a client that leaves between the node
    // accepting its attachment and the node configuring the socket it was
    // accepted on. That is the window in which every `setsockopt` on the
    // connection starts answering EINVAL.
    for _ in 0..100 {
        if let Ok(mut leaving) = UnixStream::connect(&fixture.socket) {
            let _ = leaving.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = leaving.write_all(b"{\"type\":\"hello\",\"cols\":80,\"rows\":24}\n");
            let _ = leaving.flush();
            let mut accepted = String::new();
            let _ = std::io::BufRead::read_line(&mut BufReader::new(&leaving), &mut accepted);
        }
    }
    assert!(
        fixture.running(),
        "a client that left mid-handshake ended the node"
    );

    // Nonsense, a truncated frame, and a connection that says nothing at all.
    for payload in [&b"not json at all\n"[..], &b"{\"type\":\"hel"[..], &b""[..]] {
        for _ in 0..20 {
            if let Ok(mut rude) = UnixStream::connect(&fixture.socket) {
                let _ = rude.write_all(payload);
            }
        }
    }
    assert!(fixture.running(), "a malformed frame ended the node");

    // The session is not merely alive but still serving the client that was
    // attached before any of it started.
    send(
        &mut stream,
        &ClientMessage::Input {
            bytes: b"printf p2pmux-survived\r".to_vec(),
            pane_id: Some(1),
            perf_id: None,
        },
    );
    receive_until_screens(&mut reader);
    assert!(fixture.running());
}

/// The shape that ends sessions on a developer's machine: a p2pmux started with
/// a sandbox `HOME` -- every e2e scenario, every probe script -- sweeping the
/// socket directory, which `HOME` does not move. It has no record of the
/// session somebody is working in, and the socket refuses its connection
/// because the node's backlog is full, so it used to unlink it and leave a
/// running node nobody could reach.
#[test]
fn a_store_in_another_home_does_not_sweep_a_live_nodes_socket() {
    let mut fixture = Fixture::start("sweep");
    let (_stream, mut reader, _) = attach(&fixture.socket);
    receive_until_snapshot(&mut reader);

    // What the sweep falls back on for a socket that answers nothing and that no
    // record it can see claims. Nothing else can tell it this socket belongs to a
    // session somebody is working in: the record is in another `HOME`, and a node
    // whose backlog is full refuses connections exactly as a dead one does.
    // Asserted against a node that is genuinely running, since reading a live
    // process's command line is the half a unit test cannot fake.
    assert!(
        p2pmux::agent_detect::node_socket_paths().contains(&fixture.socket),
        "a running node's socket was not traced back to it"
    );

    // A store that has never heard of this session, pointed at the directory
    // every p2pmux on the machine shares.
    let elsewhere = SessionStore::at(
        fixture.root.join("other-home"),
        fixture
            .socket
            .parent()
            .expect("socket directory")
            .to_owned(),
    );
    assert!(
        elsewhere.list_live().unwrap().is_empty(),
        "the sandbox store should know of no sessions"
    );

    assert!(
        fixture.socket.exists(),
        "a live node's socket was swept by a store in another HOME"
    );
    assert!(fixture.running(), "and its node is still running");
}

fn attach(socket: &std::path::Path) -> (UnixStream, BufReader<UnixStream>, u64) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    send(&mut stream, &ClientMessage::Hello { cols: 80, rows: 24 });
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let generation = match receive(&mut reader) {
        NodeMessage::AttachAccepted { generation, .. } => generation,
        message => panic!("unexpected attach response: {message:?}"),
    };
    (stream, reader, generation)
}

fn send(stream: &mut UnixStream, message: &ClientMessage) {
    let mut frame = serde_json::to_vec(message).unwrap();
    frame.push(b'\n');
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn receive(reader: &mut BufReader<UnixStream>) -> NodeMessage {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        match client::read_message(reader) {
            Ok(Some(message)) => return message,
            Ok(None) => panic!("the node closed its socket"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("reading from the node failed: {error}"),
        }
        assert!(Instant::now() < deadline, "the node said nothing in time");
    }
}

fn receive_until_snapshot(reader: &mut BufReader<UnixStream>) -> NodeMessage {
    receive_until(reader, "snapshot", |message| {
        matches!(message, NodeMessage::Snapshot { .. })
    })
}

/// A pane drawing something is the node doing the work a session is for.
fn receive_until_screens(reader: &mut BufReader<UnixStream>) -> NodeMessage {
    receive_until(reader, "screen update", |message| {
        matches!(message, NodeMessage::Screens { .. })
    })
}

fn receive_until(
    reader: &mut BufReader<UnixStream>,
    wanted: &str,
    matches: impl Fn(&NodeMessage) -> bool,
) -> NodeMessage {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        let message = receive(reader);
        if matches(&message) {
            return message;
        }
        assert!(Instant::now() < deadline, "no {wanted} arrived");
    }
}
