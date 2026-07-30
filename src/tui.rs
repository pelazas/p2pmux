//! The fixed-grid local terminal renderer and input loop.

mod clock;
mod debug_log;
mod geometry;
mod input;
mod multi_pane;
mod pane;
mod render;
mod runtime;
mod selection;
mod share;
mod snapshot;
mod state;
#[cfg(test)]
mod test_support;
mod text;

use std::{
    error::Error,
    fs::OpenOptions,
    io,
    io::Write,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, Event, KeyEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode,
        enable_raw_mode,
    },
};
use portable_pty::PtySize;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};

use crate::{
    kitty_keyboard::KittyKeyboardTracker,
    lease::{IDLE_AFTER, LeaseDecision, LeaseManager, LeaseState},
    pty_host::PtyHost,
    screen::{GuestScreen, HostScreen, ScreenFrame, SyncGate},
    session::{GuestEvent, GuestPane, HostControlEvent},
};

pub(crate) use debug_log::ui_debug_log;
pub(crate) use geometry::initial_root_pane_grid;
pub use input::mouse::PaneMouseProtocol;
use input::{
    events::{
        MAX_EVENTS_PER_CYCLE, begin_synchronized_output, collect_pending_events,
        end_synchronized_output, event_poll_timeout, frame_due,
    },
    keys::{encode_key, encode_paste, is_quit},
};
pub use multi_pane::MultiPaneTui;
pub use pane::local::SharedLocalPane;
use pane::remote::{
    RemoteInput, lease_allows_held_input, reconcile_remote_control_attempt, remote_input_decision,
};
pub use render::panes::{render_multi_pane, render_multi_pane_with_copy_feedback};
use render::vt::{VtScreen, render_guest_screen, render_host_screen};
pub use runtime::SharedLayoutRuntime;
pub(crate) use selection::copy_selection_to_clipboard;
pub(crate) use share::{resolve_local_ticket, share_copy_result};
pub(crate) use snapshot::{
    LocalScrollbackWindow, NodeLeaseSnapshots, NodeScreenSnapshot, NodeScreenSnapshots,
};
pub use state::{
    AgentOverlayRow, ChordMode, KeyHandling, MouseHandling, PaneGeometry, PaneViewState, ShareCopy,
    ShareView, UiIntent,
};
pub(in crate::tui) use state::{
    ModalState, PaneTextSelection, RenamePrompt, RenameTarget, ScreenCell,
};

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

/// How long a first Ctrl+A waits for a second one before the overlay commits.
pub(crate) const AGENT_TOGGLE_WINDOW: Duration = Duration::from_millis(200);
/// How often the working glyph in the agents overlay advances.
pub(crate) const AGENT_OVERLAY_ANIMATION_INTERVAL: Duration = Duration::from_millis(100);

/// The legacy fixed-grid host/guest footer, which has no chords, agents, or share modal.
const CONTROL_HELP: &str = "Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS";

pub struct HostPaneRuntime {
    host: PtyHost,
    screen: HostScreen,
    lease: LeaseManager,
    host_peer_id: Vec<u8>,
    screen_tx: watch::Sender<ScreenFrame>,
    lease_tx: watch::Sender<LeaseState>,
    control_rx: mpsc::Receiver<HostControlEvent>,
    join_code: String,
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

struct TerminalGuard {
    raw_mode: bool,
    keyboard_enhancement: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self {
            raw_mode: false,
            keyboard_enhancement: false,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_capture: false,
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhancement {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
            if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
                let _ = execute!(tty, PopKeyboardEnhancementFlags);
                let _ = tty.flush();
            }
        }
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.mouse_capture {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        if self.alternate_screen {
            let _ = execute!(stdout, crossterm::cursor::Show, LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

fn enable_keyboard_enhancement() -> io::Result<bool> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    Ok(true)
}

/// Run one local shell in a PTY whose dimensions never change after startup.
pub fn run_local() -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut host = PtyHost::spawn_default_shell(size)?;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let mut kitty_keyboard = KittyKeyboardTracker::default();

    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    execute!(io::stdout(), SetTitle("p2pmux"))?;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;

    let backend = CrosstermBackend::new(io::stdout());
    let fixed_area = Rect::new(0, 0, cols, rows);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(fixed_area),
        },
    )?;
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    let mut sync_gate = SyncGate::default();

