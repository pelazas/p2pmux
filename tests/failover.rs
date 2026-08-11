//! A session outliving the coordinator that started it, over real endpoints.
//!
//! The unit tests elsewhere cover the two halves in isolation: `failover` decides whose turn
//! it is, `ledger` decides whose signature to believe. What they cannot show is the thing
//! that actually has to work -- that a member can pick the role up and the room can find it
//! again -- because that involves minting a ticket on an existing session id, resuming a
//! chain mid-flight, and a second member dialling an address it was never handed. So these
//! run over loopback endpoints and dial for real.

use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    ledger::LedgerVerifier,
    protocol::{LedgerEntryKind, PROTOCOL_VERSION},
    session::{
        DEFAULT_RESERVATION_TIMEOUT, HostSession, LayoutControlEvent, SharedLayoutHost,
        SharedLayoutMember, join_layout_with_display_name, layout_snapshot_from_state,
    },
    ticket::JoinTicket,
    transport::{ALPN, Transport},
};
use prost::Message;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn loopback_transport() -> Transport {
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("localhost address should be valid")
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .expect("loopback endpoint should bind");
    Transport::from_endpoint(endpoint)
}

/// A coordinator with its accept loop running, as a live session would have.
async fn coordinator(transport: Transport) -> (SharedLayoutHost, tokio::task::JoinHandle<()>) {
    let host = SharedLayoutHost::with_display_name(
        HostSession::from_transport_with_session_name(transport, String::from("lisbon"))
            .expect("host session"),
        String::from("coordinator"),
        24,
        80,
    )
    .expect("shared layout host");
    let panes = host.pane_server();
    let dispatcher = host
        .incoming_dispatcher(panes)
        .expect("dispatcher should accept this host's own pane server");
    let task = tokio::spawn(async move {
        let _ = dispatcher.accept_loop().await;
    });
    (host, task)
}

