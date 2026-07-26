use std::{
    any::Any,
    io::{BufReader, Read, Write},
    os::unix::net::UnixStream,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use p2pmux::{
    client,
    local_ipc::{ClientMessage, NodeMessage},
    node::{NodeBootstrap, NodeBootstrapKind, write_bootstrap},
    session_store::{SessionDescriptor, SessionRole, SessionStore, generate_id},
};

struct NodeChild {
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_reader: Option<thread::JoinHandle<std::io::Result<()>>>,
}

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(8);

impl Drop for NodeChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.join_stderr_reader();
    }
}

impl NodeChild {
    fn spawn(bootstrap_path: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_p2pmux"))
            .arg("__node")
            .arg("--bootstrap")
            .arg(bootstrap_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let captured_stderr = Arc::clone(&stderr);
        let mut child_stderr = child.stderr.take().unwrap();
        let stderr_reader = thread::spawn(move || {
            let mut output = Vec::new();
            child_stderr.read_to_end(&mut output)?;
            captured_stderr.lock().unwrap().extend(output);
            Ok(())
        });
        Self {
            child,
            stderr,
            stderr_reader: Some(stderr_reader),
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn failure_context(&mut self) -> String {
        let status = self.child.try_wait().ok().flatten();
        if status.is_some() {
            self.join_stderr_reader();
        }
        let stderr = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
        format!("node status: {status:?}; node stderr:\n{stderr}")
    }

    fn join_stderr_reader(&mut self) {
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn detached_node_serves_real_snapshots_and_accepts_a_new_attachment() {
    let store = SessionStore::for_current_user().unwrap();
    let id = generate_id().unwrap();
    let socket_path = store.socket_path(&id).unwrap();
    let descriptor = SessionDescriptor::new(
        id,
        "amber-otter-01".into(),
        socket_path.clone(),
        1,
        SessionRole::Coordinator,
    );
    let bootstrap_path = socket_path.with_extension("bootstrap");
    write_bootstrap(
        &bootstrap_path,
        &NodeBootstrap {
            descriptor: descriptor.clone(),
            kind: NodeBootstrapKind::Create {
                display_name: "Test User".into(),
                cols: 80,
                rows: 24,
            },
        },
    )
    .unwrap();
    let mut child = NodeChild::spawn(&bootstrap_path);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_detach_resume_round_trip(&socket_path, &descriptor, &mut child);
    }));
    if let Err(payload) = result {
        panic!(
            "detach-resume node test failed: {}; {}",
            panic_message(payload),
            child.failure_context(),
        );
    }
}

fn run_detach_resume_round_trip(
    socket_path: &std::path::Path,
    descriptor: &SessionDescriptor,
    child: &mut NodeChild,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("node exited before creating its socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket_path.exists(), "node did not create its socket");

    let (mut stream, mut reader, generation) = attach(socket_path, "initial attach");
    let snapshot = receive_until_snapshot(&mut reader, "initial snapshot");
    let NodeMessage::Snapshot {
        summary,
        layout,
        screens,
        leases,
        ..
    } = snapshot
    else {
        unreachable!()
    };
    assert_eq!((summary.tabs, summary.panes, summary.hosts), (1, 1, 1));
    assert_eq!(summary.coordinator_name, "Test User");
    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.panes.len(), 1);
    assert!(
        screens
            .iter()
            .find(|frame| frame.pane_id == 1)
            .is_some_and(|frame| !frame.snapshot.is_empty())
    );
    assert!(
        leases
            .iter()
            .find(|lease| lease.pane_id == 1)
            .is_some_and(|lease| lease.ready)
    );

    // A second live client is refused while the first holds the attachment gate.
    let mut refused = UnixStream::connect(socket_path).unwrap();
    send(&mut refused, &ClientMessage::Hello { cols: 80, rows: 24 });
    let mut refused_reader = BufReader::new(refused.try_clone().unwrap());
    assert!(matches!(
        receive(&mut refused_reader, "second client rejection"),
        NodeMessage::AttachRejected { .. }
    ));

    send(
        &mut stream,
        &ClientMessage::Input {
            bytes: b"printf p2pmux-detach-roundtrip\\r".to_vec(),
        },
    );
    let updated = receive_until_snapshot(&mut reader, "snapshot after input");
    let NodeMessage::Snapshot { screens, .. } = updated else {
        unreachable!()
    };
    assert!(
        screens
            .iter()
            .find(|frame| frame.pane_id == 1)
            .is_some_and(|frame| !frame.snapshot.is_empty())
    );

    send(&mut stream, &ClientMessage::Detach { generation });
    loop {
        if matches!(receive(&mut reader, "detach acknowledgement"), NodeMessage::DetachAck { generation: ack } if ack == generation)
        {
            break;
        }
    }
    wait_for_detach(&mut reader);
    drop(reader);
    drop(stream);

    let (_stream, mut reader, _generation) = attach(socket_path, "reattach after detach");
    assert!(
        matches!(receive_until_snapshot(&mut reader, "snapshot after reattach"), NodeMessage::Snapshot { layout, screens, .. }
        if layout.panes.contains_key(&1) && screens.iter().any(|frame| frame.pane_id == 1))
    );
    // A shutdown control request must work even while this interactive attachment is live.
    client::shutdown(&descriptor).unwrap();
    drop(reader);
    drop(_stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let status = child.try_wait().unwrap();
    assert!(status.is_some(), "node did not shut down");
    assert!(status.unwrap().success(), "node exited unsuccessfully");
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!socket_path.exists(), "node did not remove its socket");
}

fn attach(socket_path: &std::path::Path, phase: &str) -> (UnixStream, BufReader<UnixStream>, u64) {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    send(&mut stream, &ClientMessage::Hello { cols: 80, rows: 24 });
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let generation = match receive(&mut reader, phase) {
        NodeMessage::AttachAccepted { generation } => generation,
        message => panic!("unexpected attach response: {message:?}"),
    };
    (stream, reader, generation)
}

fn receive_until_snapshot(reader: &mut BufReader<UnixStream>, phase: &str) -> NodeMessage {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        let message = receive_until_deadline(reader, deadline, phase);
        if matches!(message, NodeMessage::Snapshot { .. }) {
            return message;
        }
        assert!(Instant::now() < deadline, "timed out waiting for snapshot");
    }
}

fn send(stream: &mut UnixStream, message: &ClientMessage) {
    let mut frame = serde_json::to_vec(message).unwrap();
    frame.push(b'\n');
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn receive(reader: &mut BufReader<UnixStream>, phase: &str) -> NodeMessage {
    receive_until_deadline(reader, Instant::now() + RECEIVE_TIMEOUT, phase)
}

fn receive_until_deadline(
    reader: &mut BufReader<UnixStream>,
    deadline: Instant,
    phase: &str,
) -> NodeMessage {
    loop {
        match client::read_message(reader) {
            Ok(Some(message)) => return message,
            Ok(None) => panic!("node closed its socket while waiting for {phase}"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                panic!("timed out waiting for node message during {phase}: {error}")
            }
            Err(error) => panic!("failed to receive node message during {phase}: {error}"),
        }
    }
}

fn wait_for_detach(reader: &mut BufReader<UnixStream>) {
    reader.get_mut().set_nonblocking(true).unwrap();
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        match client::read_message(reader) {
            Ok(None) => return,
            Ok(Some(message)) => panic!("unexpected node message while detaching: {message:?}"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                panic!("timed out waiting for node to finish detaching: {error}")
            }
            Err(error) => panic!("failed while waiting for node to detach: {error}"),
        }
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).into()
    } else {
        "non-string panic payload".into()
    }
}
