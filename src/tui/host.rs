//! The legacy single-pane host runtime, still used by `p2pmux host`.

use std::{error::Error, time::Instant};

use portable_pty::PtySize;
use tokio::sync::{mpsc, watch};

use crate::{
    lease::{LeaseManager, LeaseState},
    pty_host::PtyHost,
    screen::{HostScreen, ScreenFrame},
    session::HostControlEvent,
};

pub struct HostPaneRuntime {
    pub(in crate::tui) host: PtyHost,
    pub(in crate::tui) screen: HostScreen,
    pub(in crate::tui) lease: LeaseManager,
    pub(in crate::tui) host_peer_id: Vec<u8>,
    pub(in crate::tui) screen_tx: watch::Sender<ScreenFrame>,
    pub(in crate::tui) lease_tx: watch::Sender<LeaseState>,
    pub(in crate::tui) control_rx: mpsc::Receiver<HostControlEvent>,
    pub(in crate::tui) join_code: String,
}
impl HostPaneRuntime {
    pub fn new(
        size: PtySize,
        host_peer_id: Vec<u8>,
        screen_tx: watch::Sender<ScreenFrame>,
        lease_tx: watch::Sender<LeaseState>,
        control_rx: mpsc::Receiver<HostControlEvent>,
        join_code: String,
    ) -> Result<Self, Box<dyn Error>> {
        let screen = HostScreen::new(size.rows, size.cols)?;
        let lease = LeaseManager::new(Vec::new(), Instant::now());
        lease_tx.send_replace(lease.state().clone());
        Ok(Self {
            host: PtyHost::spawn_default_shell(size)?,
            screen,
            lease,
            host_peer_id,
            screen_tx,
            lease_tx,
            control_rx,
            join_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use portable_pty::PtySize;
    use tokio::sync::{mpsc, watch};

    use crate::{lease::LeaseState, screen::HostScreen};

    use super::HostPaneRuntime;

    #[test]
    pub(in crate::tui) fn new_host_runtime_starts_free_while_the_host_retains_pty_ownership() {
        let host_id = b"host".to_vec();
        let screen = HostScreen::new(1, 1).expect("screen");
        let (screen_tx, _) = watch::channel(screen.current_frame().clone());
        let (lease_tx, lease_rx) = watch::channel(LeaseState {
            controller_peer_id: host_id.clone(),
            epoch: 1,
            last_activity: Instant::now(),
        });
        let (_control_tx, control_rx) = mpsc::channel(8);
        let mut runtime = HostPaneRuntime::new(
            PtySize {
                rows: 1,
                cols: 1,
                pixel_width: 0,
                pixel_height: 0,
            },
            host_id.clone(),
            screen_tx,
            lease_tx,
            control_rx,
            String::from("TESTCODE"),
        )
        .expect("host runtime");

        assert!(runtime.lease.state().controller_peer_id.is_empty());
        assert_eq!(runtime.host_peer_id, host_id);
        assert!(lease_rx.borrow().controller_peer_id.is_empty());

        runtime.host.shutdown().expect("shutdown host runtime");
    }
}
