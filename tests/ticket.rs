use std::{
    fs,
    net::SocketAddr,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{EndpointAddr, SecretKey};
use p2pmux::rendezvous::{LocalRendezvous, RendezvousError, SHORT_CODE_LEN};
use p2pmux::ticket::{
    JoinTicket, MAX_TICKET_PAYLOAD_BYTES, TICKET_PREFIX, TICKET_PREFIX_V2, TICKET_PREFIX_V3,
    TicketError,
};
use serde_json::{Value, json};

fn endpoint_addr() -> EndpointAddr {
    EndpointAddr::new(SecretKey::from_bytes(&[7; 32]).public())
        .with_ip_addr(SocketAddr::from(([127, 0, 0, 1], 4242)))
}

/// A well-formed v1 body. Built directly rather than from `Display`, which now emits v2.
fn valid_payload() -> Value {
    json!({
        "version": 1,
        "session_id": endpoint_addr().id.as_bytes().to_vec(),
        "endpoint_addr": endpoint_addr(),
    })
}

fn encode_payload(payload: Value) -> String {
    format!(
        "{TICKET_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload should encode"))
    )
}

#[test]
fn minted_ticket_round_trips_and_is_reusable() {
    let endpoint_addr = endpoint_addr();
    let ticket = JoinTicket::mint(endpoint_addr.clone()).expect("ticket should mint");
    let text = ticket.to_string();
    let first = JoinTicket::from_str(&text).expect("ticket should parse");
    let second = JoinTicket::from_str(&text).expect("ticket should parse repeatedly");

    assert_eq!(first, ticket);
    assert_eq!(second, ticket);
    assert!(text.starts_with(TICKET_PREFIX_V3));
    assert_eq!(ticket.endpoint_addr(), &endpoint_addr);
}

#[test]
fn a_minted_ticket_carries_a_secret_the_endpoint_id_does_not_reveal() {
    // The whole point of v3. While session_id *was* the endpoint public key, the ticket
    // granted nothing: an endpoint id is presented in every TLS handshake and published to
    // discovery, so anyone who learned it could mint a working ticket for the session.
    let endpoint_addr = endpoint_addr();
    let ticket = JoinTicket::mint(endpoint_addr.clone()).expect("ticket should mint");

    assert_ne!(
        ticket.session_id().as_slice(),
        endpoint_addr.id.as_bytes().as_slice(),
        "the join secret must not be derivable from the coordinator's public key"
    );
    assert!(!ticket.secret_is_public());
}

#[test]
fn two_tickets_for_the_same_endpoint_carry_different_secrets() {
    // Otherwise the "secret" would just be a slow way of naming the endpoint again.
    let first = JoinTicket::mint(endpoint_addr()).expect("first ticket should mint");
    let second = JoinTicket::mint(endpoint_addr()).expect("second ticket should mint");

    assert_ne!(first.session_id(), second.session_id());
}

#[test]
fn v1_and_v2_tickets_still_parse_but_are_flagged_as_carrying_no_secret() {
    // They must keep parsing so a stale ticket produces a join refusal rather than a
    // parser error, but nothing should mistake them for a credential.
    let v1 = JoinTicket::from_str(&encode_payload(valid_payload()))
        .expect("a v1 ticket should still parse");
    assert_eq!(v1.endpoint_addr(), &endpoint_addr());
    assert!(v1.secret_is_public());

    let v2 = JoinTicket::from_str(&format!(
        "{TICKET_PREFIX_V2}{}",
        URL_SAFE_NO_PAD.encode(
            postcard::to_allocvec(&endpoint_addr()).expect("endpoint address should encode")
        )
    ))
    .expect("a v2 ticket should still parse");
    assert!(v2.secret_is_public());
}

#[test]
fn v3_stays_far_shorter_than_the_v1_encoding_of_the_same_session() {
    // v3 pays 32 bytes more than v2 for a real secret. That must not undo the win over
    // v1, which restated session_id as 32 decimal numbers inside JSON.
    let ticket = JoinTicket::mint(endpoint_addr()).expect("ticket should mint");

    let v3 = ticket.to_string();
    let v1 = encode_payload(valid_payload());

    assert!(
        v3.len() * 2 < v1.len(),
        "v3 ({} chars) should be under half of v1 ({} chars)",
        v3.len(),
        v1.len()
    );
}

