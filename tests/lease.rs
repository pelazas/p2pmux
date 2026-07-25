use std::time::{Duration, Instant};

use p2pmux::lease::{IDLE_AFTER, LeaseDecision, LeaseError, LeaseManager};

#[test]
fn controller_input_refreshes_activity_and_stale_input_is_rejected() {
    let now = Instant::now();
    let mut lease = LeaseManager::new(vec![1], now);
    assert_eq!(lease.state().epoch, 1);
    assert_eq!(
        lease.input(&[1], 1, b"ok".to_vec(), now + Duration::from_secs(1)),
        LeaseDecision::AcceptInput(b"ok".to_vec())
    );
    let activity = lease.state().last_activity;
    assert_eq!(
        lease.input(&[2], 1, b"no".to_vec(), now + Duration::from_secs(2)),
        LeaseDecision::RejectStaleInput
    );
    assert_eq!(
        lease.input(&[1], 2, b"no".to_vec(), now + Duration::from_secs(3)),
        LeaseDecision::RejectStaleInput
    );
    assert_eq!(lease.state().last_activity, activity);
}

#[test]
fn free_pane_input_claims_control_and_delivers_the_first_input() {
    let now = Instant::now();
    let mut lease = LeaseManager::with_epoch_for_test(Vec::new(), 2, now);

    assert_eq!(
        lease.input(&[2], 2, b"first".to_vec(), now + Duration::from_secs(1)),
        LeaseDecision::AcceptInput(b"first".to_vec())
    );
    assert_eq!(lease.state().controller_peer_id, vec![2]);
    assert_eq!(lease.state().epoch, 3);
}

#[test]
fn input_from_a_non_controller_is_rejected_while_control_is_active() {
    let now = Instant::now();
    let mut lease = LeaseManager::new(vec![1], now);

    assert_eq!(
        lease.input(&[2], 1, b"no".to_vec(), now + Duration::from_secs(1)),
        LeaseDecision::RejectStaleInput
    );
    assert_eq!(lease.state().controller_peer_id, vec![1]);
    assert_eq!(lease.state().epoch, 1);
}

#[test]
fn take_control_advances_epoch_and_rejects_stale_requests() {
    let now = Instant::now();
    let mut lease = LeaseManager::new(vec![1], now);
    assert!(
        matches!(lease.take_control(vec![2], 1, now + IDLE_AFTER), Ok(LeaseDecision::Publish(state)) if state.controller_peer_id == vec![2] && state.epoch == 2)
    );
    assert_eq!(
        lease.take_control(vec![1], 1, now + IDLE_AFTER + Duration::from_secs(1)),
        Ok(LeaseDecision::RejectStaleRequest)
    );
    assert_eq!(
        lease.input(&[1], 1, b"late".to_vec(), now + IDLE_AFTER),
        LeaseDecision::RejectStaleInput
    );
}

#[test]
fn normal_take_control_requires_an_idle_controller() {
    let now = Instant::now();
    let mut lease = LeaseManager::new(vec![1], now);

    let decision = lease
        .take_control(vec![2], 1, now + Duration::from_secs(1))
        .expect("active controller does not exhaust the epoch");

    assert_eq!(decision, LeaseDecision::RejectActiveController);
    assert_eq!(lease.state().controller_peer_id, vec![1]);
    assert_eq!(lease.state().epoch, 1);
}

#[test]
fn force_clear_releases_an_active_controller_and_advances_the_epoch() {
    let now = Instant::now();
    let mut lease = LeaseManager::new(vec![1], now);

    let cleared = lease
        .clear_controller(now + Duration::from_secs(1))
        .expect("clearing an active controller");

    assert!(cleared.is_some());
    assert!(lease.state().controller_peer_id.is_empty());
    assert_eq!(lease.state().epoch, 2);
}

#[test]
fn epoch_exhaustion_is_an_error_not_a_wrap() {
    let now = Instant::now();
    let mut lease = LeaseManager::with_epoch_for_test(vec![1], u64::MAX, now);
    assert_eq!(
        lease.take_control(vec![2], u64::MAX, now),
        Err(LeaseError::EpochExhausted)
    );
}
