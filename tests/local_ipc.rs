use p2pmux::{
    layout::LayoutSnapshot,
    local_ipc::{AttachmentGate, ClientMessage, NodeMessage, SessionSummary},
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
        rosters: serde_json::json!({}),
        tab_id: 2,
        pane_id: 3,
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
