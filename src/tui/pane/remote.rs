//! A pane hosted by another peer: the guest screen, the lease it is waiting
//! on, and what happens to input while control is pending.

use std::time::Instant;

use crate::{
    lease::{IDLE_AFTER, LeaseState},
    screen::GuestScreen,
    session::{GuestEvent, GuestPane},
    tui::PaneViewState,
};

pub(in crate::tui) struct SharedRemotePane {
    pub(in crate::tui) pane: GuestPane,
    pub(in crate::tui) screen: GuestScreen,
    pub(in crate::tui) lease: Option<LeaseState>,
    pub(in crate::tui) last_lease: Instant,
    pub(in crate::tui) pending_control: bool,
    pub(in crate::tui) held_input: Vec<u8>,
    pub(in crate::tui) exited: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum RemotePaneDrain {
    Unchanged,
    Changed,
    Disconnected,
}
pub(in crate::tui) fn lease_allows_held_input(controller_peer_id: &[u8], peer_id: &[u8]) -> bool {
    controller_peer_id.is_empty() || controller_peer_id == peer_id
}
pub(in crate::tui) fn reconcile_remote_control_attempt(
    pending_control: &mut bool,
    held_input: &mut Vec<u8>,
    controller_peer_id: &[u8],
    peer_id: &[u8],
) {
    *pending_control = false;
    if !lease_allows_held_input(controller_peer_id, peer_id) {
        held_input.clear();
    }
}
/// What a guest should do with one keystroke aimed at a pane hosted by someone else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum RemoteInput {
    /// Send it now, stamped with the lease epoch this guest currently knows.
    Send,
    /// Append it to `held_input`: a lease change of ours is already in flight, and
    /// anything sent before the answer arrives would be stamped with a dead epoch.
    Hold,
    /// Ask the host for the pane, then hold this keystroke until it answers.
    Request,
    /// Somebody else is actively typing here. Their pane is protected.
    Ignore,
}
/// The guest-side input rule, in one place because two loops need it and they had
/// drifted apart.
///
/// The subtle case is `controller_peer_id` being empty -- a free pane, claimed by
/// typing into it. That claim is not free: the host bumps the lease epoch the moment it
/// accepts the first byte, so every keystroke typed during the round trip that follows
/// arrives stamped with an epoch the host has already left behind and is rejected as
/// stale. On loopback the window is under a millisecond and no scenario ever caught it;
/// across an 85ms internet path it silently swallows the next five characters, so
/// `echo N-CROSS` reaches the shell as `e-CROSS`. Holding the rest of the burst until
/// the new lease lands costs nothing and keeps the line intact.
pub(in crate::tui) fn remote_input_decision(
    controller_peer_id: &[u8],
    peer_id: &[u8],
    pending_control: bool,
    held_input_empty: bool,
    controller_idle: bool,
) -> RemoteInput {
    if controller_peer_id == peer_id {
        return if held_input_empty {
            RemoteInput::Send
        } else {
            RemoteInput::Hold
        };
    }
    if controller_peer_id.is_empty() {
        return if pending_control {
            RemoteInput::Hold
        } else {
            RemoteInput::Send
        };
    }
    if controller_idle {
        return if pending_control {
            RemoteInput::Hold
        } else {
            RemoteInput::Request
        };
    }
    RemoteInput::Ignore
}
impl SharedRemotePane {
    pub(in crate::tui) fn new(pane: GuestPane) -> Self {
        Self {
            pane,
            screen: GuestScreen::new(),
            lease: None,
            last_lease: Instant::now(),
            pending_control: false,
            held_input: Vec::new(),
            exited: false,
        }
    }

    pub(in crate::tui) fn view_state(&self) -> PaneViewState {
        PaneViewState {
            ready: self.screen.screen().is_some() && self.lease.is_some(),
            controller_peer_id: self
                .lease
                .as_ref()
                .map(|lease| lease.controller_peer_id.clone()),
            controller_active: self
                .lease
                .as_ref()
                .is_some_and(|lease| !lease.is_idle_at(Instant::now())),
            scrollback: 0,
            origin: Default::default(),
        }
    }

