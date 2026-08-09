use p2pmux::protocol::{
    AgentRoster, AgentRosterEntry, AgentRosterState, ControlLease, CreatePane, CreateTab,
    DeletePane, DeleteTab, Delta, Envelope, Input, Join, LEDGER_HASH_BYTES, LEDGER_SIGNATURE_BYTES,
    LayoutCommit, LayoutNode, LayoutReject, LayoutRejectReason, LayoutRequest, LayoutSplit,
    LayoutState, LedgerEntry, LedgerEntryKind, MAX_AGENT_CWD_BYTES, MAX_AGENT_KIND_BYTES,
    MAX_AGENT_ROSTER_ENTRIES, MAX_DELTA_BYTES, MAX_ENDPOINT_ADDR_BYTES, MAX_ENVELOPE_BYTES,
    MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_LEDGER_PAYLOAD_BYTES, MAX_PANE_ID_BYTES,
    MAX_PEER_ID_BYTES, MAX_PRESENCE_ENTRIES, MAX_SESSION_ID_BYTES, MAX_SNAPSHOT_BYTES,
    MarkPaneExited, MemberDescriptor, MemberKind, NewPanePosition, PROTOCOL_VERSION,
    PaneDescriptor, PaneFailed, PaneGrid, PaneReady, PaneReservation, PaneSubscribe, Presence,
    PresenceRoster, ProtocolError, SessionSnapshot, SetPaneLock, SetSplitRatio, Snapshot,
    SplitAxis, TabDescriptor, TakeControl, UpdatePaneGrids, Welcome, decode_frame, encode_frame,
    envelope,
};
use prost::Message;

#[derive(Debug)]
struct ParsedField {
    field_number: u32,
    wire_type: u8,
    value: Vec<u8>,
}

/// A structurally valid ledger entry. The signatures are the right shape and nothing more:
/// `validate_envelope` checks framing, and only [`p2pmux::ledger::LedgerVerifier`] checks
/// that a signature means anything.
fn ledger_entry() -> LedgerEntry {
    LedgerEntry {
        seq: 1,
        prev_hash: vec![0; LEDGER_HASH_BYTES],
        author_peer_id: b"peer-a".to_vec(),
        kind: LedgerEntryKind::LayoutChange as i32,
        payload: b"payload".to_vec(),
        author_signature: Vec::new(),
        coordinator_signature: vec![7; LEDGER_SIGNATURE_BYTES],
        coordinator_epoch: 0,
    }
}

/// A commit whose state is valid, so only the entry can be what a case is testing.
fn commit_with(entry: LedgerEntry) -> LayoutCommit {
    LayoutCommit {
        revision: 1,
        state: Some(layout_state(leaf(1))),
        entry: Some(entry),
    }
}

fn envelope(body: envelope::Body) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: b"peer-a".to_vec(),
        body: Some(body),
    }
}

#[test]
fn protocol_version_is_v10() {
    // v10 added the coordinator epoch to the welcome and to every ledger entry. A v9 peer
    // cannot tell which coordinator sealed what it is being handed, so it must not be let in
    // rather than be left guessing.
    assert_eq!(PROTOCOL_VERSION, 10);
}

#[test]
fn welcome_session_name_round_trips_and_defaults_empty() {
    let named = envelope(envelope::Body::Welcome(Welcome {
        session_id: b"session-a".to_vec(),
        admitted_peer_id: b"peer-a".to_vec(),
        coordinator_peer_id: b"peer-host".to_vec(),
        session_name: "lisbon".to_owned(),
        coordinator_epoch: 0,
    }));
    assert_eq!(decode_frame(&encode_frame(&named).unwrap()).unwrap(), named);

    let legacy = envelope(envelope::Body::Welcome(Welcome {
        session_id: b"session-a".to_vec(),
        admitted_peer_id: b"peer-a".to_vec(),
        coordinator_peer_id: b"peer-host".to_vec(),
        session_name: String::new(),
        coordinator_epoch: 0,
    }));
    let decoded = decode_frame(&encode_frame(&legacy).unwrap()).unwrap();
    let Some(envelope::Body::Welcome(welcome)) = decoded.body else {
        panic!("expected Welcome");
    };
    assert!(welcome.session_name.is_empty());
}

#[test]
fn ratio_and_grid_actions_validate_strictly() {
    let request = LayoutRequest {
        request_id: 1,
        base_revision: 1,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: Some(SetSplitRatio {
            pane_id: 1,
            axis: Some(SplitAxis::LeftRight as i32),
            first_share_bps: 7_500,
        }),
        update_pane_grids: None,

        rename_pane: None,
        rename_tab: None,
        set_pane_lock: None,
        mark_pane_exited: None,
        author_signature: Vec::new(),
    };
    let encoded = encode_frame(&envelope(envelope::Body::LayoutRequest(request.clone())))
        .expect("valid request");
    assert_eq!(
        decode_frame(&encoded).expect("round trip"),
        envelope(envelope::Body::LayoutRequest(request))
    );

    for first_share_bps in [0, 10_000] {
        let invalid = LayoutRequest {
            set_split_ratio: Some(SetSplitRatio {
                pane_id: 1,
                axis: Some(SplitAxis::LeftRight as i32),
                first_share_bps,
            }),
            ..LayoutRequest {
                request_id: 1,
                base_revision: 1,
                create_pane: None,
                delete_pane: None,
                create_tab: None,
                delete_tab: None,
                set_split_ratio: None,
                update_pane_grids: None,
                rename_pane: None,
                rename_tab: None,
                set_pane_lock: None,
                mark_pane_exited: None,
                author_signature: Vec::new(),
            }
        };
        assert!(encode_frame(&envelope(envelope::Body::LayoutRequest(invalid))).is_err());
    }

    let grids = LayoutRequest {
        request_id: 2,
        base_revision: 1,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: None,
        update_pane_grids: Some(UpdatePaneGrids {
            panes: vec![PaneGrid {
                pane_id: 1,
                grid_rows: 24,
                grid_cols: 80,
            }],
        }),

        rename_pane: None,
        rename_tab: None,
        set_pane_lock: None,
        mark_pane_exited: None,
        author_signature: Vec::new(),
    };
    assert!(
        decode_frame(
            &encode_frame(&envelope(envelope::Body::LayoutRequest(grids))).expect("encode")
        )
        .is_ok()
    );
}

#[test]
fn pane_lock_action_round_trips_and_is_the_only_action() {
    let request = LayoutRequest {
        request_id: 1,
        base_revision: 1,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: None,
        update_pane_grids: None,
        rename_pane: None,
        rename_tab: None,
        set_pane_lock: Some(SetPaneLock {
            pane_id: 1,
            locked: true,
        }),
        mark_pane_exited: None,
        author_signature: Vec::new(),
    };
    let encoded = encode_frame(&envelope(envelope::Body::LayoutRequest(request.clone())))
        .expect("valid lock request");
    assert_eq!(
        decode_frame(&encoded).expect("round trip"),
        envelope(envelope::Body::LayoutRequest(request))
    );
}

#[test]
fn mark_pane_exited_round_trips_and_is_the_only_action() {
    let request = LayoutRequest {
        request_id: 1,
        base_revision: 1,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: None,
        update_pane_grids: None,
        rename_pane: None,
        rename_tab: None,
        set_pane_lock: None,
        mark_pane_exited: Some(MarkPaneExited { pane_id: 1 }),
        author_signature: Vec::new(),
    };
    let encoded = encode_frame(&envelope(envelope::Body::LayoutRequest(request.clone())))
        .expect("valid exited request");
    assert_eq!(
        decode_frame(&encoded).expect("round trip"),
        envelope(envelope::Body::LayoutRequest(request))
    );
}

