//! Wall-clock reads for timestamps the UI shows or ships to peers.

use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
