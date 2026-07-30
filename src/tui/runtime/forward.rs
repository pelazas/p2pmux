//! Sending local input to whichever pane holds focus, local or remote.

use std::error::Error;

use crossterm::event::KeyEvent;

use crate::tui::{
    PaneMouseProtocol,
    input::keys::{encode_key, encode_paste},
};

use super::SharedLayoutRuntime;

impl SharedLayoutRuntime {
    /// The mouse reporting the focused pane's child has turned on, if any.
    pub(in crate::tui) fn focused_pane_mouse_protocol(&self) -> PaneMouseProtocol {
        let pane_id = self.tui.focused_pane();
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