#[test]
fn envelope_exposes_each_v1_body() {
    let messages = [
        envelope(envelope::Body::Join(Join {
            session_id: b"session-a".to_vec(),
            peer_id: b"peer-a".to_vec(),
            endpoint_addr: b"endpoint-a".to_vec(),
            display_name: String::new(),
            member_kind: Default::default(),
        })),
        envelope(envelope::Body::Welcome(Welcome {
            session_id: b"session-a".to_vec(),
            admitted_peer_id: b"peer-a".to_vec(),
            coordinator_peer_id: b"peer-host".to_vec(),
            session_name: String::new(),
            coordinator_epoch: 0,
        })),
        envelope(envelope::Body::Input(Input {
            pane_id: b"pane-a".to_vec(),
            lease_epoch: u32::MAX as u64 + 1,
            data: b"ls\r".to_vec(),
        })),
        envelope(envelope::Body::TakeControl(TakeControl {
            pane_id: b"pane-a".to_vec(),
            requester_peer_id: b"peer-b".to_vec(),
            known_lease_epoch: u32::MAX as u64 + 2,
        })),
        envelope(envelope::Body::ControlLease(ControlLease {
            pane_id: b"pane-a".to_vec(),
            controller_peer_id: b"peer-b".to_vec(),
            lease_epoch: u32::MAX as u64 + 3,
        })),
        envelope(envelope::Body::Snapshot(Snapshot {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            sequence: u32::MAX as u64 + 4,
            screen: b"full screen".to_vec(),
            kitty_keyboard_active: false,
        })),
        envelope(envelope::Body::Delta(Delta {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            base_sequence: u32::MAX as u64 + 4,
            sequence: u32::MAX as u64 + 5,
            changes: b"patch".to_vec(),
            kitty_keyboard_active: false,
        })),
    ];

    let expected_body_shapes: [&[(u32, u8)]; 7] = [
        &[(1, 2), (2, 2), (3, 2)],
        &[(1, 2), (2, 2), (3, 2)],
        &[(1, 2), (2, 0), (3, 2)],
        &[(1, 2), (2, 2), (3, 0)],
        &[(1, 2), (2, 2), (3, 0)],
        &[(1, 2), (2, 2), (3, 0), (4, 2)],
        &[(1, 2), (2, 2), (3, 0), (4, 0), (5, 2)],
    ];
    for ((message, expected_body_field), expected_body_shape) in
        messages.into_iter().zip(10..=16).zip(expected_body_shapes)
    {
        let wire = message.encode_to_vec();
        let envelope_fields = parse_fields(&wire);
        assert_eq!(
            field_shape(&envelope_fields),
            vec![(1, 0), (2, 2), (expected_body_field, 2)],
        );
        assert_eq!(
            field_shape(&parse_fields(&envelope_fields[2].value)),
            expected_body_shape,
        );
        assert_eq!(Envelope::decode(wire.as_slice()).unwrap(), message);
    }
}

fn field_shape(fields: &[ParsedField]) -> Vec<(u32, u8)> {
    fields
        .iter()
        .map(|field| (field.field_number, field.wire_type))
        .collect()
}

fn parse_fields(input: &[u8]) -> Vec<ParsedField> {
    let mut position = 0;
    let mut fields = Vec::new();

    while position < input.len() {
        let key = read_varint(input, &mut position);
        let field_number = u32::try_from(key >> 3).expect("field number fits u32");
        let wire_type = u8::try_from(key & 0x07).expect("wire type fits u8");
        let value = match wire_type {
            0 => {
                let start = position;
                read_varint(input, &mut position);
                input[start..position].to_vec()
            }
            2 => {
                let length =
                    usize::try_from(read_varint(input, &mut position)).expect("length fits usize");
                let end = position
                    .checked_add(length)
                    .expect("length does not overflow");
                let value = input[position..end].to_vec();
                position = end;
                value
            }
            _ => panic!("unsupported wire type {wire_type}"),
        };
        fields.push(ParsedField {
            field_number,
            wire_type,
            value,
        });
    }

    fields
}

fn read_varint(input: &[u8], position: &mut usize) -> u64 {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *input.get(*position).expect("varint is complete");
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
    }
    panic!("varint exceeds u64");
}

fn sample_envelopes() -> Vec<Envelope> {
    vec![
        envelope(envelope::Body::Join(Join {
            session_id: b"session-a".to_vec(),
            peer_id: b"peer-a".to_vec(),
            endpoint_addr: b"endpoint-a".to_vec(),
            display_name: String::new(),
            member_kind: Default::default(),
        })),
        envelope(envelope::Body::Welcome(Welcome {
            session_id: b"session-a".to_vec(),
            admitted_peer_id: b"peer-a".to_vec(),
            coordinator_peer_id: b"peer-host".to_vec(),
            session_name: String::new(),
            coordinator_epoch: 0,
        })),
        envelope(envelope::Body::Input(Input {
            pane_id: b"pane-a".to_vec(),
            lease_epoch: 1,
            data: b"ls\r".to_vec(),
        })),
        envelope(envelope::Body::TakeControl(TakeControl {
            pane_id: b"pane-a".to_vec(),
            requester_peer_id: b"peer-b".to_vec(),
            known_lease_epoch: 0,
        })),
        envelope(envelope::Body::ControlLease(ControlLease {
            pane_id: b"pane-a".to_vec(),
            controller_peer_id: b"peer-b".to_vec(),
            lease_epoch: 1,
        })),
        envelope(envelope::Body::Snapshot(Snapshot {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            sequence: 1,
            screen: b"full screen".to_vec(),
            kitty_keyboard_active: false,
        })),
        envelope(envelope::Body::Delta(Delta {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            base_sequence: 1,
            sequence: 2,
            changes: b"patch".to_vec(),
            kitty_keyboard_active: false,
        })),
    ]
}

#[test]
fn framed_envelopes_round_trip_all_v1_bodies() {
    for original in sample_envelopes() {
        let frame = encode_frame(&original).expect("valid envelope encodes");
        assert_eq!(decode_frame(&frame).expect("valid frame decodes"), original);
    }
}

#[test]
fn kitty_keyboard_flag_round_trips() {
    let messages = [
        envelope(envelope::Body::Snapshot(Snapshot {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            sequence: 1,
            screen: b"full screen".to_vec(),
            kitty_keyboard_active: true,
        })),
        envelope(envelope::Body::Delta(Delta {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            base_sequence: 1,
            sequence: 2,
            changes: b"patch".to_vec(),
            kitty_keyboard_active: true,
        })),
    ];

    for message in messages {
        let frame = encode_frame(&message).expect("valid envelope encodes");
        assert_eq!(decode_frame(&frame).expect("valid frame decodes"), message);
    }
}

fn agent_roster(entries: Vec<AgentRosterEntry>) -> AgentRoster {
    AgentRoster {
        host_peer_id: b"peer-a".to_vec(),
        generation: 7,
        entries,
    }
}

fn agent_entry(pane_id: u64) -> AgentRosterEntry {
    AgentRosterEntry {
        pane_id,
        agent_kind: String::from("codex"),
        cwd: String::from("/repo"),
        state: AgentRosterState::Working as i32,
        working_since_unix_ms: 1_725_000_000_123,
    }
}

#[test]
fn agent_roster_uses_tag_26_and_round_trips() {
    let original = envelope(envelope::Body::AgentRoster(agent_roster(vec![
        agent_entry(9),
    ])));
    let wire = original.encode_to_vec();
    assert_eq!(
        field_shape(&parse_fields(&wire)),
        vec![(1, 0), (2, 2), (26, 2)]
    );
    assert_eq!(Envelope::decode(wire.as_slice()).unwrap(), original);
    let frame = encode_frame(&original).expect("valid roster encodes");
    assert_eq!(
        decode_frame(&frame).expect("valid roster decodes"),
        original
    );
}