    pub(in crate::tui) fn drain(&mut self) -> RemotePaneDrain {
        if self.exited {
            while self.pane.events.try_recv().is_ok() {}
            return RemotePaneDrain::Unchanged;
        }
        let mut changed = false;
        let mut received_lease = false;
        loop {
            match self.pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    if self
                        .screen
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok()
                    {
                        self.screen
                            .set_kitty_keyboard_active(snapshot.kitty_keyboard_active);
                        changed = true;
                    }
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    if self
                        .screen
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok()
                    {
                        self.screen
                            .set_kitty_keyboard_active(delta.kitty_keyboard_active);
                        changed = true;
                    }
                }
                Ok(GuestEvent::Lease(lease)) => {
                    received_lease = true;
                    self.lease = Some(LeaseState {
                        controller_peer_id: lease.controller_peer_id,
                        epoch: lease.lease_epoch,
                        last_activity: Instant::now(),
                    });
                    self.last_lease = Instant::now();
                    self.pending_control = false;
                    changed = true;
                }
                Ok(GuestEvent::ScreenGap { .. }) => {}
                Ok(GuestEvent::Disconnected)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return RemotePaneDrain::Disconnected;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            }
        }
        if received_lease && let Some(lease) = self.lease.as_ref() {
            reconcile_remote_control_attempt(
                &mut self.pending_control,
                &mut self.held_input,
                &lease.controller_peer_id,
                self.pane.controls.peer_id(),
            );
        }
        if !self.pending_control
            && !self.held_input.is_empty()
            && self.lease.as_ref().is_some_and(|lease| {
                lease_allows_held_input(&lease.controller_peer_id, self.pane.controls.peer_id())
            })
        {
            let bytes = std::mem::take(&mut self.held_input);
            if self
                .pane
                .controls
                .try_input(
                    self.lease.as_ref().expect("checked lease").epoch,
                    bytes.clone(),
                )
                .is_err()
            {
                self.held_input = bytes;
            }
        }
        if changed {
            RemotePaneDrain::Changed
        } else {
            RemotePaneDrain::Unchanged
        }
    }

    pub(in crate::tui) fn input(&mut self, bytes: Vec<u8>) {
        if self.exited {
            return;
        }
        let Some(lease) = self.lease.as_ref() else {
            return;
        };
        let claiming_free_pane = lease.controller_peer_id.is_empty();
        match remote_input_decision(
            &lease.controller_peer_id,
            self.pane.controls.peer_id(),
            self.pending_control,
            self.held_input.is_empty(),
            self.last_lease.elapsed() >= IDLE_AFTER,
        ) {
            RemoteInput::Send => {
                if self.pane.controls.try_input(lease.epoch, bytes).is_ok() && claiming_free_pane {
                    // The host will answer this byte with a new epoch; hold the rest of
                    // the burst until it does.
                    self.pending_control = true;
                }
            }
            RemoteInput::Hold => self.held_input.extend_from_slice(&bytes),
            RemoteInput::Request => {
                self.held_input.extend_from_slice(&bytes);
                self.pending_control = self.pane.controls.try_take_control(lease.epoch).is_ok();
                if !self.pending_control {
                    self.held_input.clear();
                }
            }
            RemoteInput::Ignore => {}
        }
    }

    pub(in crate::tui) fn release_controller(&mut self) -> bool {
        let Some(lease) = self.lease.as_mut() else {
            return false;
        };
        if lease.controller_peer_id != self.pane.controls.peer_id()
            || self.pane.controls.try_release_control().is_err()
        {
            return false;
        }
        lease.controller_peer_id.clear();
        self.pending_control = false;
        self.held_input.clear();
        true
    }

    pub(in crate::tui) fn mark_exited(&mut self) {
        self.exited = true;
        self.pending_control = false;
        self.held_input.clear();
    }
}

#[cfg(test)]
mod tests {

    use super::{
        RemoteInput, lease_allows_held_input, reconcile_remote_control_attempt,
        remote_input_decision,
    };

    #[test]
    fn remote_held_input_follows_the_final_lease_owner() {
        let peer = b"requester";
        let mut pending_control = true;
        let mut held_input = b"first".to_vec();

        assert!(lease_allows_held_input(b"", peer));
        reconcile_remote_control_attempt(&mut pending_control, &mut held_input, b"", peer);
        assert!(!pending_control, "free leases release the retry gate");
        assert_eq!(held_input, b"first", "free leases retain queued input");

        reconcile_remote_control_attempt(&mut pending_control, &mut held_input, b"other", peer);
        assert!(
            held_input.is_empty(),
            "a later controller wins and discards stale input"
        );
    }

    #[test]
    fn claiming_a_free_remote_pane_holds_the_rest_of_the_burst() {
        let peer = b"typist";

        // The first keystroke goes out and claims the pane.
        assert_eq!(
            remote_input_decision(b"", peer, false, true, false),
            RemoteInput::Send
        );
        // Everything typed during the round trip has to wait: the host has already
        // bumped the epoch, so sending now means the host drops those bytes as stale.
        // This is the whole bug -- invisible on loopback, five lost characters at 85ms.
        assert_eq!(
            remote_input_decision(b"", peer, true, true, false),
            RemoteInput::Hold
        );
    }

    #[test]
    fn remote_input_respects_the_controller_lease() {
        let peer = b"typist";

        assert_eq!(
            remote_input_decision(peer, peer, false, true, false),
            RemoteInput::Send,
            "the controller types straight through"
        );
        assert_eq!(
            remote_input_decision(peer, peer, false, false, false),
            RemoteInput::Hold,
            "queued input keeps its place in line"
        );
        assert_eq!(
            remote_input_decision(b"other", peer, false, true, false),
            RemoteInput::Ignore,
            "an active controller is protected from takeover"
        );
        assert_eq!(
            remote_input_decision(b"other", peer, false, true, true),
            RemoteInput::Request,
            "an idle controller can be displaced"
        );
        assert_eq!(
            remote_input_decision(b"other", peer, true, true, true),
            RemoteInput::Hold,
            "one request at a time; the rest of the burst waits for the answer"
        );
    }
}