    loop {
        let drain_started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        let ready = if pending.is_empty() {
            sync_gate.flush_stale(Instant::now())
        } else {
            sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            kitty_keyboard.observe(&ready);
            parser.process(&ready);
            if let Some(reply) = kitty_keyboard.take_query_reply() {
                host.write_input(&reply)?;
            }
            dirty = true;
        }
        if host.output_closed() {
            break;
        }

        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                let screen = parser.screen();
                let area = frame.area();
                frame.render_widget(VtScreen::new(screen), area);
                let (row, col) = screen.cursor_position();
                if !screen.hide_cursor() && row < area.height && col < area.width {
                    frame.set_cursor_position((area.x + col, area.y + row));
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }

        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if is_quit(key) {
                        quit = true;
                        break;
                    }
                    if let Some(bytes) = encode_key(key, parser.screen(), kitty_keyboard.active()) {
                        host.write_input(&bytes)?;
                    }
                }
                Event::Paste(text) => {
                    let bytes = encode_paste(&text, parser.screen().bracketed_paste());
                    host.write_input(&bytes)?;
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }

    Ok(())
}

/// Run the one fixed-grid host PTY and keep all peer work outside its drain loop.
pub fn run_host(mut runtime: HostPaneRuntime) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    let footer = format!("{CONTROL_HELP} | join: p2pmux join {}", runtime.join_code);
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    let mut sync_gate = SyncGate::default();
    loop {
        if let Some(state) = runtime.lease.clear_if_idle(Instant::now())? {
            runtime.lease_tx.send_replace(state);
        }
        while let Ok(event) = runtime.control_rx.try_recv() {
            match event {
                HostControlEvent::Input { peer_id, input } => match runtime.lease.input(
                    &peer_id,
                    input.lease_epoch,
                    input.data,
                    Instant::now(),
                ) {
                    LeaseDecision::AcceptInput(bytes) => {
                        runtime.host.write_input(&bytes)?;
                        runtime.lease_tx.send_replace(runtime.lease.state().clone());
                    }
                    LeaseDecision::Publish(_)
                    | LeaseDecision::RejectStaleInput
                    | LeaseDecision::RejectStaleRequest
                    | LeaseDecision::RejectActiveController => {}
                },
                HostControlEvent::TakeControl { peer_id, request } => {
                    let decision = runtime.lease.take_control(
                        peer_id,
                        request.known_lease_epoch,
                        Instant::now(),
                    )?;
                    match decision {
                        LeaseDecision::Publish(state) => {
                            runtime.lease_tx.send_replace(state);
                        }
                        LeaseDecision::RejectActiveController => {
                            runtime.lease_tx.send_replace(runtime.lease.state().clone());
                        }
                        LeaseDecision::AcceptInput(_)
                        | LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest => {}
                    }
                }
                HostControlEvent::ReleaseControl { peer_id } => {
                    if runtime.lease.state().controller_peer_id == peer_id
                        && let Some(state) = runtime.lease.clear_controller(Instant::now())?
                    {
                        runtime.lease_tx.send_replace(state);
                    }
                }
            }
        }
        let drain_started = Instant::now();
        let mut pending = Vec::new();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = runtime.host.try_read_output()? else {
                break;
            };
            pending.extend_from_slice(&bytes);
        }
        let ready = if pending.is_empty() {
            sync_gate.flush_stale(Instant::now())
        } else {
            sync_gate.feed(&pending, Instant::now())
        };
        if !ready.is_empty() {
            if let Ok(frame) = runtime.screen.process_pty(&ready) {
                if let Some(reply) = runtime.screen.take_kitty_keyboard_query_reply() {
                    runtime.host.write_input(&reply)?;
                }
                runtime.screen_tx.send_replace(frame);
            }
            dirty = true;
        }
        if runtime.host.output_closed() {
            break;
        }
        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                let screen = runtime.screen.screen();
                render_host_screen(frame, screen, &footer);
                let (row, col) = screen.cursor_position();
                let screen_height = screen.size().0.min(frame.area().height.saturating_sub(1));
                if !screen.hide_cursor() && row < screen_height && col < frame.area().width {
                    frame.set_cursor_position((frame.area().x + col, frame.area().y + row));
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }
        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if is_quit(key) {
                        quit = true;
                        break;
                    }
                    if let Some(bytes) = encode_key(
                        key,
                        runtime.screen.screen(),
                        runtime.screen.kitty_keyboard_active(),
                    ) {
                        let now = Instant::now();
                        let epoch = runtime.lease.state().epoch;
                        let decision =
                            runtime
                                .lease
                                .input(&runtime.host_peer_id, epoch, bytes, now);
                        match decision {
                            LeaseDecision::AcceptInput(bytes) => {
                                runtime.host.write_input(&bytes)?;
                                runtime.lease_tx.send_replace(runtime.lease.state().clone());
                            }
                            LeaseDecision::Publish(_) => {}
                            LeaseDecision::RejectStaleInput
                            | LeaseDecision::RejectStaleRequest
                            | LeaseDecision::RejectActiveController => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    let bytes = encode_paste(&text, runtime.screen.screen().bracketed_paste());
                    let now = Instant::now();
                    let epoch = runtime.lease.state().epoch;
                    let decision = runtime
                        .lease
                        .input(&runtime.host_peer_id, epoch, bytes, now);
                    match decision {
                        LeaseDecision::AcceptInput(bytes) => {
                            runtime.host.write_input(&bytes)?;
                            runtime.lease_tx.send_replace(runtime.lease.state().clone());
                        }
                        LeaseDecision::Publish(_) => {}
                        LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest
                        | LeaseDecision::RejectActiveController => {}
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }
    Ok(())
}

/// Render one remote, immutable terminal grid. Input forwarding arrives in milestone 12.
pub fn run_guest(mut pane: GuestPane) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = terminal::size()?;
    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    guard.keyboard_enhancement = enable_keyboard_enhancement()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    let mut remote = GuestScreen::new();
    let mut footer = String::from("controller: waiting spectator");
    let mut lease = None;
    let mut last_lease = Instant::now();
    let mut pending_control = false;
    let mut held_input = Vec::new();
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;

    loop {
        let mut received_lease = false;
        loop {
            match pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    if remote
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok()
                    {
                        remote.set_kitty_keyboard_active(snapshot.kitty_keyboard_active);
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    if remote
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok()
                    {
                        remote.set_kitty_keyboard_active(delta.kitty_keyboard_active);
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenGap { .. }) => {}
                Ok(GuestEvent::Lease(state)) => {
                    received_lease = true;
                    footer = format!(
                        "controller: {} typing",
                        short_peer(&state.controller_peer_id)
                    );
                    last_lease = Instant::now();
                    pending_control = false;
                    lease = Some(state);
                    dirty = true;
                }
                Ok(GuestEvent::Disconnected)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "host session disconnected",
                    )
                    .into());
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            }
        }