fn presence(peer_id: &[u8], tab_id: u64, pane_id: u64) -> Presence {
    Presence {
        peer_id: peer_id.to_vec(),
        generation: 4,
        tab_id,
        pane_id,
        attached: true,
    }
}

#[test]
fn presence_uses_tag_27_and_round_trips() {
    let original = envelope(envelope::Body::Presence(presence(b"peer-a", 2, 9)));
    let wire = original.encode_to_vec();
    assert_eq!(
        field_shape(&parse_fields(&wire)),
        vec![(1, 0), (2, 2), (27, 2)]
    );
    assert_eq!(Envelope::decode(wire.as_slice()).unwrap(), original);
    let frame = encode_frame(&original).expect("valid presence encodes");
    assert_eq!(
        decode_frame(&frame).expect("valid presence decodes"),
        original
    );
}

#[test]
fn detached_presence_round_trips_as_an_empty_location() {
    // A detached member has no focus at all. The zero tab/pane is the wire's way of
    // saying so, and prost elides both fields -- the whole update is a few bytes.
    let original = envelope(envelope::Body::Presence(Presence {
        peer_id: b"peer-a".to_vec(),
        generation: 5,
        tab_id: 0,
        pane_id: 0,
        attached: false,
    }));
    let frame = encode_frame(&original).expect("detached presence encodes");
    assert_eq!(
        decode_frame(&frame).expect("detached presence decodes"),
        original
    );
}

#[test]
fn presence_roster_uses_tag_28_and_round_trips() {
    let original = envelope(envelope::Body::PresenceRoster(PresenceRoster {
        entries: vec![
            presence(b"peer-a", 2, 9),
            Presence {
                peer_id: b"peer-b".to_vec(),
                generation: 1,
                tab_id: 0,
                pane_id: 0,
                attached: false,
            },
        ],
    }));
    let wire = original.encode_to_vec();
    assert_eq!(
        field_shape(&parse_fields(&wire)),
        vec![(1, 0), (2, 2), (28, 2)]
    );
    let frame = encode_frame(&original).expect("valid presence roster encodes");
    assert_eq!(
        decode_frame(&frame).expect("valid presence roster decodes"),
        original
    );
}

#[test]
fn presence_entry_limit_matches_the_member_limit() {
    assert_eq!(MAX_PRESENCE_ENTRIES, p2pmux::layout::MAX_MEMBERS);
}

#[test]
fn presence_validation_rejects_bad_shapes() {
    let oversized_peer = Presence {
        peer_id: vec![0; MAX_PEER_ID_BYTES + 1],
        ..presence(b"peer-a", 1, 1)
    };
    assert!(matches!(
        encode_frame(&envelope(envelope::Body::Presence(oversized_peer))),
        Err(ProtocolError::FieldTooLarge {
            field: "presence.peer_id",
            ..
        })
    ));

    // An attached member is always somewhere; a detached one is always nowhere.
    let attached_nowhere = Presence {
        tab_id: 0,
        ..presence(b"peer-a", 0, 1)
    };
    assert!(encode_frame(&envelope(envelope::Body::Presence(attached_nowhere))).is_err());
    let detached_somewhere = Presence {
        attached: false,
        ..presence(b"peer-a", 1, 1)
    };
    assert!(encode_frame(&envelope(envelope::Body::Presence(detached_somewhere))).is_err());
}

#[test]
fn presence_roster_rejects_duplicate_members_and_an_oversized_set() {
    let duplicated = PresenceRoster {
        entries: vec![presence(b"peer-a", 1, 1), presence(b"peer-a", 1, 2)],
    };
    assert!(matches!(
        encode_frame(&envelope(envelope::Body::PresenceRoster(duplicated))),
        Err(ProtocolError::InvalidLayout(
            "presence_roster.entry.peer_id"
        ))
    ));

    let too_many = PresenceRoster {
        entries: (0..=u64::try_from(MAX_PRESENCE_ENTRIES).unwrap())
            .map(|index| Presence {
                peer_id: format!("peer-{index}").into_bytes(),
                generation: 1,
                tab_id: 1,
                pane_id: index + 1,
                attached: true,
            })
            .collect(),
    };
    assert!(matches!(
        encode_frame(&envelope(envelope::Body::PresenceRoster(too_many))),
        Err(ProtocolError::FieldTooLarge {
            field: "presence_roster.entries",
            ..
        })
    ));
}

#[test]
fn agent_roster_accepts_opencode_kind() {
    let roster = agent_roster(vec![AgentRosterEntry {
        agent_kind: String::from("opencode"),
        ..agent_entry(9)
    }]);

    assert!(encode_frame(&envelope(envelope::Body::AgentRoster(roster))).is_ok());
}

#[test]
fn agent_roster_allows_zero_working_since_while_working() {
    let roster = agent_roster(vec![AgentRosterEntry {
        working_since_unix_ms: 0,
        ..agent_entry(9)
    }]);

    assert!(encode_frame(&envelope(envelope::Body::AgentRoster(roster))).is_ok());
}

#[test]
fn agent_roster_survives_states_and_kinds_from_a_newer_peer() {
    // Both fields are open vocabularies. A peer on a newer build may name an
    // agent this binary has never heard of, or report a state it has no variant
    // for; neither may fail the frame, because `decode_frame` failing drops the
    // whole peer stream in `FrameReader::read_next`. An unknown state reads as
    // `Idle` rather than vanishing from the overlay.
    let roster = agent_roster(vec![AgentRosterEntry {
        agent_kind: String::from("some-future-agent"),
        state: 99,
        ..agent_entry(9)
    }]);
    let original = envelope(envelope::Body::AgentRoster(roster));

    let frame = encode_frame(&original).expect("an unknown kind and state still encode");
    assert_eq!(
        decode_frame(&frame).expect("an unknown kind and state still decode"),
        original
    );
    assert_eq!(AgentRosterState::from_wire(99), AgentRosterState::Idle);
    assert_eq!(
        AgentRosterState::from_wire(AgentRosterState::Done as i32),
        AgentRosterState::Done
    );
}

#[test]
fn agent_roster_state_severity_ranks_failures_over_waiting_over_work() {
    // Severity is deliberately not the enum's own ordering: `Working = 1` and
    // `Done = 2` are frozen wire values, so declaration order says Done is more
    // severe than Working, which is backwards. `severity()` is the real order.
    let mut by_severity = [
        AgentRosterState::Pending,
        AgentRosterState::Idle,
        AgentRosterState::Error,
        AgentRosterState::Done,
        AgentRosterState::Working,
    ];
    by_severity.sort_by_key(|state| state.severity());
    assert_eq!(
        by_severity,
        [
            AgentRosterState::Idle,
            AgentRosterState::Done,
            AgentRosterState::Working,
            AgentRosterState::Pending,
            AgentRosterState::Error,
        ]
    );

    // needs_you is the blocked set; needs_attention adds a finished turn.
    assert!(AgentRosterState::Pending.needs_you());
    assert!(AgentRosterState::Error.needs_you());
    assert!(!AgentRosterState::Done.needs_you());
    assert!(!AgentRosterState::Working.needs_you());
    assert!(!AgentRosterState::Idle.needs_you());

    assert!(AgentRosterState::Done.needs_attention());
    assert!(!AgentRosterState::Working.needs_attention());
    assert!(!AgentRosterState::Idle.needs_attention());

    assert!(AgentRosterState::Done.is_completion());
    assert!(AgentRosterState::Error.is_completion());
    assert!(!AgentRosterState::Pending.is_completion());
}

