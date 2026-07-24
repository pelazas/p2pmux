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
fn epoch_exhaustion_is_an_error_not_a_wrap() {
    let now = Instant::now();
    let mut lease = LeaseManager::with_epoch_for_test(vec![1], u64::MAX, now);
    assert_eq!(
        lease.take_control(vec![2], u64::MAX, now),
        Err(LeaseError::EpochExhausted)
    );
}
