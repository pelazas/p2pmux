use base64::Engine as _;
use p2pmux::{
    layout::LayoutSnapshot,
    local_ipc::{
        AgentOverlaySnapshotRow, AttachmentGate, ClientMessage, NodeMessage, PaneLeaseSnapshot,
        PaneScreenSnapshot, ScreenUpdate, SessionSummary,
    },
};

#[test]
fn protocol_messages_are_tagged_and_attachment_is_generation_safe() {
    let message = NodeMessage::Snapshot {
        room_name: "lisbon".into(),
        role: "coordinator".into(),
        summary: SessionSummary::default(),
        layout: Box::new(LayoutSnapshot {
            revision: 0,
            members: vec![],
            tabs: vec![],
            panes: Default::default(),
        }),
        screens: Default::default(),
        leases: Default::default(),
        rosters: vec![],
        local_peer_id: vec![],
        tab_id: 2,
        pane_id: 3,
        join_code: None,
    };
    assert_eq!(serde_json::to_value(message).unwrap()["type"], "snapshot");
    assert_eq!(
        serde_json::to_value(ClientMessage::Detach { generation: 7 }).unwrap()["generation"],
        7
    );
    let gate = AttachmentGate::default();
    let old = gate.attach().unwrap();
    assert!(gate.detach(old));
    let current = gate.attach().unwrap();
    assert!(!gate.detach(old));
    assert!(gate.detach(current));
}

#[test]
fn incremental_node_messages_round_trip_without_unchanged_screens() {
    let layout = LayoutSnapshot {
        revision: 0,
        members: vec![],
        tabs: vec![],
        panes: Default::default(),
    };
    let messages = vec![
        NodeMessage::Screens {
            screens: vec![
                PaneScreenSnapshot {
                    pane_id: 1,
                    state: ScreenUpdate::Snapshot {
                        sequence: 1,
                        snapshot: vec![1],
                        kitty_keyboard_active: false,
                    },
                    history_len: 4,
                    history_end: 12,
                },
                PaneScreenSnapshot {
                    pane_id: 2,
                    state: ScreenUpdate::Delta {
                        base_sequence: 1,
                        sequence: 2,
                        delta: vec![2],
                        kitty_keyboard_active: true,
                    },
                    history_len: 0,
                    history_end: 0,
                },
            ],
        },
        NodeMessage::Layout {
            layout: Box::new(layout),
        },
        NodeMessage::Leases {
            leases: vec![PaneLeaseSnapshot::default()],
        },
        NodeMessage::Rosters {
            rosters: vec![AgentOverlaySnapshotRow {
                pane_id: 1,
                kind: "agent".into(),
                cwd: "/tmp".into(),
                state: 1,
                working_since_unix_ms: 2,
                host: "host".into(),
                controller: "controller".into(),
            }],
        },
        NodeMessage::Focus {
            tab_id: 2,
            pane_id: 3,
        },
        NodeMessage::ScrollbackWindow {
            pane_id: 1,
            request_id: 9,
            sequence: 3,
            grid_rows: 24,
            grid_cols: 80,
            total_rows: 12,
            offset: 0,
            rows: vec![base64::engine::general_purpose::STANDARD.encode(b"row")],
        },
    ];
    for message in messages {
        let encoded = serde_json::to_vec(&message).unwrap();
        let decoded: NodeMessage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, message);
        if let NodeMessage::Screens { screens } = decoded {
            assert!(
                screens
                    .iter()
                    .all(|frame| !matches!(frame.state, ScreenUpdate::Unchanged { .. }))
            );
        }
    }
}

#[test]
fn scrollback_query_round_trips_and_screen_updates_carry_no_rows() {
    let query = ClientMessage::ScrollbackQuery {
        pane_id: 7,
        offset: 3,
        max_rows: 64,
        request_id: 11,
    };
    let encoded = serde_json::to_value(&query).unwrap();
    assert_eq!(encoded["type"], "scrollback_query");
    assert_eq!(serde_json::from_value::<ClientMessage>(encoded).unwrap(), query);

    let screen = PaneScreenSnapshot {
        pane_id: 7,
        state: ScreenUpdate::Unchanged {
            sequence: 4,
            kitty_keyboard_active: false,
        },
        history_len: 2,
        history_end: 9,
    };
    let encoded = serde_json::to_value(screen).unwrap();
    assert!(encoded.get("local_history").is_none());
    assert!(encoded.get("rows").is_none());
}
