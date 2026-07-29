use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::transport::{ALPN, PathKind, PeerPath, Transport, link_summary};
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn transport_uses_v2_alpn() {
    assert_eq!(ALPN, b"p2pmux/2");
}

async fn loopback_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("localhost address should be valid")
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .expect("loopback endpoint should bind")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_connects_accepts_and_opens_a_bi_stream_on_localhost() {
    let host = Transport::from_endpoint(loopback_endpoint().await);
    let client = Transport::from_endpoint(loopback_endpoint().await);
    let client_id = client.endpoint_id();
    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let connection = timeout(TEST_TIMEOUT, host.accept_connection())
                .await
                .expect("accept should not time out")
                .expect("connection should be accepted");
            assert_eq!(connection.remote_id(), client_id);
            let (_send, mut recv) = timeout(TEST_TIMEOUT, connection.accept_bi())
                .await
                .expect("accept_bi should not time out")
                .expect("bi-stream should be accepted");
            let received = timeout(TEST_TIMEOUT, recv.read_to_end(4))
                .await
                .expect("read should not time out")
                .expect("stream should read");
            assert_eq!(received, b"ping");
        })
    };

    let connection = timeout(TEST_TIMEOUT, client.connect(host.endpoint_addr()))
        .await
        .expect("connect should not time out")
        .expect("connection should succeed");
    let (mut send, _recv) = timeout(TEST_TIMEOUT, connection.open_bi())
        .await
        .expect("open_bi should not time out")
        .expect("bi-stream should open");
    timeout(TEST_TIMEOUT, send.write_all(b"ping"))
        .await
        .expect("write should not time out")
        .expect("stream should write");
    send.finish().expect("stream should finish");

    host_task.await.expect("host task should complete");
    client.close().await;
    host.close().await;
}

#[test]
fn link_summary_reports_the_worst_path_not_an_average() {
    // One peer stuck on a relay is the fact worth surfacing; averaging with the healthy
    // direct peers would hide exactly the case the badge exists for.
    let paths = vec![
        PeerPath {
            peer_id: vec![1; 32],
            kind: PathKind::Direct,
            rtt_ms: Some(10),
        },
        PeerPath {
            peer_id: vec![2; 32],
            kind: PathKind::Relayed,
            rtt_ms: Some(120),
        },
        PeerPath {
            peer_id: vec![3; 32],
            kind: PathKind::Direct,
            rtt_ms: Some(400),
        },
    ];
    assert_eq!(link_summary(&paths).as_deref(), Some("relayed 120ms ×3"));
}

#[test]
fn link_summary_omits_the_count_for_a_single_peer() {
    let paths = vec![PeerPath {
        peer_id: vec![7; 32],
        kind: PathKind::Direct,
        rtt_ms: Some(35),
    }];
    assert_eq!(link_summary(&paths).as_deref(), Some("direct 35ms"));
}

#[test]
fn link_summary_is_absent_when_nothing_is_connected() {
    // A stale `direct 30ms` beside a dead session reads as working connectivity.
    assert_eq!(link_summary(&[]), None);
}

#[test]
fn link_summary_reports_a_kind_before_any_rtt_estimate_exists() {
    let paths = vec![PeerPath {
        peer_id: vec![9; 32],
        kind: PathKind::Relayed,
        rtt_ms: None,
    }];
    assert_eq!(link_summary(&paths).as_deref(), Some("relayed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_connection_is_reported_as_a_direct_path_on_both_sides() {
    let host = Transport::from_endpoint(loopback_endpoint().await);
    let client = Transport::from_endpoint(loopback_endpoint().await);
    let host_id = host.endpoint_id();
    let client_id = client.endpoint_id();

    let host_task = {
        let host = host.clone();
        tokio::spawn(async move {
            let connection = timeout(TEST_TIMEOUT, host.accept_connection())
                .await
                .expect("accept should not time out")
                .expect("connection should be accepted");
            // Hold the connection open while both sides are sampled.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            drop(connection);
        })
    };

    let connection = timeout(TEST_TIMEOUT, client.connect(host.endpoint_addr()))
        .await
        .expect("connect should not time out")
        .expect("connection should succeed");

    // The observer samples on a 1s tick, so allow one full period.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let client_paths = client.paths();
    assert_eq!(client_paths.len(), 1, "client should see exactly one peer");
    assert_eq!(client_paths[0].peer_id, host_id.as_bytes().to_vec());
    assert_eq!(
        client_paths[0].kind,
        PathKind::Direct,
        "a loopback endpoint with relays disabled can only be direct"
    );

    let host_paths = host.paths();
    assert_eq!(host_paths.len(), 1, "accepted connections must be observed too");
    assert_eq!(host_paths[0].peer_id, client_id.as_bytes().to_vec());
    assert_eq!(host_paths[0].kind, PathKind::Direct);

    drop(connection);
    host_task.await.expect("host task should complete");
    client.close().await;
    host.close().await;
}

#[test]
fn a_sub_millisecond_path_reports_under_one_ms_rather_than_zero() {
    // A loopback or same-LAN link genuinely measures under a millisecond. Printing
    // `direct 0ms` reads as a broken measurement instead of a very good path -- and it
    // was, briefly, exactly what a localhost session displayed.
    let paths = vec![PeerPath {
        peer_id: vec![5; 32],
        kind: PathKind::Direct,
        rtt_ms: Some(0),
    }];
    assert_eq!(link_summary(&paths).as_deref(), Some("direct <1ms"));
}
