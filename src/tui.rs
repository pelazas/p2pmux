//! The terminal UI: the fixed-grid renderer, the input loop, and the runtimes
//! that drive them.
//!
//! The module is a facade. Everything lives in a submodule, grouped by what it
//! does, and this root only re-exports what the rest of the crate uses:
//!
//! - [`state`], [`snapshot`] — the value types the UI keeps and hands back
//! - [`input`], [`geometry`], [`text`], [`selection`] — key and mouse
//!   encoding, rect math, width-aware strings, clipboard
//! - [`render`] — every pixel: pane chrome, footer, overlays, modals, vt100
//! - [`multi_pane`] — `MultiPaneTui`, the local view of a shared layout
//! - [`pane`], [`runtime`] — the panes a session owns and the loop over them
//! - [`app`], [`host`], [`terminal`] — the blocking entry points and their
//!   terminal setup
//!
//! Items shared inside the tree are `pub(in crate::tui)`; only the re-exports
//! below are visible to the rest of the crate.

mod app;
mod clock;
mod debug_log;
mod geometry;
mod home;
mod host;
mod input;
mod multi_pane;
mod pane;
mod render;
mod runtime;
mod selection;
mod share;
mod snapshot;
mod state;
mod terminal;
#[cfg(test)]
mod test_support;
mod text;

use std::time::Duration;

use app::member_label;
pub use app::{run_guest, run_host, run_local};
pub(crate) use debug_log::ui_debug_log;
pub(crate) use geometry::{
    initial_root_pane_grid, missed_resize, resize_recheck_due, stale_node_size,
};
pub use home::MachineRow;
pub use host::HostPaneRuntime;
pub use input::mouse::PaneMouseProtocol;
pub use multi_pane::MultiPaneTui;
pub use pane::local::SharedLocalPane;
pub use render::home::machine_line;
pub use render::panes::{render_multi_pane, render_multi_pane_with_copy_feedback};
pub use runtime::{RolePersist, SharedLayoutRuntime};
pub(crate) use selection::copy_selection_to_clipboard;
pub(crate) use share::share_copy_result;
pub(crate) use snapshot::{
    LocalScrollbackWindow, NodeLeaseSnapshots, NodeScreenSnapshot, NodeScreenSnapshots,
};
pub use state::{
    AgentOverlayRow, ChordMode, KeyHandling, MouseHandling, PairedMachine, PaneGeometry,
    PaneViewState, ShareCopy, ShareView, UiIntent,
};
pub(in crate::tui) use state::{
    Invite, ModalState, PaneTextSelection, RenamePrompt, RenameTarget, ScreenCell,
};
pub(crate) use terminal::clear_before_first_frame;
pub(in crate::tui) use terminal::{TerminalGuard, enable_keyboard_enhancement};

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

/// How long a first Ctrl+A waits for a second one before Home commits.
///
/// Ctrl+A is the legacy binding, and screen and tmux users have a decade of
/// muscle memory that says a doubled Ctrl+A means "send a literal one". Within
/// this window the second press closes Home and forwards; after it, Home simply
/// stays open. Ctrl+O -- the binding this build teaches -- has no such history
/// and no such window.
pub(crate) const HOME_TOGGLE_WINDOW: Duration = Duration::from_millis(200);
/// How often the working glyph on an inbox row advances.
pub(crate) const AGENT_OVERLAY_ANIMATION_INTERVAL: Duration = Duration::from_millis(100);