/// Join a coordinator and take delivery of the layout it answers with.
async fn join(
    transport: Transport,
    ticket: &JoinTicket,
    display_name: &str,
) -> (SharedLayoutMember, p2pmux::protocol::LayoutState) {
    let mut member = timeout(
        TEST_TIMEOUT,
        join_layout_with_display_name(transport, ticket.clone(), display_name.to_owned()),
    )
    .await
    .expect("join should not time out")
    .expect("join should be admitted");
    let state = match timeout(TEST_TIMEOUT, member.events.recv())
        .await
        .expect("first control event should not time out")
    {
        Some(LayoutControlEvent::Snapshot(snapshot)) => {
            snapshot.state.expect("welcome carries a layout")
        }
        other => panic!("expected an opening snapshot, got {other:?}"),
    };
    (member, state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_promoted_member_serves_the_same_session_the_coordinator_left_behind() {
    let coordinator_transport = loopback_transport().await;
    let (host, host_task) = coordinator(coordinator_transport.clone()).await;
    let ticket = host.ticket().clone();
    let session_id = ticket.session_id().to_vec();

    let first_transport = loopback_transport().await;
    let (first, state) = join(first_transport.clone(), &ticket, "first").await;
    let second_transport = loopback_transport().await;
    let (second, _) = join(second_transport.clone(), &ticket, "second").await;
    // The second join moves the layout on, so promote from what the coordinator holds rather
    // than from the older copy the first member happened to be sent.
    let state = host
        .session_snapshot()
        .expect("coordinator layout")
        .state
        .unwrap_or(state);
    let before = layout_snapshot_from_state(&state).expect("committed layout");
    assert_eq!(before.members.len(), 3);
    assert_eq!(first.coordinator_epoch, 0, "nothing has changed hands yet");

    // The coordinator goes away without saying anything, which is the case that matters:
    // a clean exit would have removed it from the roster on its way out.
    host_task.abort();
    coordinator_transport.close().await;
    second.disconnect_control();

    let head = first.ledger_head();
    let promoted = SharedLayoutHost::take_over(
        HostSession::resume_with_session_name(
            first_transport.clone(),
            session_id.clone(),
            first.session_name.clone(),
            first.coordinator_epoch + 1,
        )
        .expect("a member can mint a ticket on the session id it already holds"),
        first
            .pane_server(session_id.clone())
            .expect("the member's own pane service"),
        before.clone(),
        head,
        DEFAULT_RESERVATION_TIMEOUT,
    )
    .expect("a member in the layout can take it over");
    let panes = promoted.pane_server();
    let dispatcher = promoted
        .incoming_dispatcher(panes)
        .expect("dispatcher for the promoted host");
    let promoted_task = tokio::spawn(async move {
        let _ = dispatcher.accept_loop().await;
    });

    // Nothing handed this ticket out. It is built from the session id every member already
    // holds plus the successor's address out of the layout, which is the whole reason a
    // takeover needs no new protocol.
    let rejoin_ticket = JoinTicket::from_parts(
        session_id.clone(),
        promoted.ticket().endpoint_addr().clone(),
    )
    .expect("a ticket for the new coordinator");
    assert_ne!(
        rejoin_ticket.endpoint_addr().id,
        ticket.endpoint_addr().id,
        "the rebuilt ticket must name the successor, not the coordinator that left"
    );
    assert_eq!(rejoin_ticket.session_id(), ticket.session_id());

    let (returning, after) = join(second_transport.clone(), &rejoin_ticket, "second").await;

    assert_eq!(
        returning.coordinator_epoch, 1,
        "the welcome states which coordinator this member is now following"
    );
    let after = layout_snapshot_from_state(&after).expect("layout from the successor");
    assert_eq!(
        after
            .members
            .iter()
            .map(|member| member.peer_id.clone())
            .collect::<Vec<_>>(),
        before
            .members
            .iter()
            .map(|member| member.peer_id.clone())
            .collect::<Vec<_>>(),
        "the room is looking at the same session, in the same join order"
    );
    assert_eq!(after.panes.len(), before.panes.len());
    assert!(
        after.revision >= before.revision,
        "a takeover must not rewind the layout"
    );

    promoted_task.abort();
    returning.disconnect_control();
    first_transport.close().await;
    second_transport.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_successor_carries_on_the_ledger_rather_than_starting_a_second_one() {
    let coordinator_transport = loopback_transport().await;
    let (host, host_task) = coordinator(coordinator_transport.clone()).await;
    let ticket = host.ticket().clone();
    let session_id = ticket.session_id().to_vec();

    let first_transport = loopback_transport().await;
    let (first, _) = join(first_transport.clone(), &ticket, "first").await;
    let state = host
        .session_snapshot()
        .expect("coordinator layout")
        .state
        .expect("layout state");
    let before = layout_snapshot_from_state(&state).expect("committed layout");

    host_task.abort();
    coordinator_transport.close().await;

    let (next_seq, prev_hash) = first.ledger_head();
    let promoted = SharedLayoutHost::take_over(
        HostSession::resume_with_session_name(
            first_transport.clone(),
            session_id.clone(),
            String::from("lisbon"),
            1,
        )
        .expect("host session"),
        first
            .pane_server(session_id.clone())
            .expect("pane service")
            .clone(),
        before,
        (next_seq, prev_hash),
        DEFAULT_RESERVATION_TIMEOUT,
    )
    .expect("takeover");

    let commit = promoted
        .announce_takeover(ticket.endpoint_addr().id.as_bytes().to_vec())
        .expect("a takeover is recorded like any other change");
    let entry = commit.entry.expect("a commit carries its entry");

    assert_eq!(
        entry.seq, next_seq,
        "the successor picks up where this member had got to, not at 1"
    );
    assert_eq!(entry.prev_hash, prev_hash.to_vec());
    assert_eq!(entry.coordinator_epoch, 1);
    assert_eq!(entry.kind, LedgerEntryKind::CoordinatorChange as i32);

    let record = p2pmux::protocol::CoordinatorChangeRecord::decode(entry.payload.as_slice())
        .expect("the payload is a coordinator-change record");
    assert_eq!(
        record.replaced_peer_id,
        ticket.endpoint_addr().id.as_bytes().to_vec()
    );
    assert_eq!(record.epoch, 1);

    // A member that made the same decision independently, and so rotated onto this key,
    // accepts what it seals. That is the only thing that makes the entry believable: the
    // coordinator it replaces never signed anything approving of it.
    let mut rotated = LedgerVerifier::new(session_id.clone(), ticket.endpoint_addr().id);
    rotated
        .rotate(promoted.ticket().endpoint_addr().id, 1)
        .expect("the member decided on this successor");
    assert_eq!(rotated.accept(&entry), Ok(()));

    // One that did not rotate refuses it, and says why: wrong coordinator, not a forgery.
    let mut following_the_old_one = LedgerVerifier::new(session_id, ticket.endpoint_addr().id);
    assert_eq!(
        following_the_old_one.accept(&entry),
        Err(p2pmux::ledger::LedgerError::WrongEpoch {
            current: 0,
            found: 1
        })
    );

    first_transport.close().await;
}

#[test]
fn the_wire_version_gates_peers_that_cannot_read_an_epoch() {
    // A v9 peer parses the new welcome and the new entries, and silently reads every epoch
    // as 0 -- which is exactly the confusion the epoch exists to prevent. The version bump
    // is what keeps it out.
    assert_eq!(PROTOCOL_VERSION, 12);
}