#[test]
fn agent_roster_carries_the_new_states_over_the_wire() {
    for state in [AgentRosterState::Pending, AgentRosterState::Error] {
        let roster = agent_roster(vec![AgentRosterEntry {
            state: state as i32,
            ..agent_entry(9)
        }]);
        let original = envelope(envelope::Body::AgentRoster(roster));
        let frame = encode_frame(&original).expect("new states encode");
        assert_eq!(decode_frame(&frame).expect("new states decode"), original);
        assert_eq!(AgentRosterState::from_wire(state as i32), state);
    }
}

#[test]
fn agent_roster_validation_rejects_bad_entries() {
    let cases = [
        agent_roster(vec![agent_entry(0)]),
        agent_roster(vec![agent_entry(1), agent_entry(1)]),
        // A kind is still shape-checked, so nothing that would corrupt a
        // rendered cell gets through: empty, uppercase, spaces, control chars.
        agent_roster(vec![AgentRosterEntry {
            agent_kind: String::new(),
            ..agent_entry(1)
        }]),
        agent_roster(vec![AgentRosterEntry {
            agent_kind: String::from("Claude"),
            ..agent_entry(1)
        }]),
        agent_roster(vec![AgentRosterEntry {
            agent_kind: String::from("claude code"),
            ..agent_entry(1)
        }]),
        agent_roster(vec![AgentRosterEntry {
            agent_kind: String::from("claude\u{1b}[2J"),
            ..agent_entry(1)
        }]),
        agent_roster(vec![AgentRosterEntry {
            agent_kind: "x".repeat(MAX_AGENT_KIND_BYTES + 1),
            ..agent_entry(1)
        }]),
        agent_roster(vec![AgentRosterEntry {
            cwd: "x".repeat(MAX_AGENT_CWD_BYTES + 1),
            ..agent_entry(1)
        }]),
        agent_roster(
            (1..=u64::try_from(MAX_AGENT_ROSTER_ENTRIES + 1).unwrap())
                .map(agent_entry)
                .collect(),
        ),
    ];

    for roster in cases {
        assert!(matches!(
            encode_frame(&envelope(envelope::Body::AgentRoster(roster))),
            Err(ProtocolError::FieldTooLarge { .. }) | Err(ProtocolError::InvalidLayout(_))
        ));
    }
}

fn leaf(pane_id: u64) -> LayoutNode {
    LayoutNode {
        leaf_pane_id: Some(pane_id),
        split: None,
    }
}

fn layout_state(root: LayoutNode) -> LayoutState {
    LayoutState {
        revision: 1,
        members: vec![MemberDescriptor {
            peer_id: b"peer-a".to_vec(),
            endpoint_addr: b"endpoint-a".to_vec(),
            display_name: String::new(),
            member_kind: Default::default(),
        }],
        panes: vec![PaneDescriptor {
            pane_id: 1,
            host_peer_id: b"peer-a".to_vec(),
            grid_rows: 24,
            grid_cols: 80,
            title: None,
            locked: true,
            exited: false,
        }],
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(root),

            title: None,
        }],
    }
}

fn v2_envelopes() -> Vec<Envelope> {
    let state = layout_state(leaf(1));
    vec![
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(state.clone()),
        })),
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            request_id: 1,
            base_revision: 1,
            create_pane: Some(CreatePane {
                target_pane_id: 1,
                axis: Some(SplitAxis::LeftRight as i32),
                grid_rows: 24,
                grid_cols: 80,
                position: None,
            }),
            delete_pane: None,
            create_tab: None,
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,

            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        })),
        envelope(envelope::Body::PaneReservation(PaneReservation {
            reservation_id: 1,
            pane_id: 2,
            tab_id: None,
        })),
        envelope(envelope::Body::PaneReady(PaneReady {
            reservation_id: 1,
            base_revision: 1,
            request_id: 1,
            author_signature: Vec::new(),
        })),
        envelope(envelope::Body::LayoutCommit(LayoutCommit {
            revision: 1,
            state: Some(state),
            entry: Some(ledger_entry()),
        })),
        envelope(envelope::Body::LayoutReject(LayoutReject {
            request_id: 1,
            reason: LayoutRejectReason::NotHost as i32,
        })),
        envelope(envelope::Body::PaneSubscribe(PaneSubscribe {
            session_id: b"session-a".to_vec(),
            peer_id: b"peer-a".to_vec(),
            pane_id: 1,
        })),
        envelope(envelope::Body::PaneFailed(PaneFailed {
            reservation_id: 1,
            request_id: 1,
            base_revision: 1,
        })),
    ]
}

#[test]
fn envelope_exposes_each_v2_layout_body_with_stable_tags() {
    let expected_body_shapes: [&[(u32, u8)]; 8] = [
        &[(1, 2)],
        &[(1, 0), (2, 0), (3, 2)],
        &[(1, 0), (2, 0)],
        &[(1, 0), (2, 0), (3, 0)],
        // LayoutCommit: revision, state, and the ledger entry the state materializes.
        &[(1, 0), (2, 2), (3, 2)],
        &[(1, 0), (2, 0)],
        &[(1, 2), (2, 2), (3, 0)],
        &[(1, 0), (2, 0), (3, 0)],
    ];

    for ((message, expected_body_field), expected_body_shape) in v2_envelopes()
        .into_iter()
        .zip(17..=24)
        .zip(expected_body_shapes)
    {
        let wire = message.encode_to_vec();
        let envelope_fields = parse_fields(&wire);
        assert_eq!(
            field_shape(&envelope_fields),
            vec![(1, 0), (2, 2), (expected_body_field, 2)],
        );
        assert_eq!(
            field_shape(&parse_fields(&envelope_fields[2].value)),
            expected_body_shape,
        );
        assert_eq!(Envelope::decode(wire.as_slice()).unwrap(), message);
    }
}

#[test]
fn framed_envelopes_round_trip_all_v2_layout_bodies() {
    for original in v2_envelopes() {
        let frame = encode_frame(&original).expect("valid layout envelope encodes");
        assert_eq!(decode_frame(&frame).expect("valid frame decodes"), original);
    }
}

