//! Live tests against a deployed blind store.
//!
//! These are skipped unless `P2PMUX_RENDEZVOUS_URL` is set, so CI stays hermetic and a
//! developer without network access is not blocked. Run them against a real deploy with:
//!
//! ```sh
//! P2PMUX_RENDEZVOUS_URL=https://rv.p2pmux.com cargo test --test hosted_rendezvous
//! ```
//!
//! The crypto is covered offline in `src/hosted_rendezvous.rs`. What only a live store can
//! show is the wire contract: that the index is accepted in the shape the client derives,
//! that a missing record is distinguishable from a wrong code, and that a delete sticks.

use p2pmux::hosted_rendezvous::{HostedRendezvous, JoinCode, ResolveError};

/// `None` when the endpoint is unset, which the caller reports as a skip.
fn store() -> Option<HostedRendezvous> {
    let endpoint = std::env::var("P2PMUX_RENDEZVOUS_URL").ok()?;
    Some(HostedRendezvous::at(endpoint).expect("client should build"))
}

macro_rules! live {
    ($store:ident) => {
        match store() {
            Some(value) => value,
            None => {
                eprintln!("skipped: set P2PMUX_RENDEZVOUS_URL to run against a deploy");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_published_ticket_comes_back_through_its_code() {
    let store = live!(store);
    let ticket = "p2pmux-v3:LIVE-ROUND-TRIP";

    let code = store.publish(ticket).await.expect("publish should succeed");
    let resolved = store.resolve(&code).await.expect("resolve should succeed");
    assert_eq!(resolved, ticket);

    store.remove(&code).await.expect("remove should succeed");
    assert!(matches!(
        store.resolve(&code).await,
        Err(ResolveError::NotFound)
    ));
}

#[tokio::test]
async fn a_code_that_was_never_published_is_not_found() {
    let store = live!(store);
    let code = JoinCode::mint().expect("code should mint");

    assert!(matches!(
        store.resolve(&code).await,
        Err(ResolveError::NotFound)
    ));
}

#[tokio::test]
async fn republishing_resets_the_record_without_changing_the_code() {
    // The node re-seals on a timer to hold the TTL open, so the same code has to keep
    // resolving across refreshes -- with a fresh nonce each time, since a repeated nonce
    // under one key is the failure that breaks GCM outright.
    let store = live!(store);
    let code = store
        .publish("p2pmux-v3:FIRST")
        .await
        .expect("publish should succeed");

    store
        .republish(&code, "p2pmux-v3:SECOND")
        .await
        .expect("republish should succeed");

    assert_eq!(
        store.resolve(&code).await.expect("resolve should succeed"),
        "p2pmux-v3:SECOND"
    );
    store.remove(&code).await.expect("remove should succeed");
}

#[tokio::test]
async fn a_wrong_code_does_not_open_someone_else_s_record() {
    let store = live!(store);
    let code = store
        .publish("p2pmux-v3:PRIVATE")
        .await
        .expect("publish should succeed");

    // Same store, different code: this lands on a different index entirely, which is what
    // makes guessing an index useless without the code that derived it.
    let other = JoinCode::mint().expect("code should mint");
    assert!(matches!(
        store.resolve(&other).await,
        Err(ResolveError::NotFound)
    ));

    store.remove(&code).await.expect("remove should succeed");
}

#[tokio::test]
async fn errors_never_echo_the_index_the_code_derived() {
    // reqwest puts the request URL in its error strings and our URL carries the index, so a
    // connection failure would otherwise print a value derived from the credential straight
    // into a shared pane.
    let store = HostedRendezvous::at("https://127.0.0.1:1").expect("client should build");
    let code = JoinCode::mint().expect("code should mint");

    let error = store
        .resolve(&code)
        .await
        .expect_err("a closed port should fail");

    let message = error.to_string();
    assert!(!message.contains(&code.index()), "leaked index: {message}");
    assert!(
        !message.contains(code.canonical()),
        "leaked code: {message}"
    );
}
