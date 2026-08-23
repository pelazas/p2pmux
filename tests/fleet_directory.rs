//! The fleet directory, against the real blind store.
//!
//! Ignored by default: these reach `rv.p2pmux.com` (or whatever
//! `P2PMUX_RENDEZVOUS_URL` names), and a suite that fails when a third party is
//! having a bad afternoon is a suite people learn to ignore. Run them by hand
//! when the directory itself is what is in question:
//!
//! ```text
//! cargo test --test fleet_directory -- --ignored --nocapture
//! ```
//!
//! The last one is the field instrument rather than a test: given a fleet key
//! out of a `pairing.toml`, it says where that fleet believes it is meeting.
//! That is the question nobody could ask on 2026-08-16, when two machines spent
//! four days chasing a session that had ended and every error said "network".

use p2pmux::fleet::{FleetKey, FleetRecord, LocateError, locate, publish, withdraw};

#[tokio::test]
#[ignore = "reaches the rendezvous service"]
async fn a_fleet_record_survives_a_round_trip_through_the_real_store() {
    let key = FleetKey::mint().expect("key should mint");
    let record = FleetRecord {
        ticket: String::from("p2pmux-v3:ROUNDTRIP"),
        host: String::from("test"),
        published_at: 1,
    };

    publish(&key, &record).await.expect("should publish");
    let found = locate(&key).await.expect("should locate");
    assert_eq!(found.ticket, record.ticket);

    withdraw(&key).await.expect("should withdraw");
    assert!(
        matches!(locate(&key).await, Err(LocateError::Nobody)),
        "a withdrawn record must read as nobody hosting, not as an error"
    );
}

#[tokio::test]
#[ignore = "reaches the rendezvous service"]
async fn a_fleet_nobody_has_ever_published_reads_as_quiet() {
    // The state a fresh fleet is in, and the one the agent must not mistake for
    // a failure: it waits rather than backing off, and never invents a session.
    let key = FleetKey::mint().expect("key should mint");

    assert!(matches!(locate(&key).await, Err(LocateError::Nobody)));
}

#[tokio::test]
#[ignore = "reaches the rendezvous service; needs P2PMUX_FLEET_KEY"]
async fn where_is_this_fleet_meeting() {
    let Ok(key) = std::env::var("P2PMUX_FLEET_KEY") else {
        panic!("set P2PMUX_FLEET_KEY to the fleet_key out of a pairing.toml");
    };
    let key = FleetKey::parse(&key).expect("that is not a fleet key");

    match locate(&key).await {
        Ok(record) => println!(
            "meeting at {} (published by {} at {})",
            record.ticket, record.host, record.published_at
        ),
        Err(error) => println!("{error}"),
    }
}
