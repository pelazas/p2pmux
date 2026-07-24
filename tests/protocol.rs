use p2pmux::protocol::{
    ControlLease, Delta, Envelope, Input, Join, MAX_DELTA_BYTES, MAX_ENVELOPE_BYTES,
    MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_PANE_ID_BYTES, MAX_PEER_ID_BYTES, MAX_SESSION_ID_BYTES,
    MAX_SNAPSHOT_BYTES, PROTOCOL_VERSION, ProtocolError, Snapshot, TakeControl, Welcome,
    decode_frame, encode_frame, envelope,
};
use prost::Message;

#[derive(Debug)]
struct ParsedField {
    field_number: u32,
    wire_type: u8,
    value: Vec<u8>,
}

fn envelope(body: envelope::Body) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: b"peer-a".to_vec(),
        body: Some(body),
    }
}

#[test]
fn envelope_exposes_each_v1_body() {
    let messages = [
        envelope(envelope::Body::Join(Join {
            session_id: b"session-a".to_vec(),
            peer_id: b"peer-a".to_vec(),
        })),
        envelope(envelope::Body::Welcome(Welcome {
            session_id: b"session-a".to_vec(),
            admitted_peer_id: b"peer-a".to_vec(),
            coordinator_peer_id: b"peer-host".to_vec(),
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
            force: true,
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
        })),
        envelope(envelope::Body::Delta(Delta {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            base_sequence: u32::MAX as u64 + 4,
            sequence: u32::MAX as u64 + 5,
            changes: b"patch".to_vec(),
        })),
    ];

    let expected_body_shapes: [&[(u32, u8)]; 7] = [
        &[(1, 2), (2, 2)],
        &[(1, 2), (2, 2), (3, 2)],
        &[(1, 2), (2, 0), (3, 2)],
        &[(1, 2), (2, 2), (3, 0), (4, 0)],
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
        })),
        envelope(envelope::Body::Welcome(Welcome {
            session_id: b"session-a".to_vec(),
            admitted_peer_id: b"peer-a".to_vec(),
            coordinator_peer_id: b"peer-host".to_vec(),
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
            force: false,
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
        })),
        envelope(envelope::Body::Delta(Delta {
            pane_id: b"pane-a".to_vec(),
            host_peer_id: b"peer-host".to_vec(),
            base_sequence: 1,
            sequence: 2,
            changes: b"patch".to_vec(),
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
fn decoder_rejects_unsupported_version() {
    let mut wrong = sample_envelopes().remove(0);
    wrong.version = PROTOCOL_VERSION + 1;
    let mut frame = Vec::new();
    wrong.encode_length_delimited(&mut frame).unwrap();

    assert!(matches!(
        decode_frame(&frame),
        Err(ProtocolError::UnsupportedVersion(v)) if v == PROTOCOL_VERSION + 1
    ));
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
    }));
    let maximum_delta = envelope(envelope::Body::Delta(Delta {
        pane_id: b"pane-a".to_vec(),
        host_peer_id: b"peer-host".to_vec(),
        base_sequence: 1,
        sequence: 2,
        changes: vec![0; MAX_DELTA_BYTES],
    }));

    for envelope in [maximum_input, maximum_snapshot, maximum_delta] {
        let mut frame = Vec::new();
        envelope.encode_length_delimited(&mut frame).unwrap();
        assert_eq!(decode_frame(&frame).unwrap(), envelope);
    }
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
            })),
        ),
        (
            "embedded peer id cap",
            envelope(envelope::Body::Join(Join {
                session_id: b"session-a".to_vec(),
                peer_id: vec![0; MAX_PEER_ID_BYTES + 1],
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
            })),
        ),
    ];

    for (name, envelope) in cases {
        assert!(encode_frame(&envelope).is_err(), "{name} must be rejected");
    }
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
