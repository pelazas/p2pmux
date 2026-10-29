//! Sending local input to whichever pane holds focus, local or remote.

use std::error::Error;

use crossterm::event::KeyEvent;

use crate::{
    layout::PaneId,
    tui::{
        PaneMouseProtocol,
        input::keys::{encode_key, encode_paste},
    },
};

use super::SharedLayoutRuntime;

impl SharedLayoutRuntime {
    /// The mouse reporting a pane's child has turned on, if any.
    pub(in crate::tui) fn pane_mouse_protocol(&self, pane_id: PaneId) -> PaneMouseProtocol {
        if !self.input_allowed(pane_id) {
            return PaneMouseProtocol::default();
        }
        self.local
            .get(&pane_id)
            .map(|pane| PaneMouseProtocol::from_screen(pane.screen.screen()))
            .or_else(|| {
                self.remote
                    .get(&pane_id)
                    .and_then(|pane| pane.screen.screen())
                    .map(PaneMouseProtocol::from_screen)
            })
            .unwrap_or_default()
    }

    /// The mouse reporting the focused pane's child has turned on, if any.
    pub(in crate::tui) fn focused_pane_mouse_protocol(&self) -> PaneMouseProtocol {
        self.pane_mouse_protocol(self.tui.focused_pane())
    }

    pub(in crate::tui) fn forward_mouse(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(bytes.clone())?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id) {
            pane.input(bytes);
        }
        Ok(())
    }

    pub(in crate::tui) fn forward_key(&mut self, key: KeyEvent) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        let mut sent = false;
        if let Some(pane) = self.local.get_mut(&pane_id)
            && let Some(bytes) = encode_key(
                key,
                pane.screen.screen(),
                pane.screen.kitty_keyboard_active(),
            )
        {
            pane.input(bytes)?;
            sent = true;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id)
            && let Some(screen) = pane.screen.screen()
            && let Some(bytes) = encode_key(key, screen, pane.screen.kitty_keyboard_active())
        {
            pane.input(bytes);
            sent = true;
        }
        if sent {
            self.tui.reset_scrollback(pane_id);
        }
        Ok(())
    }

    /// Focus in/out for a pane switch the user just made.
    ///
    /// Only a pane that turned on DECSET 1004 hears it, and only as the two
    /// bytes that mode asks for. A pane this node does not have on screen is
    /// skipped; the check is the screen, so a remote pane is told by sending
    /// the bytes on to the machine that owns it.
    pub(in crate::tui) fn deliver_focus_report(&mut self) -> Result<(), Box<dyn Error>> {
        let Some((left, entered)) = self.tui.take_focus_report() else {
            return Ok(());
        };
        self.send_focus_event(left, b"\x1b[O")?;
        self.send_focus_event(entered, b"\x1b[I")?;
        Ok(())
    }

    pub(in crate::tui) fn send_focus_event(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        let wants = self
            .pane_vt_screen(pane_id)
            .is_some_and(|screen| screen.focus_event_mode());
        if !wants {
            return Ok(());
        }
        self.forward_bytes(pane_id, bytes.to_vec())
    }

    pub(in crate::tui) fn pane_vt_screen(&self, pane_id: PaneId) -> Option<&vt100::Screen> {
        if let Some(pane) = self.local.get(&pane_id) {
            return Some(pane.screen.screen());
        }
        self.remote
            .get(&pane_id)
            .and_then(|pane| pane.screen.screen())
    }

    pub(in crate::tui) fn forward_bytes(
        &mut self,
        pane_id: PaneId,
        bytes: Vec<u8>,
    ) -> Result<(), Box<dyn Error>> {
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(bytes.clone())?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id) {
            pane.input(bytes);
        }
        Ok(())
    }

    pub(in crate::tui) fn forward_paste(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        if !self.input_allowed(pane_id) {
            return Ok(());
        }
        if let Some(pane) = self.local.get_mut(&pane_id) {
            pane.input(encode_paste(text, pane.screen.screen().bracketed_paste()))?;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id)
            && let Some(screen) = pane.screen.screen()
        {
            pane.input(encode_paste(text, screen.bracketed_paste()));
        }
        self.tui.reset_scrollback(pane_id);
        Ok(())
    }
}