#[test]
fn parser_rejects_invalid_ticket_classes_without_echoing_input() {
    let valid = valid_payload();
    let mut short_session = valid.clone();
    short_session["session_id"] = json!(vec![7; 31]);
    let mut empty_addresses = valid.clone();
    empty_addresses["endpoint_addr"]["addrs"] = json!([]);

    let oversized = format!(
        "{TICKET_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(vec![0; MAX_TICKET_PAYLOAD_BYTES + 1])
    );
    let cases = [
        ("", TicketError::MissingPrefix),
        ("other-v1:abc", TicketError::MissingPrefix),
        ("p2pmux-v1:%%%", TicketError::MalformedBase64),
        (
            &encode_payload(json!({"not": "a ticket"})),
            TicketError::MalformedPayload,
        ),
        (
            &encode_payload({
                let mut payload = valid.clone();
                payload["version"] = json!(2);
                payload
            }),
            TicketError::UnsupportedVersion,
        ),
        (
            &encode_payload(short_session),
            TicketError::InvalidSessionId,
        ),
        (
            &encode_payload(empty_addresses),
            TicketError::MissingAddresses,
        ),
        (&oversized, TicketError::PayloadTooLarge),
        ("p2pmux-v2:%%%", TicketError::MalformedBase64),
        (
            &format!("{TICKET_PREFIX_V2}{}", URL_SAFE_NO_PAD.encode([0_u8; 4])),
            TicketError::MalformedPayload,
        ),
        (
            &format!(
                "{TICKET_PREFIX_V2}{}",
                URL_SAFE_NO_PAD.encode(
                    postcard::to_allocvec(&EndpointAddr::new(
                        SecretKey::from_bytes(&[7; 32]).public()
                    ))
                    .expect("addressless endpoint should encode")
                )
            ),
            TicketError::MissingAddresses,
        ),
    ];

    for (input, expected) in cases {
        let error = JoinTicket::from_str(input).expect_err("ticket should be rejected");
        assert_eq!(error.class(), expected.class());
        if !input.is_empty() {
            assert!(!error.to_string().contains(input));
        }
    }
}

#[test]
fn local_rendezvous_codes_are_short_reusable_and_resolve_the_ticket() {
    let directory = temporary_directory();
    let store = LocalRendezvous::at(directory.clone());
    let ticket = JoinTicket::mint(endpoint_addr()).expect("ticket should mint");

    let entry = store.publish(&ticket).expect("ticket should publish");
    assert_eq!(entry.code().len(), SHORT_CODE_LEN);
    assert!(entry.code().chars().all(|character| {
        matches!(character, '2'..='9' | 'A'..='H' | 'J'..='K' | 'M'..='N' | 'P'..='Z')
    }));
    assert_eq!(
        store.resolve(entry.code()).expect("code should resolve"),
        ticket
    );
    assert_eq!(
        store
            .resolve(entry.code())
            .expect("code should be reusable"),
        ticket
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(entry.path())
                .expect("entry metadata")
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }

    entry.remove().expect("entry should remove");
    fs::remove_dir_all(directory).expect("temporary directory should remove");
}

#[test]
fn local_rendezvous_rejects_unknown_codes_without_echoing_them() {
    let directory = temporary_directory();
    fs::create_dir_all(&directory).expect("temporary directory should create");
    let store = LocalRendezvous::at(directory.clone());
    let code = "ABCDEFGHJK";

    let error = store.resolve(code).expect_err("unknown code should fail");
    assert!(matches!(error, RendezvousError::NotFound));
    assert!(!error.to_string().contains(code));

    fs::remove_dir_all(directory).expect("temporary directory should remove");
}

#[test]
fn local_rendezvous_lists_published_codes_and_ignores_other_files() {
    let directory = temporary_directory();
    let store = LocalRendezvous::at(directory.clone());
    let ticket = JoinTicket::mint(endpoint_addr()).expect("ticket should mint");

    let first = store.publish(&ticket).expect("first ticket should publish");
    let second = store
        .publish(&ticket)
        .expect("second ticket should publish");
    fs::write(directory.join("not-a-code"), b"ignored").expect("stray file should write");

    let mut expected = vec![first.code().to_owned(), second.code().to_owned()];
    expected.sort();
    assert_eq!(store.codes().expect("codes should list"), expected);

    drop(first);
    drop(second);
    fs::remove_dir_all(directory).expect("temporary directory should remove");
}

#[test]
fn local_rendezvous_lists_no_codes_before_any_session_exists() {
    let store = LocalRendezvous::at(temporary_directory());

    assert!(
        store
            .codes()
            .expect("a missing directory should not be an error")
            .is_empty()
    );
}

static TEMPORARY_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_directory() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let sequence = TEMPORARY_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "p2pmux-rendezvous-test-{}-{nonce}-{sequence}",
        std::process::id()
    ))
}
