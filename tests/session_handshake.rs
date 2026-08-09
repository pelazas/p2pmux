use std::{net::Ipv4Addr, str::FromStr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    protocol::{Envelope, Join, PROTOCOL_VERSION, envelope},
    session::{HostSession, SessionError, join_once},
    ticket::JoinTicket,
    transport::{ALPN, Transport},
};
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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

async fn rejected_raw_join_endpoint(host: &HostSession, endpoint_addr: Vec<u8>) -> SessionError {
    let host_task = {
        let host = host.clone();
        tokio::spawn(async move { host.accept_one_join().await })
    };
    let guest = loopback_transport().await;
    let guest_id = guest.endpoint_id().as_bytes().to_vec();
    let connection = guest
        .connect(host.ticket().endpoint_addr().clone())
        .await
        .expect("connect");
    let (mut send, _recv) = guest.open_bi(&connection).await.expect("open Join stream");
    guest
        .write_frame(
            &mut send,
            &Envelope {
                version: PROTOCOL_VERSION,
                sender_peer_id: guest_id.clone(),
                body: Some(envelope::Body::Join(Join {
                    session_id: host.ticket().session_id().to_vec(),
                    peer_id: guest_id,
                    endpoint_addr,
                    display_name: String::new(),
                    member_kind: Default::default(),
                })),
            },
        )
        .await
        .expect("write raw Join");
    let error = timeout(TEST_TIMEOUT, host_task)
        .await
        .expect("host should reject Join")
        .expect("host task should complete")
        .expect_err("Join endpoint must be rejected");
    connection.close(0u8.into(), b"");
    guest.close().await;
    error
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_ticket_admits_two_joiners_in_separate_handshakes() {
    let host_transport = loopback_transport().await;
    let host = HostSession::from_transport(host_transport).expect("host should be created");
    let ticket_text = host.ticket().to_string();
    let first_ticket = JoinTicket::from_str(&ticket_text).expect("ticket should parse");
    let second_ticket = JoinTicket::from_str(&ticket_text).expect("ticket should parse");
    assert_eq!(first_ticket, second_ticket);

    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let first = timeout(TEST_TIMEOUT, host.accept_one_join())
                .await
                .expect("first accept should not time out")
                .expect("first join should succeed");
            let second = timeout(TEST_TIMEOUT, host.accept_one_join())
                .await
                .expect("second accept should not time out")
                .expect("second join should succeed");
            (first, second)
        })
    };

    let first_transport = loopback_transport().await;
    let first_id = first_transport.endpoint_id().as_bytes().to_vec();
    let first_receipt = timeout(TEST_TIMEOUT, join_once(first_transport, first_ticket))
        .await
        .expect("first join should not time out")
        .expect("first join should succeed");

    let second_transport = loopback_transport().await;
    let second_id = second_transport.endpoint_id().as_bytes().to_vec();
    let second_receipt = timeout(TEST_TIMEOUT, join_once(second_transport, second_ticket))
        .await
        .expect("second join should not time out")
        .expect("second join should succeed");

    let (first_host_receipt, second_host_receipt) =
        host_task.await.expect("host task should complete");
    let session_id = host.ticket().session_id().to_vec();
    let coordinator = host.ticket().endpoint_addr().id.as_bytes().to_vec();

    assert_eq!(first_receipt.session_id, session_id);
    assert_eq!(second_receipt.session_id, session_id);
    assert_eq!(first_host_receipt.admitted_peer_id, first_id);
    assert_eq!(second_host_receipt.admitted_peer_id, second_id);
    assert_eq!(
        first_host_receipt.endpoint_addr.id.as_bytes().as_slice(),
        first_host_receipt.admitted_peer_id
    );
    assert_eq!(
        second_host_receipt.endpoint_addr.id.as_bytes().as_slice(),
        second_host_receipt.admitted_peer_id
    );
    assert_eq!(first_receipt.coordinator_peer_id, coordinator);
    assert_eq!(second_receipt.coordinator_peer_id, coordinator);
    assert_ne!(
        first_host_receipt.admitted_peer_id,
        second_host_receipt.admitted_peer_id
    );

    host.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_rejects_malformed_or_mismatched_join_endpoint_addresses() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");

    for endpoint_addr in [
        b"not an endpoint address".to_vec(),
        serde_json::to_vec(host.ticket().endpoint_addr())
            .expect("endpoint address should serialize"),
    ] {
        assert!(matches!(
            rejected_raw_join_endpoint(&host, endpoint_addr).await,
            SessionError::InvalidJoinEndpointAddress
        ));
    }

    host.close().await;
}
