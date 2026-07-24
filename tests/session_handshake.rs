use std::{net::Ipv4Addr, str::FromStr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    session::{HostSession, join_once},
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
    assert_eq!(first_receipt.coordinator_peer_id, coordinator);
    assert_eq!(second_receipt.coordinator_peer_id, coordinator);
    assert_ne!(
        first_host_receipt.admitted_peer_id,
        second_host_receipt.admitted_peer_id
    );

    host.close().await;
}
