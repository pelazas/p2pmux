use p2pmux::protocol::{
    ControlLease, Delta, Envelope, Input, Join, PROTOCOL_VERSION, Snapshot, TakeControl, Welcome,
    envelope,
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