        if received_lease && let Some(state) = lease.as_ref() {
            reconcile_remote_control_attempt(
                &mut pending_control,
                &mut held_input,
                &state.controller_peer_id,
                pane.controls.peer_id(),
            );
        }

        if !pending_control
            && !held_input.is_empty()
            && let Some(state) = lease.as_ref()
            && lease_allows_held_input(&state.controller_peer_id, pane.controls.peer_id())
        {
            let bytes = std::mem::take(&mut held_input);
            if pane
                .controls
                .try_input(state.lease_epoch, bytes.clone())
                .is_err()
            {
                held_input = bytes;
            }
        }

        if dirty && frame_due(last_draw) {
            begin_synchronized_output()?;
            terminal.draw(|frame| {
                if let Some(screen) = remote.screen() {
                    render_guest_screen(frame, screen, &footer);
                }
            })?;
            end_synchronized_output()?;
            dirty = false;
            last_draw = Some(Instant::now());
        }

        if !event::poll(event_poll_timeout(dirty, last_draw))? {
            continue;
        }
        let mut quit = false;
        for event in collect_pending_events(MAX_EVENTS_PER_CYCLE)? {
            match event {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && is_quit(key) =>
                {
                    quit = true;
                    break;
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen())
                        && let Some(bytes) = encode_key(key, screen, remote.kitty_keyboard_active())
                    {
                        let claiming_free_pane = state.controller_peer_id.is_empty();
                        match remote_input_decision(
                            &state.controller_peer_id,
                            pane.controls.peer_id(),
                            pending_control,
                            held_input.is_empty(),
                            last_lease.elapsed() >= IDLE_AFTER,
                        ) {
                            RemoteInput::Send => {
                                if pane.controls.try_input(state.lease_epoch, bytes).is_ok()
                                    && claiming_free_pane
                                {
                                    pending_control = true;
                                }
                            }
                            RemoteInput::Hold => held_input.extend_from_slice(&bytes),
                            RemoteInput::Request => {
                                held_input.extend_from_slice(&bytes);
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                            RemoteInput::Ignore => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen()) {
                        let bytes = encode_paste(&text, screen.bracketed_paste());
                        let claiming_free_pane = state.controller_peer_id.is_empty();
                        match remote_input_decision(
                            &state.controller_peer_id,
                            pane.controls.peer_id(),
                            pending_control,
                            held_input.is_empty(),
                            last_lease.elapsed() >= IDLE_AFTER,
                        ) {
                            RemoteInput::Send => {
                                if pane.controls.try_input(state.lease_epoch, bytes).is_ok()
                                    && claiming_free_pane
                                {
                                    pending_control = true;
                                }
                            }
                            RemoteInput::Hold => held_input.extend_from_slice(&bytes),
                            RemoteInput::Request => {
                                held_input.extend_from_slice(&bytes);
                                pending_control = true;
                                if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                    pending_control = false;
                                    held_input.clear();
                                }
                            }
                            RemoteInput::Ignore => {}
                        }
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if quit {
            break;
        }
    }
    Ok(())
}

fn short_peer(peer_id: &[u8]) -> String {
    peer_id
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn member_label(peer_id: &[u8], members: &[crate::layout::Member]) -> String {
    let Some(member) = members.iter().find(|member| member.peer_id == peer_id) else {
        return short_peer(peer_id);
    };
    if member.display_name.is_empty() {
        return short_peer(peer_id);
    }
    let duplicates = members
        .iter()
        .filter(|candidate| candidate.display_name == member.display_name)
        .count();
    if duplicates > 1 {
        format!("{} · {}", member.display_name, short_peer(peer_id))
    } else {
        member.display_name.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;
    use tokio::sync::{mpsc, watch};

    use portable_pty::PtySize;

    use crate::lease::LeaseState;
    use crate::screen::HostScreen;

    use super::{HostPaneRuntime, member_label};

    #[test]
    fn new_host_runtime_starts_free_while_the_host_retains_pty_ownership() {
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

    #[test]
    fn member_labels_disambiguate_duplicate_display_names() {
        let members = vec![
            crate::layout::Member {
                peer_id: vec![0xaa, 0xbb, 0xcc, 0xdd],
                endpoint_addr: vec![1],
                display_name: "sam".into(),
            },
            crate::layout::Member {
                peer_id: vec![0x11, 0x22, 0x33, 0x44],
                endpoint_addr: vec![2],
                display_name: "sam".into(),
            },
            crate::layout::Member {
                peer_id: vec![0x55, 0x66, 0x77, 0x88],
                endpoint_addr: vec![3],
                display_name: "pat".into(),
            },
        ];

        assert_eq!(
            member_label(&members[0].peer_id, &members),
            "sam · aabbccdd"
        );
        assert_eq!(member_label(&members[2].peer_id, &members), "pat");
    }
}
