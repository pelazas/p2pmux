use std::{
    io::{BufReader, Write},
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use p2pmux::{
    client,
    local_ipc::{ClientMessage, NodeMessage},
    node::{NodeBootstrap, NodeBootstrapKind, write_bootstrap},
    session_store::{SessionDescriptor, SessionRole, SessionStore, generate_id},
};

struct NodeChild(Child);

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(8);

impl Drop for NodeChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
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
    let mut child = NodeChild(
        Command::new(env!("CARGO_BIN_EXE_p2pmux"))
            .arg("__node")
            .arg("--bootstrap")
            .arg(&bootstrap_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket_path.exists(), "node did not create its socket");

    let (mut stream, mut reader, generation) = attach(&socket_path);
    let snapshot = receive_until_snapshot(&mut reader);
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
    let mut refused = UnixStream::connect(&socket_path).unwrap();
    send(&mut refused, &ClientMessage::Hello { cols: 80, rows: 24 });
    let mut refused_reader = BufReader::new(refused.try_clone().unwrap());
    assert!(matches!(
        receive(&mut refused_reader),
        NodeMessage::AttachRejected { .. }
    ));

    send(
        &mut stream,
        &ClientMessage::Input {
            bytes: b"printf p2pmux-detach-roundtrip\\r".to_vec(),
        },
    );
    let updated = receive_until_snapshot(&mut reader);
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
        if matches!(receive(&mut reader), NodeMessage::DetachAck { generation: ack } if ack == generation)
        {
            break;
        }
    }
    drop(reader);
    drop(stream);

    let (_stream, mut reader, _generation) = attach(&socket_path);
    assert!(
        matches!(receive_until_snapshot(&mut reader), NodeMessage::Snapshot { layout, screens, .. }
        if layout.panes.contains_key(&1) && screens.iter().any(|frame| frame.pane_id == 1))
    );
    // A shutdown control request must work even while this interactive attachment is live.
    client::shutdown(&descriptor).unwrap();
    drop(reader);
    drop(_stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let status = child.0.try_wait().unwrap();
    assert!(status.is_some(), "node did not shut down");
    assert!(status.unwrap().success(), "node exited unsuccessfully");
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!socket_path.exists(), "node did not remove its socket");
}

fn attach(socket_path: &std::path::Path) -> (UnixStream, BufReader<UnixStream>, u64) {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    send(&mut stream, &ClientMessage::Hello { cols: 80, rows: 24 });
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let generation = match receive(&mut reader) {
        NodeMessage::AttachAccepted { generation } => generation,
        message => panic!("unexpected attach response: {message:?}"),
    };
    (stream, reader, generation)
}

fn receive_until_snapshot(reader: &mut BufReader<UnixStream>) -> NodeMessage {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        let message = receive_until_deadline(reader, deadline);
        if matches!(message, NodeMessage::Snapshot { .. }) {
            return message;
        }
        assert!(Instant::now() < deadline, "timed out waiting for snapshot");
    }
}

fn send(stream: &mut UnixStream, message: &ClientMessage) {
    serde_json::to_writer(&mut *stream, message).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

fn receive(reader: &mut BufReader<UnixStream>) -> NodeMessage {
    receive_until_deadline(reader, Instant::now() + RECEIVE_TIMEOUT)
}

fn receive_until_deadline(reader: &mut BufReader<UnixStream>, deadline: Instant) -> NodeMessage {
    loop {
        match client::read_message(reader) {
            Ok(Some(message)) => return message,
            Ok(None) => panic!("node closed its socket"),
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
                panic!("timed out waiting for node message: {error}")
            }
            Err(error) => panic!("failed to receive node message: {error}"),
        }
    }
}