#[test]
fn layout_messages_reject_invalid_shapes_and_bounds() {
    let state = layout_state(leaf(1));
    let empty_action = LayoutRequest {
        request_id: 1,
        base_revision: 1,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: None,
        update_pane_grids: None,

        rename_pane: None,
        rename_tab: None,
        set_pane_lock: None,
        mark_pane_exited: None,
        author_signature: Vec::new(),
    };
    let multiple_actions = LayoutRequest {
        create_pane: Some(CreatePane {
            target_pane_id: 1,
            axis: Some(SplitAxis::LeftRight as i32),
            grid_rows: 24,
            grid_cols: 80,
            position: None,
        }),
        delete_pane: Some(DeletePane { pane_id: 1 }),
        ..empty_action.clone()
    };
    let empty_node = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(LayoutNode {
                leaf_pane_id: None,
                split: None,
            }),

            title: None,
        }],
        ..state.clone()
    };
    let multiple_nodes = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(LayoutNode {
                leaf_pane_id: Some(1),
                split: Some(Box::new(LayoutSplit {
                    axis: Some(SplitAxis::LeftRight as i32),
                    first: Some(leaf(1)),
                    second: Some(leaf(1)),
                    first_share_bps: None,
                })),
            }),

            title: None,
        }],
        ..state.clone()
    };
    let too_many_members = LayoutState {
        members: (0..9)
            .map(|index| MemberDescriptor {
                peer_id: format!("peer-{index}").into_bytes(),
                endpoint_addr: b"endpoint".to_vec(),
                display_name: String::new(),
                member_kind: Default::default(),
            })
            .collect(),
        ..state.clone()
    };
    let too_many_panes = LayoutState {
        panes: (1..=73)
            .map(|pane_id| PaneDescriptor {
                pane_id,
                host_peer_id: b"peer-a".to_vec(),
                grid_rows: 24,
                grid_cols: 80,
                title: None,
                locked: false,
                exited: false,
            })
            .collect(),
        ..state.clone()
    };
    let too_many_tabs = LayoutState {
        tabs: (1..=10)
            .map(|tab_id| TabDescriptor {
                tab_id,
                root: Some(leaf(1)),
                title: None,
            })
            .collect(),
        ..state.clone()
    };
    let zero_pane_grid = LayoutState {
        panes: vec![PaneDescriptor {
            pane_id: 1,
            host_peer_id: b"peer-a".to_vec(),
            grid_rows: 0,
            grid_cols: 80,
            title: None,
            locked: true,
            exited: false,
        }],
        ..state.clone()
    };
    let invalid_grid = LayoutRequest {
        create_tab: Some(CreateTab {
            grid_rows: 0,
            grid_cols: 80,
        }),
        ..empty_action.clone()
    };
    let invalid_delete = LayoutRequest {
        delete_tab: Some(DeleteTab { tab_id: 0 }),
        ..empty_action.clone()
    };
    let invalid_commit = LayoutCommit {
        revision: 2,
        state: Some(state.clone()),
        entry: Some(ledger_entry()),
    };

    let cases = vec![
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            request_id: 0,
            base_revision: 1,
            ..multiple_actions.clone()
        })),
        envelope(envelope::Body::LayoutRequest(empty_action)),
        envelope(envelope::Body::LayoutRequest(multiple_actions)),
        envelope(envelope::Body::LayoutRequest(invalid_grid)),
        envelope(envelope::Body::LayoutRequest(invalid_delete)),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(empty_node),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(multiple_nodes),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(too_many_members),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(too_many_panes),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(too_many_tabs),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(zero_pane_grid),
        })),
        envelope(envelope::Body::LayoutCommit(invalid_commit)),
        // A commit with no entry is a snapshot with nothing vouching for it, which is the
        // shape this protocol version exists to stop accepting.
        envelope(envelope::Body::LayoutCommit(LayoutCommit {
            revision: 1,
            state: Some(state.clone()),
            entry: None,
        })),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            seq: 0,
            ..ledger_entry()
        }))),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            prev_hash: vec![0; LEDGER_HASH_BYTES - 1],
            ..ledger_entry()
        }))),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            author_peer_id: Vec::new(),
            ..ledger_entry()
        }))),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            kind: 99,
            ..ledger_entry()
        }))),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            payload: vec![0; MAX_LEDGER_PAYLOAD_BYTES + 1],
            ..ledger_entry()
        }))),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            author_signature: vec![3; LEDGER_SIGNATURE_BYTES - 1],
            ..ledger_entry()
        }))),
        envelope(envelope::Body::LayoutCommit(commit_with(LedgerEntry {
            coordinator_signature: Vec::new(),
            ..ledger_entry()
        }))),
        envelope(envelope::Body::PaneReservation(PaneReservation {
            reservation_id: 0,
            pane_id: 1,
            tab_id: None,
        })),
        envelope(envelope::Body::PaneReady(PaneReady {
            reservation_id: 0,
            base_revision: 1,
            request_id: 1,
            author_signature: Vec::new(),
        })),
        envelope(envelope::Body::LayoutReject(LayoutReject {
            request_id: 1,
            reason: 0,
        })),
        envelope(envelope::Body::PaneSubscribe(PaneSubscribe {
            session_id: Vec::new(),
            peer_id: b"peer-a".to_vec(),
            pane_id: 1,
        })),
    ];

    for invalid in cases {
        assert!(
            encode_frame(&invalid).is_err(),
            "{invalid:?} must be rejected"
        );
    }
}

#[test]
fn join_endpoint_and_reservation_lifecycle_identifiers_are_required() {
    let empty_join_endpoint = envelope(envelope::Body::Join(Join {
        session_id: b"session-a".to_vec(),
        peer_id: b"peer-a".to_vec(),
        endpoint_addr: Vec::new(),
        display_name: String::new(),
        member_kind: Default::default(),
    }));
    let missing_ready_revision = envelope(envelope::Body::PaneReady(PaneReady {
        reservation_id: 1,
        base_revision: 0,
        request_id: 1,
        author_signature: Vec::new(),
    }));
    let missing_ready_request = envelope(envelope::Body::PaneReady(PaneReady {
        reservation_id: 1,
        base_revision: 1,
        request_id: 0,
        author_signature: Vec::new(),
    }));
    let missing_failed_request = envelope(envelope::Body::PaneFailed(PaneFailed {
        reservation_id: 1,
        request_id: 0,
        base_revision: 1,
    }));

    for invalid in [
        empty_join_endpoint,
        missing_ready_revision,
        missing_ready_request,
        missing_failed_request,
    ] {
        assert!(
            encode_frame(&invalid).is_err(),
            "v2 Join endpoint addresses and PaneReady revisions are required"
        );
        let mut raw = Vec::new();
        invalid
            .encode_length_delimited(&mut raw)
            .expect("raw invalid frame should encode");
        assert!(decode_frame(&raw).is_err());
    }
}

#[test]
fn create_pane_axis_and_position_are_validated() {
    fn create_request(axis: Option<i32>, position: Option<i32>) -> Envelope {
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            request_id: 1,
            base_revision: 1,
            create_pane: Some(CreatePane {
                target_pane_id: 1,
                axis,
                grid_rows: 24,
                grid_cols: 80,
                position,
            }),
            delete_pane: None,
            create_tab: None,
            delete_tab: None,
            set_split_ratio: None,
            update_pane_grids: None,

            rename_pane: None,
            rename_tab: None,
            set_pane_lock: None,
            mark_pane_exited: None,
            author_signature: Vec::new(),
        }))
    }

    fn split_snapshot(axis: Option<i32>) -> Envelope {
        let state = LayoutState {
            panes: vec![
                PaneDescriptor {
                    pane_id: 1,
                    host_peer_id: b"peer-a".to_vec(),
                    grid_rows: 24,
                    grid_cols: 80,
                    title: None,
                    locked: false,
                    exited: false,
                },
                PaneDescriptor {
                    pane_id: 2,
                    host_peer_id: b"peer-a".to_vec(),
                    grid_rows: 24,
                    grid_cols: 80,
                    title: None,
                    locked: false,
                    exited: false,
                },
            ],
            tabs: vec![TabDescriptor {
                tab_id: 1,
                root: Some(LayoutNode {
                    leaf_pane_id: None,
                    split: Some(Box::new(LayoutSplit {
                        axis,
                        first: Some(leaf(1)),
                        second: Some(leaf(2)),
                        first_share_bps: None,
                    })),
                }),

                title: None,
            }],
            ..layout_state(leaf(1))
        };
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(state),
        }))
    }

    for invalid in [
        create_request(None, None),
        create_request(Some(99), None),
        create_request(Some(SplitAxis::LeftRight as i32), Some(99)),
        split_snapshot(None),
        split_snapshot(Some(99)),
    ] {
        assert!(
            encode_frame(&invalid).is_err(),
            "layout axes and placement must be valid"
        );
        let mut raw = Vec::new();
        invalid
            .encode_length_delimited(&mut raw)
            .expect("raw invalid frame should encode");
        assert!(decode_frame(&raw).is_err());
    }
}

