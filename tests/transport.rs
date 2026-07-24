use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::transport::{ALPN, Transport};
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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
