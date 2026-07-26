//! Headless owner of a shared layout.
//!
//! `SharedLayoutRuntime` remains the temporary foreground adapter during the split.  The node
//! owns it without terminal I/O and exposes only node operations; the local socket client is the
//! only component allowed to render a terminal.

use std::{error::Error, time::Instant};

use crate::tui::SharedLayoutRuntime;

pub struct SharedLayoutNode {
    runtime: SharedLayoutRuntime,
    last_tab_id: u64,
    last_pane_id: u64,
}

impl SharedLayoutNode {
    pub fn new(runtime: SharedLayoutRuntime) -> Self {
        let (last_tab_id, last_pane_id) = runtime.local_focus();
        Self { runtime, last_tab_id, last_pane_id }
    }

    /// Advances Iroh, pane servers, PTYs, leases, subscriptions and agent sampling without ever
    /// touching terminal state.
    pub fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let changed = self.runtime.drain_node()?;
        (self.last_tab_id, self.last_pane_id) = self.runtime.local_focus();
        Ok(changed)
    }

    pub fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> { self.runtime.node_input(bytes) }
    pub fn release_all_local_control(&mut self) -> Result<(), Box<dyn Error>> { self.runtime.release_all_local_control() }
    pub fn local_focus(&self) -> (u64, u64) { (self.last_tab_id, self.last_pane_id) }
    pub fn screen_text(&self) -> String { self.runtime.node_screen_text() }
    pub fn tick_due(&self) -> Instant { Instant::now() }
    pub fn shutdown(self) { self.runtime.shutdown_node(); }
}