#[test]
fn create_pane_position_has_a_stable_new_wire_tag_and_defaults_to_second() {
    let default = CreatePane {
        target_pane_id: 1,
        axis: Some(SplitAxis::LeftRight as i32),
        grid_rows: 24,
        grid_cols: 80,
        position: None,
    };
    assert_eq!(
        field_shape(&parse_fields(&default.encode_to_vec())),
        vec![(1, 0), (2, 0), (3, 0), (4, 0)]
    );

    let first = CreatePane {
        position: Some(NewPanePosition::First as i32),
        ..default.clone()
    };
    assert_eq!(
        field_shape(&parse_fields(&first.encode_to_vec())),
        vec![(1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]
    );
    assert_eq!(
        CreatePane::decode(first.encode_to_vec().as_slice()).unwrap(),
        first
    );
}

#[test]
fn layout_grids_must_fit_the_reducer_u16_grid() {
    let maximum = u32::from(u16::MAX);
    let oversized = maximum + 1;
    let state = layout_state(leaf(1));
    let empty_action = LayoutRequest {
        request_id: 1,
        base_revision: 1,
        create_pane: None,
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
        set_split_ratio: None,
        update_pane_grids: None,

        rename_pane: None,
        rename_tab: None,
        set_pane_lock: None,
        mark_pane_exited: None,
        author_signature: Vec::new(),
    };

    for valid in [
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            create_pane: Some(CreatePane {
                target_pane_id: 1,
                axis: Some(SplitAxis::LeftRight as i32),
                grid_rows: maximum,
                grid_cols: maximum,
                position: None,
            }),
            ..empty_action.clone()
        })),
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            create_tab: Some(CreateTab {
                grid_rows: maximum,
                grid_cols: maximum,
            }),
            ..empty_action.clone()
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(LayoutState {
                panes: vec![PaneDescriptor {
                    grid_rows: maximum,
                    grid_cols: maximum,
                    ..state.panes[0].clone()
                }],
                ..state.clone()
            }),
        })),
    ] {
        assert!(
            encode_frame(&valid).is_ok(),
            "u16 grid maximum must be valid"
        );
    }

    let invalid = [
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            create_pane: Some(CreatePane {
                target_pane_id: 1,
                axis: Some(SplitAxis::LeftRight as i32),
                grid_rows: oversized,
                grid_cols: 80,
                position: None,
            }),
            ..empty_action.clone()
        })),
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            create_pane: Some(CreatePane {
                target_pane_id: 1,
                axis: Some(SplitAxis::LeftRight as i32),
                grid_rows: 24,
                grid_cols: oversized,
                position: None,
            }),
            ..empty_action.clone()
        })),
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            create_tab: Some(CreateTab {
                grid_rows: oversized,
                grid_cols: 80,
            }),
            ..empty_action.clone()
        })),
        envelope(envelope::Body::LayoutRequest(LayoutRequest {
            create_tab: Some(CreateTab {
                grid_rows: 24,
                grid_cols: oversized,
            }),
            ..empty_action.clone()
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(LayoutState {
                panes: vec![PaneDescriptor {
                    grid_rows: oversized,
                    ..state.panes[0].clone()
                }],
                ..state.clone()
            }),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(LayoutState {
                panes: vec![PaneDescriptor {
                    grid_cols: oversized,
                    ..state.panes[0].clone()
                }],
                ..state
            }),
        })),
    ];

    for invalid in invalid {
        assert!(
            encode_frame(&invalid).is_err(),
            "oversized layout grid must be rejected"
        );
    }
}

#[test]
fn pane_reservation_rejects_a_zero_present_tab_id() {
    for tab_id in [None, Some(1)] {
        assert!(
            encode_frame(&envelope(envelope::Body::PaneReservation(
                PaneReservation {
                    reservation_id: 1,
                    pane_id: 2,
                    tab_id,
                }
            )))
            .is_ok(),
            "an absent or nonzero reservation tab ID must be accepted"
        );
    }

    let reservation = envelope(envelope::Body::PaneReservation(PaneReservation {
        reservation_id: 1,
        pane_id: 2,
        tab_id: Some(0),
    }));

    assert!(
        encode_frame(&reservation).is_err(),
        "a present reservation tab ID must be nonzero"
    );
}

#[test]
fn layout_state_rejects_empty_duplicate_and_dangling_references() {
    let state = layout_state(leaf(1));
    let zero_revision = LayoutState {
        revision: 0,
        ..state.clone()
    };
    let empty_members = LayoutState {
        members: Vec::new(),
        ..state.clone()
    };
    let empty_panes = LayoutState {
        panes: Vec::new(),
        ..state.clone()
    };
    let empty_tabs = LayoutState {
        tabs: Vec::new(),
        ..state.clone()
    };
    let duplicate_members = LayoutState {
        members: vec![state.members[0].clone(), state.members[0].clone()],
        ..state.clone()
    };
    let empty_member_id = LayoutState {
        members: vec![MemberDescriptor {
            peer_id: Vec::new(),
            ..state.members[0].clone()
        }],
        ..state.clone()
    };
    let empty_member_endpoint = LayoutState {
        members: vec![MemberDescriptor {
            endpoint_addr: Vec::new(),
            ..state.members[0].clone()
        }],
        ..state.clone()
    };
    let duplicate_tabs = LayoutState {
        tabs: vec![state.tabs[0].clone(), state.tabs[0].clone()],
        ..state.clone()
    };
    let duplicate_panes = LayoutState {
        panes: vec![state.panes[0].clone(), state.panes[0].clone()],
        ..state.clone()
    };
    let zero_pane_id = LayoutState {
        panes: vec![PaneDescriptor {
            pane_id: 0,
            ..state.panes[0].clone()
        }],
        ..state.clone()
    };
    let zero_tab_id = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 0,
            ..state.tabs[0].clone()
        }],
        ..state.clone()
    };
    let unknown_host = LayoutState {
        panes: vec![PaneDescriptor {
            host_peer_id: b"peer-b".to_vec(),
            ..state.panes[0].clone()
        }],
        ..state.clone()
    };
    let duplicate_leaves = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(LayoutNode {
                leaf_pane_id: None,
                split: Some(Box::new(LayoutSplit {
                    axis: Some(SplitAxis::LeftRight as i32),
                    first: Some(leaf(1)),
                    second: Some(leaf(1)),
                    first_share_bps: None,
                })),
            }),

            title: None,
        }],
        ..state.clone()
    };
    let unknown_leaf = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(leaf(2)),

            title: None,
        }],
        ..state.clone()
    };
    let zero_leaf = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(leaf(0)),

            title: None,
        }],
        ..state.clone()
    };
    let unreferenced_pane = LayoutState {
        panes: vec![
            state.panes[0].clone(),
            PaneDescriptor {
                pane_id: 2,
                host_peer_id: b"peer-a".to_vec(),
                grid_rows: 24,
                grid_cols: 80,
                title: None,
                locked: false,
                exited: false,
            },
        ],
        ..state
    };

    for invalid in [
        zero_revision,
        empty_members,
        empty_panes,
        empty_tabs,
        duplicate_members,
        empty_member_id,
        empty_member_endpoint,
        duplicate_tabs,
        duplicate_panes,
        zero_pane_id,
        zero_tab_id,
        unknown_host,
        duplicate_leaves,
        unknown_leaf,
        zero_leaf,
        unreferenced_pane,
    ] {
        assert!(
            encode_frame(&envelope(envelope::Body::SessionSnapshot(
                SessionSnapshot {
                    state: Some(invalid),
                }
            )))
            .is_err(),
            "inconsistent layout state must be rejected"
        );
    }
}

