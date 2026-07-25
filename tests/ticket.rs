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
use p2pmux::ticket::{JoinTicket, MAX_TICKET_PAYLOAD_BYTES, TICKET_PREFIX, TicketError};
use serde_json::{Value, json};

fn endpoint_addr() -> EndpointAddr {
    EndpointAddr::new(SecretKey::from_bytes(&[7; 32]).public())
        .with_ip_addr(SocketAddr::from(([127, 0, 0, 1], 4242)))
}

fn valid_payload() -> Value {
    let ticket = JoinTicket::mint(endpoint_addr()).expect("ticket should mint");
    let text = ticket.to_string();
    let encoded = text
        .strip_prefix(TICKET_PREFIX)
        .expect("ticket should have the version prefix");
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("ticket should decode"),
    )
    .expect("ticket should contain JSON")
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
    assert!(text.starts_with(TICKET_PREFIX));
    assert_eq!(ticket.endpoint_addr(), &endpoint_addr);
    assert_eq!(ticket.session_id(), endpoint_addr.id.as_bytes());
}

#[test]
fn parser_rejects_invalid_ticket_classes_without_echoing_input() {
    let valid = valid_payload();
    let mut short_session = valid.clone();
    short_session["session_id"] = json!(vec![7; 31]);
    let mut mismatched_session = valid.clone();
    mismatched_session["session_id"] = json!(vec![8; 32]);
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
            &encode_payload(mismatched_session),
            TicketError::SessionMismatch,
        ),
        (
            &encode_payload(empty_addresses),
            TicketError::MissingAddresses,
        ),
        (&oversized, TicketError::PayloadTooLarge),
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
