use std::{net::Ipv4Addr, time::Duration};

use iroh::{Endpoint, RelayMode, endpoint::presets};
use p2pmux::{
    protocol::{CreatePane, DeletePane, LayoutRejectReason, LayoutRequest, PaneReady, SplitAxis},
    session::{HostSession, LayoutControlEvent, SharedLayoutHost, join_layout},
    transport::{ALPN, Transport},
};
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

async fn loopback_transport() -> Transport {
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("localhost address")
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .expect("loopback endpoint");
    Transport::from_endpoint(endpoint)
}

async fn next_event(member: &mut p2pmux::session::SharedLayoutMember) -> LayoutControlEvent {
    timeout(TEST_TIMEOUT, member.events.recv())
        .await
        .expect("control event should arrive")
        .expect("control stream should remain open")
}

fn create_request(request_id: u64, base_revision: u64) -> LayoutRequest {
    LayoutRequest {
        request_id,
        base_revision,
        create_pane: Some(CreatePane {
            target_pane_id: 1,
            axis: Some(SplitAxis::LeftRight as i32),
            grid_rows: 24,
            grid_cols: 80,
        }),
        delete_pane: None,
        create_tab: None,
        delete_tab: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_member_receives_a_snapshot_and_existing_members_receive_admission_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");

    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first joins");
    accept_first
        .await
        .expect("accept task")
        .expect("accept first");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Snapshot(snapshot) if snapshot.state.as_ref().is_some_and(|state| state.revision == 2))
    );

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second joins");
    accept_second
        .await
        .expect("accept task")
        .expect("accept second");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 3)
    );
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Snapshot(snapshot) if snapshot.state.as_ref().is_some_and(|state| state.revision == 3))
    );

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reservation_is_targeted_and_ready_broadcasts_the_commit() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second");
    accept_second.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    let _ = next_event(&mut second).await;

    second
        .try_request(create_request(7, 3))
        .expect("queue request");
    let reservation = match next_event(&mut second).await {
        LayoutControlEvent::Reservation(reservation) => reservation,
        event => panic!("expected reservation, got {event:?}"),
    };
    assert!(
        timeout(Duration::from_millis(150), first.events.recv())
            .await
            .is_err(),
        "reservation must be targeted"
    );
    second
        .try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            base_revision: 3,
            request_id: 7,
        })
        .expect("queue ready");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 4)
    );
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Commit(commit) if commit.revision == 4)
    );

    second
        .try_request(LayoutRequest {
            request_id: 8,
            base_revision: 4,
            create_pane: None,
            delete_pane: Some(DeletePane {
                pane_id: reservation.pane_id,
            }),
            create_tab: None,
            delete_tab: None,
        })
        .expect("queue deletion");
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 5)
    );
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Commit(commit) if commit.revision == 5)
    );

    first
        .try_request(LayoutRequest {
            request_id: 9,
            base_revision: 5,
            create_pane: None,
            delete_pane: Some(DeletePane { pane_id: 1 }),
            create_tab: None,
            delete_tab: None,
        })
        .expect("queue foreign deletion");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Reject(reject)
            if reject.request_id == 9 && reject.reason == LayoutRejectReason::NotHost as i32
    ));

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_requests_are_rejected_only_for_the_requester() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut member = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("member");
    accept.await.unwrap().unwrap();
    let _ = next_event(&mut member).await;

    member
        .try_request(create_request(8, 1))
        .expect("queue request");
    assert!(
        matches!(next_event(&mut member).await, LayoutControlEvent::Reject(reject) if reject.request_id == 8 && reject.reason == LayoutRejectReason::Stale as i32)
    );
    member.shutdown().await;
    coordinator.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_invalidates_a_member_reservation_and_notifies_its_creator() {
    let host = HostSession::from_transport(loopback_transport().await).expect("host");
    let coordinator = SharedLayoutHost::new(host, 24, 80).expect("shared host");
    let accept_first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut first = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("first");
    accept_first.await.unwrap().unwrap();
    let _ = next_event(&mut first).await;
    first
        .try_request(create_request(10, 2))
        .expect("queue reservation request");
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Reservation(_)
    ));

    let accept_second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.accept_one_member().await })
    };
    let mut second = join_layout(loopback_transport().await, coordinator.ticket().clone())
        .await
        .expect("second");
    accept_second.await.unwrap().unwrap();
    assert!(
        matches!(next_event(&mut first).await, LayoutControlEvent::Commit(commit) if commit.revision == 3)
    );
    assert!(matches!(
        next_event(&mut first).await,
        LayoutControlEvent::Reject(reject)
            if reject.request_id == 10 && reject.reason == LayoutRejectReason::Stale as i32
    ));
    assert!(
        matches!(next_event(&mut second).await, LayoutControlEvent::Snapshot(snapshot) if snapshot.state.as_ref().is_some_and(|state| state.revision == 3))
    );

    first.shutdown().await;
    second.shutdown().await;
    coordinator.close().await;
}