#[test]
fn layout_state_wire_shape_includes_all_nested_fields() {
    let state = LayoutState {
        revision: 7,
        members: vec![MemberDescriptor {
            peer_id: b"peer-a".to_vec(),
            endpoint_addr: b"endpoint-a".to_vec(),
            display_name: String::new(),
            member_kind: Default::default(),
        }],
        panes: vec![PaneDescriptor {
            pane_id: 11,
            host_peer_id: b"peer-a".to_vec(),
            grid_rows: 24,
            grid_cols: 80,
            title: None,
            locked: true,
            exited: false,
        }],
        tabs: vec![TabDescriptor {
            tab_id: 13,
            root: Some(LayoutNode {
                leaf_pane_id: None,
                split: Some(Box::new(LayoutSplit {
                    axis: Some(SplitAxis::TopBottom as i32),
                    first: Some(leaf(11)),
                    second: Some(leaf(11)),
                    first_share_bps: None,
                })),
            }),

            title: None,
        }],
    };
    let state_fields = parse_fields(&state.encode_to_vec());
    assert_eq!(
        field_shape(&state_fields),
        vec![(1, 0), (2, 2), (3, 2), (4, 2)]
    );
    assert_eq!(
        field_shape(&parse_fields(&state_fields[1].value)),
        vec![(1, 2), (2, 2)]
    );
    assert_eq!(
        field_shape(&parse_fields(&state_fields[2].value)),
        vec![(1, 0), (2, 2), (3, 0), (4, 0), (6, 0)]
    );
    let tab_fields = parse_fields(&state_fields[3].value);
    assert_eq!(field_shape(&tab_fields), vec![(1, 0), (2, 2)]);
    let node_fields = parse_fields(&tab_fields[1].value);
    assert_eq!(field_shape(&node_fields), vec![(2, 2)]);
    let split_fields = parse_fields(&node_fields[0].value);
    assert_eq!(field_shape(&split_fields), vec![(1, 0), (2, 2), (3, 2)]);
    assert_eq!(
        field_shape(&parse_fields(&split_fields[1].value)),
        vec![(1, 0)]
    );
    assert_eq!(
        field_shape(&parse_fields(&split_fields[2].value)),
        vec![(1, 0)]
    );
}

#[test]
fn join_wire_shape_encodes_a_present_endpoint_address() {
    let join = Join {
        session_id: b"session-a".to_vec(),
        peer_id: b"peer-a".to_vec(),
        endpoint_addr: b"endpoint-a".to_vec(),
        display_name: String::new(),
        member_kind: Default::default(),
    };

    assert_eq!(
        field_shape(&parse_fields(&join.encode_to_vec())),
        vec![(1, 2), (2, 2), (3, 2)]
    );
}

#[test]
fn layout_messages_reject_deep_or_wide_trees_and_oversize_join_endpoint() {
    fn full_tree(depth: usize, next_pane_id: &mut u64) -> LayoutNode {
        if depth == 0 {
            let pane_id = *next_pane_id;
            *next_pane_id += 1;
            return leaf(pane_id);
        }
        LayoutNode {
            leaf_pane_id: None,
            split: Some(Box::new(LayoutSplit {
                axis: Some(SplitAxis::TopBottom as i32),
                first: Some(full_tree(depth - 1, next_pane_id)),
                second: Some(full_tree(depth - 1, next_pane_id)),
                first_share_bps: None,
            })),
        }
    }

    let mut next_pane_id = 1;
    let wide_tree = full_tree(4, &mut next_pane_id);
    let mut next_pane_id = 1;
    let deep_tree = full_tree(5, &mut next_pane_id);
    let too_wide = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(wide_tree),

            title: None,
        }],
        ..layout_state(leaf(1))
    };
    let too_deep = LayoutState {
        tabs: vec![TabDescriptor {
            tab_id: 1,
            root: Some(deep_tree),

            title: None,
        }],
        ..layout_state(leaf(1))
    };
    let oversized_endpoint = envelope(envelope::Body::Join(Join {
        session_id: b"session-a".to_vec(),
        peer_id: b"peer-a".to_vec(),
        endpoint_addr: vec![0; MAX_ENDPOINT_ADDR_BYTES + 1],
        display_name: String::new(),
        member_kind: Default::default(),
    }));

    for invalid in [
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(too_wide),
        })),
        envelope(envelope::Body::SessionSnapshot(SessionSnapshot {
            state: Some(too_deep),
        })),
        oversized_endpoint,
    ] {
        assert!(
            encode_frame(&invalid).is_err(),
            "{invalid:?} must be rejected"
        );
    }
}

#[test]
fn decoder_rejects_unsupported_version() {
    for version in [1, PROTOCOL_VERSION + 1] {
        let mut wrong = sample_envelopes().remove(0);
        wrong.version = version;
        let mut frame = Vec::new();
        wrong.encode_length_delimited(&mut frame).unwrap();

        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::UnsupportedVersion(v)) if v == version
        ));
    }
}

#[test]
fn decoder_rejects_oversize_declared_and_decoded_payloads() {
    let declared_only = encode_varint((MAX_ENVELOPE_BYTES + 1) as u64);
    assert!(matches!(
        decode_frame(&declared_only),
        Err(ProtocolError::FrameTooLarge { .. })
    ));

    let oversized_input = envelope(envelope::Body::Input(Input {
        pane_id: b"pane-a".to_vec(),
        lease_epoch: 1,
        data: vec![0; MAX_INPUT_BYTES + 1],
    }));
    let mut input_frame = Vec::new();
    oversized_input
        .encode_length_delimited(&mut input_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&input_frame),
        Err(ProtocolError::FieldTooLarge {
            field: "input.data",
            ..
        })
    ));

    let oversized_snapshot = envelope(envelope::Body::Snapshot(Snapshot {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        sequence: 1,
        screen: vec![0; MAX_SNAPSHOT_BYTES + 1],
        kitty_keyboard_active: false,
    }));
    let mut snapshot_frame = Vec::new();
    oversized_snapshot
        .encode_length_delimited(&mut snapshot_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&snapshot_frame),
        Err(ProtocolError::FieldTooLarge {
            field: "snapshot.screen",
            ..
        })
    ));

    let oversized_delta = envelope(envelope::Body::Delta(Delta {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        base_sequence: 1,
        sequence: 2,
        changes: vec![0; MAX_DELTA_BYTES + 1],
        kitty_keyboard_active: false,
    }));
    let mut delta_frame = Vec::new();
    oversized_delta
        .encode_length_delimited(&mut delta_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&delta_frame),
        Err(ProtocolError::FieldTooLarge {
            field: "delta.changes",
            ..
        })
    ));
}

#[test]
fn decoder_rejects_malformed_and_truncated_length_prefixes() {
    assert!(matches!(
        decode_frame(&[0x80]),
        Err(ProtocolError::MalformedLengthPrefix)
    ));

    let mut overflowing = vec![0x80; 9];
    overflowing.push(0x02);
    assert!(matches!(
        decode_frame(&overflowing),
        Err(ProtocolError::MalformedLengthPrefix)
    ));
}

#[test]
fn decoder_rejects_truncated_and_trailing_frames() {
    let mut truncated = encode_frame(&sample_envelopes()[0]).unwrap();
    truncated.pop();
    assert!(matches!(
        decode_frame(&truncated),
        Err(ProtocolError::TruncatedFrame { .. })
    ));

    let mut trailing = encode_frame(&sample_envelopes()[0]).unwrap();
    trailing.push(0);
    assert!(matches!(
        decode_frame(&trailing),
        Err(ProtocolError::TrailingFrameBytes { .. })
    ));
}

#[test]
fn decoder_rejects_overcomplete_frames_before_decoding() {
    assert!(matches!(
        decode_frame(&vec![0; MAX_FRAME_BYTES + 1]),
        Err(ProtocolError::FrameTooLarge { .. })
    ));
}

