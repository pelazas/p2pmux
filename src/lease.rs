//! Pure epoch-checked controller lease state machine.

use std::time::{Duration, Instant};

pub const IDLE_AFTER: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseState {
    pub controller_peer_id: Vec<u8>,
    pub epoch: u64,
    pub last_activity: Instant,
}

impl LeaseState {
    pub fn is_idle_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_activity) >= IDLE_AFTER
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum LeaseDecision {
    AcceptInput(Vec<u8>),
    Publish(LeaseState),
    RejectStaleInput,
    RejectStaleRequest,
    RejectActiveController,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LeaseError {
    EpochExhausted,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("lease epoch exhausted")
    }
}

impl std::error::Error for LeaseError {}

pub struct LeaseManager {
    state: LeaseState,
}

impl LeaseManager {
    pub fn new(initial_controller: Vec<u8>, now: Instant) -> Self {
        Self {
            state: LeaseState {
                controller_peer_id: initial_controller,
                epoch: 1,
                last_activity: now,
            },
        }
    }

    pub fn state(&self) -> &LeaseState {
        &self.state
    }

    pub fn input(
        &mut self,
        sender: &[u8],
        epoch: u64,
        data: Vec<u8>,
        now: Instant,
    ) -> LeaseDecision {
        if data.is_empty() || sender != self.state.controller_peer_id || epoch != self.state.epoch {
            return LeaseDecision::RejectStaleInput;
        }
        self.state.last_activity = now;
        LeaseDecision::AcceptInput(data)
    }

    pub fn take_control(
        &mut self,
        sender: Vec<u8>,
        known_epoch: u64,
        now: Instant,
    ) -> Result<LeaseDecision, LeaseError> {
        if known_epoch != self.state.epoch {
            return Ok(LeaseDecision::RejectStaleRequest);
        }
        if self.state.epoch == u64::MAX {
            return Err(LeaseError::EpochExhausted);
        }
        if !self.state.is_idle_at(now) {
            return Ok(LeaseDecision::RejectActiveController);
        }
        self.change_controller(sender, now)
    }

    fn change_controller(
        &mut self,
        sender: Vec<u8>,
        now: Instant,
    ) -> Result<LeaseDecision, LeaseError> {
        let epoch = self
            .state
            .epoch
            .checked_add(1)
            .ok_or(LeaseError::EpochExhausted)?;
        self.state = LeaseState {
            controller_peer_id: sender,
            epoch,
            last_activity: now,
        };
        Ok(LeaseDecision::Publish(self.state.clone()))
    }

    #[doc(hidden)]
    pub fn with_epoch_for_test(initial_controller: Vec<u8>, epoch: u64, now: Instant) -> Self {
        Self {
            state: LeaseState {
                controller_peer_id: initial_controller,
                epoch,
                last_activity: now,
            },
        }
    }
}