#[test]
fn decoder_rejects_missing_fields_and_invalid_sequences() {
    let missing_body = Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: b"peer-a".to_vec(),
        body: None,
    };
    let mut missing_body_frame = Vec::new();
    missing_body
        .encode_length_delimited(&mut missing_body_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&missing_body_frame),
        Err(ProtocolError::MissingBody)
    ));

    let empty_id = envelope(envelope::Body::Join(Join {
        session_id: Vec::new(),
        peer_id: b"peer-a".to_vec(),
        endpoint_addr: Vec::new(),
        display_name: String::new(),
        member_kind: Default::default(),
    }));
    let mut empty_id_frame = Vec::new();
    empty_id
        .encode_length_delimited(&mut empty_id_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&empty_id_frame),
        Err(ProtocolError::EmptyField("join.session_id"))
    ));

    let zero_epoch = envelope(envelope::Body::Input(Input {
        pane_id: b"pane-a".to_vec(),
        lease_epoch: 0,
        data: Vec::new(),
    }));
    let mut zero_epoch_frame = Vec::new();
    zero_epoch
        .encode_length_delimited(&mut zero_epoch_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&zero_epoch_frame),
        Err(ProtocolError::InvalidLeaseEpoch("input.lease_epoch"))
    ));

    let invalid_delta = envelope(envelope::Body::Delta(Delta {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        base_sequence: 1,
        sequence: 1,
        changes: Vec::new(),
        kitty_keyboard_active: false,
    }));
    let mut invalid_delta_frame = Vec::new();
    invalid_delta
        .encode_length_delimited(&mut invalid_delta_frame)
        .unwrap();
    assert!(matches!(
        decode_frame(&invalid_delta_frame),
        Err(ProtocolError::InvalidScreenSequence("delta.sequence"))
    ));
}

#[test]
fn decoder_accepts_maximum_payloads() {
    let maximum_input = envelope(envelope::Body::Input(Input {
        pane_id: b"pane-a".to_vec(),
        lease_epoch: 1,
        data: vec![0; MAX_INPUT_BYTES],
    }));
    let maximum_snapshot = envelope(envelope::Body::Snapshot(Snapshot {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        sequence: 1,
        screen: vec![0; MAX_SNAPSHOT_BYTES],
        kitty_keyboard_active: false,
    }));
    let maximum_delta = envelope(envelope::Body::Delta(Delta {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        base_sequence: 1,
        sequence: 2,
        changes: vec![0; MAX_DELTA_BYTES],
        kitty_keyboard_active: false,
    }));

    for envelope in [maximum_input, maximum_snapshot, maximum_delta] {
        let mut frame = Vec::new();
        envelope.encode_length_delimited(&mut frame).unwrap();
        assert_eq!(decode_frame(&frame).unwrap(), envelope);
    }
}

#[test]
fn control_lease_allows_an_empty_controller_for_a_free_pane() {
    let free_lease = envelope(envelope::Body::ControlLease(ControlLease {
        pane_id: b"pane-a".to_vec(),
        controller_peer_id: Vec::new(),
        lease_epoch: 2,
    }));
    let mut frame = Vec::new();
    free_lease.encode_length_delimited(&mut frame).unwrap();

    assert_eq!(decode_frame(&frame).unwrap(), free_lease);
}

#[test]
fn encode_frame_rejects_invalid_envelopes() {
    let mut wrong_version = sample_envelopes()[0].clone();
    wrong_version.version = PROTOCOL_VERSION + 1;
    let missing_body = Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: b"peer-a".to_vec(),
        body: None,
    };
    let empty_sender = Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: Vec::new(),
        body: sample_envelopes()[0].body.clone(),
    };
    let zero_lease = envelope(envelope::Body::ControlLease(ControlLease {
        pane_id: b"pane-a".to_vec(),
        controller_peer_id: b"peer-b".to_vec(),
        lease_epoch: 0,
    }));
    let invalid_sequence = envelope(envelope::Body::Snapshot(Snapshot {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        sequence: 0,
        screen: Vec::new(),
        kitty_keyboard_active: false,
    }));

    let cases = vec![
        ("version", wrong_version),
        ("body", missing_body),
        ("identifier", empty_sender),
        ("lease", zero_lease),
        ("sequence", invalid_sequence),
        (
            "sender peer id cap",
            Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: vec![0; MAX_PEER_ID_BYTES + 1],
                body: sample_envelopes()[0].body.clone(),
            },
        ),
        (
            "session id cap",
            envelope(envelope::Body::Join(Join {
                session_id: vec![0; MAX_SESSION_ID_BYTES + 1],
                peer_id: b"peer-a".to_vec(),
                endpoint_addr: Vec::new(),
                display_name: String::new(),
                member_kind: Default::default(),
            })),
        ),
        (
            "embedded peer id cap",
            envelope(envelope::Body::Join(Join {
                session_id: b"session-a".to_vec(),
                peer_id: vec![0; MAX_PEER_ID_BYTES + 1],
                endpoint_addr: Vec::new(),
                display_name: String::new(),
                member_kind: Default::default(),
            })),
        ),
        (
            "pane id cap",
            envelope(envelope::Body::Input(Input {
                pane_id: vec![0; MAX_PANE_ID_BYTES + 1],
                lease_epoch: 1,
                data: Vec::new(),
            })),
        ),
        (
            "input cap",
            envelope(envelope::Body::Input(Input {
                pane_id: b"pane-a".to_vec(),
                lease_epoch: 1,
                data: vec![0; MAX_INPUT_BYTES + 1],
            })),
        ),
        (
            "snapshot cap",
            envelope(envelope::Body::Snapshot(Snapshot {
                pane_id: b"pane-a".to_vec(),
                host_peer_id: b"peer-host".to_vec(),
                sequence: 1,
                screen: vec![0; MAX_SNAPSHOT_BYTES + 1],
                kitty_keyboard_active: false,
            })),
        ),
        (
            "delta cap",
            envelope(envelope::Body::Delta(Delta {
                pane_id: b"pane-a".to_vec(),
                host_peer_id: b"peer-host".to_vec(),
                base_sequence: 1,
                sequence: 2,
                changes: vec![0; MAX_DELTA_BYTES + 1],
                kitty_keyboard_active: false,
            })),
        ),
    ];

    for (name, envelope) in cases {
        assert!(encode_frame(&envelope).is_err(), "{name} must be rejected");
    }
}

#[test]
fn a_member_kind_survives_the_wire_and_an_unknown_one_reads_as_silence() {
    // The marker that separates a machine you own from a person who joined has
    // to cross the network, because the member list is the only thing that
    // does. What it must never do is turn a peer that said nothing — an older
    // build, or one whose node started before its box was paired — into a peer
    // that claimed to be a person, because that claim takes ownership away.
    let joined = Join {
        session_id: b"session-a".to_vec(),
        peer_id: b"peer-a".to_vec(),
        endpoint_addr: b"endpoint-a".to_vec(),
        display_name: String::from("droplet"),
        member_kind: MemberKind::Machine as i32,
    };
    let decoded = Join::decode(joined.encode_to_vec().as_slice()).expect("join decodes");
    assert_eq!(decoded.member_kind, MemberKind::Machine as i32);

    let member = MemberDescriptor {
        peer_id: b"peer-a".to_vec(),
        endpoint_addr: b"endpoint-a".to_vec(),
        display_name: String::from("droplet"),
        member_kind: MemberKind::Person as i32,
    };
    let decoded =
        MemberDescriptor::decode(member.encode_to_vec().as_slice()).expect("member decodes");
    assert_eq!(decoded.member_kind, MemberKind::Person as i32);

    assert_eq!(
        MemberKind::Unspecified as i32,
        0,
        "a field absent from an older peer's message decodes as zero, and zero must mean silence"
    );
    assert!(
        MemberKind::try_from(9_999).is_err(),
        "a kind this build does not know is not coerced into one it does"
    );
}

fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut encoded = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        encoded.push(byte);
        if value == 0 {
            return encoded;
        }
    }
}
