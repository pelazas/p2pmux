//! The fixed-grid local terminal renderer and input loop.

use std::{
    error::Error,
    io,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

use crossterm::{
    event::{self, DisableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use portable_pty::PtySize;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Widget},
};

use crate::{
    lease::{IDLE_AFTER, LeaseDecision, LeaseError, LeaseManager, LeaseState},
    protocol::TakeControl,
    pty_host::PtyHost,
    screen::{GuestScreen, HostScreen, ScreenFrame},
    session::{GuestEvent, GuestPane, HostControlEvent},
};

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

const CONTROL_HELP: &str = "type to claim idle | active typing is protected | Ctrl+Q quit";

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

fn handle_take_control_event(
    lease: &mut LeaseManager,
    lease_tx: &watch::Sender<LeaseState>,
    peer_id: Vec<u8>,
    request: TakeControl,
    now: Instant,
) -> Result<(), LeaseError> {
    match lease.take_control(peer_id, request.known_lease_epoch, now)? {
        LeaseDecision::Publish(state) => {
            lease_tx.send_replace(state);
        }
        LeaseDecision::RejectActiveController => {
            lease_tx.send_replace(lease.state().clone());
        }
        LeaseDecision::AcceptInput(_)
        | LeaseDecision::RejectStaleInput
        | LeaseDecision::RejectStaleRequest => {}
    }
    Ok(())
}

fn handle_input_event(
    lease: &mut LeaseManager,
    lease_tx: &watch::Sender<LeaseState>,
    peer_id: &[u8],
    lease_epoch: u64,
    data: Vec<u8>,
    now: Instant,
) -> Option<Vec<u8>> {
    match lease.input(peer_id, lease_epoch, data, now) {
        LeaseDecision::AcceptInput(bytes) => {
            lease_tx.send_replace(lease.state().clone());
            Some(bytes)
        }
        LeaseDecision::Publish(_)
        | LeaseDecision::RejectStaleInput
        | LeaseDecision::RejectStaleRequest
        | LeaseDecision::RejectActiveController => None,
    }
}

fn resolve_guest_claim(
    pending_control: &mut bool,
    held_input: &mut Vec<u8>,
    claimant_won: bool,
) -> Option<Vec<u8>> {
    if !std::mem::replace(pending_control, false) {
        return None;
    }
    if claimant_won {
        (!held_input.is_empty()).then(|| std::mem::take(held_input))
    } else {
        held_input.clear();
        None
    }
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
        let lease = LeaseManager::new(host_peer_id.clone(), Instant::now());
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

struct VtScreen<'a> {
    screen: &'a vt100::Screen,
}

impl<'a> VtScreen<'a> {
    fn new(screen: &'a vt100::Screen) -> Self {
        Self { screen }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlChrome {
    Active,
    Idle,
}

impl ControlChrome {
    fn from_lease(lease: Option<&LeaseState>, now: Instant) -> Option<Self> {
        lease.map(|lease| {
            if lease.is_idle_at(now) {
                Self::Idle
            } else {
                Self::Active
            }
        })
    }

    fn from_receipt(last_receipt: Option<Instant>, now: Instant) -> Option<Self> {
        last_receipt.map(|receipt| {
            if now.saturating_duration_since(receipt) >= IDLE_AFTER {
                Self::Idle
            } else {
                Self::Active
            }
        })
    }

    fn block(self) -> Block<'static> {
        let (color, label) = match self {
            Self::Active => (Color::Rgb(255, 69, 0), "this user is typing"),
            Self::Idle => (Color::Rgb(140, 91, 68), "this user has control"),
        };
        Block::bordered()
            .border_style(Style::default().fg(color))
            .title(label)
    }
}

impl Widget for VtScreen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        for row in 0..rows.min(area.height) {
            for col in 0..cols.min(area.width) {
                let Some(source) = self.screen.cell(row, col) else {
                    continue;
                };
                if source.is_wide_continuation() {
                    continue;
                }
                let target = &mut buf[(area.x + col, area.y + row)];
                let contents = source.contents();
                target.set_symbol(if contents.is_empty() { " " } else { contents });
                target.set_style(vt_style(source));
            }
        }
    }
}

fn vt_style(cell: &vt100::Cell) -> Style {
    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers.insert(Modifier::BOLD);
    }
    if cell.dim() {
        modifiers.insert(Modifier::DIM);
    }
    if cell.italic() {
        modifiers.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        modifiers.insert(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        modifiers.insert(Modifier::REVERSED);
    }
    Style::default()
        .fg(vt_color(cell.fgcolor()))
        .bg(vt_color(cell.bgcolor()))
        .add_modifier(modifiers)
}

fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn render_guest_screen(
    frame: &mut Frame<'_>,
    screen: &vt100::Screen,
    footer: &str,
    chrome: Option<ControlChrome>,
) {
    let area = frame.area();
    let pane_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let screen_area = if let Some(chrome) = chrome {
        let block = chrome.block();
        let inner = block.inner(pane_area);
        frame.render_widget(block, pane_area);
        inner
    } else {
        pane_area
    };
    frame.render_widget(VtScreen::new(screen), screen_area);
    let (row, col) = screen.cursor_position();
    if !screen.hide_cursor() && row < screen_area.height && col < screen_area.width {
        frame.set_cursor_position((screen_area.x + col, screen_area.y + row));
    }
    if area.height > 0 {
        let footer_y = area.y + pane_area.height;
        frame
            .buffer_mut()
            .set_string(area.x, footer_y, footer, Style::default());
    }
}

fn render_host_screen(
    frame: &mut Frame<'_>,
    screen: &vt100::Screen,
    footer: &str,
    chrome: ControlChrome,
) {
    render_guest_screen(frame, screen, footer, Some(chrome));
}

fn host_footer(join_code: &str) -> String {
    format!("join: p2pmux join {join_code} | {CONTROL_HELP}")
}

fn guest_footer(controller_peer_id: Option<&[u8]>) -> String {
    match controller_peer_id {
        Some(peer_id) => format!(
            "controller: {} typing | {CONTROL_HELP}",
            short_peer(peer_id)
        ),
        None => format!("waiting for control state | {CONTROL_HELP}"),
    }
}

fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL
}

fn encode_key(key: KeyEvent, screen: &vt100::Screen) -> Option<Vec<u8>> {
    if is_quit(key) {
        return None;
    }

    let modifiers = modifier_parameter(key.modifiers)?;
    let bytes = match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let character = character.to_ascii_lowercase();
            if !character.is_ascii_lowercase() {
                return None;
            }
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            bytes.push(character as u8 - b'a' + 1);
            bytes
        }
        KeyCode::Char(character) => {
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            bytes.extend(character.to_string().bytes());
            bytes
        }
        KeyCode::Enter if modifiers == 1 => b"\r".to_vec(),
        KeyCode::Tab if modifiers == 1 => b"\t".to_vec(),
        KeyCode::BackTab if modifiers == 2 => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if modifiers == 1 => b"\x7f".to_vec(),
        KeyCode::Esc if modifiers == 1 => b"\x1b".to_vec(),
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => {
            let suffix = match key.code {
                KeyCode::Up => b'A',
                KeyCode::Down => b'B',
                KeyCode::Right => b'C',
                KeyCode::Left => b'D',
                _ => unreachable!(),
            };
            if modifiers == 1 && screen.application_cursor() {
                vec![0x1b, b'O', suffix]
            } else if modifiers == 1 {
                vec![0x1b, b'[', suffix]
            } else {
                format!("\x1b[1;{modifiers}{}", suffix as char).into_bytes()
            }
        }
        KeyCode::Home if modifiers == 1 => b"\x1b[H".to_vec(),
        KeyCode::End if modifiers == 1 => b"\x1b[F".to_vec(),
        KeyCode::Delete if modifiers == 1 => b"\x1b[3~".to_vec(),
        KeyCode::Insert if modifiers == 1 => b"\x1b[2~".to_vec(),
        KeyCode::PageUp if modifiers == 1 => b"\x1b[5~".to_vec(),
        KeyCode::PageDown if modifiers == 1 => b"\x1b[6~".to_vec(),
        KeyCode::F(number) if modifiers == 1 => function_key(number)?,
        _ => return None,
    };
    Some(bytes)
}

fn modifier_parameter(modifiers: KeyModifiers) -> Option<u8> {
    let supported = KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL;
    if !(modifiers - supported).is_empty() {
        return None;
    }
    let mut parameter = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        parameter += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parameter += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        parameter += 4;
    }
    Some(parameter)
}

fn function_key(number: u8) -> Option<Vec<u8>> {
    let bytes = match number {
        1 => b"\x1bOP".as_slice(),
        2 => b"\x1bOQ".as_slice(),
        3 => b"\x1bOR".as_slice(),
        4 => b"\x1bOS".as_slice(),
        5 => b"\x1b[15~".as_slice(),
        6 => b"\x1b[17~".as_slice(),
        7 => b"\x1b[18~".as_slice(),
        8 => b"\x1b[19~".as_slice(),
        9 => b"\x1b[20~".as_slice(),
        10 => b"\x1b[21~".as_slice(),
        11 => b"\x1b[23~".as_slice(),
        12 => b"\x1b[24~".as_slice(),
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn encode_paste(text: &str, bracketed_paste: bool) -> Vec<u8> {
    if bracketed_paste {
        [
            b"\x1b[200~".as_slice(),
            text.as_bytes(),
            b"\x1b[201~".as_slice(),
        ]
        .concat()
    } else {
        text.as_bytes().to_vec()
    }
}

struct TerminalGuard {
    raw_mode: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self {
            raw_mode: false,
            alternate_screen: false,
            bracketed_paste: false,
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.alternate_screen {
            let _ = execute!(stdout, crossterm::cursor::Show, LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
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

    let mut guard = TerminalGuard::new();
    enable_raw_mode()?;
    guard.raw_mode = true;
    guard.alternate_screen = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.bracketed_paste = true;
    execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(io::stdout());
    let fixed_area = Rect::new(0, 0, cols, rows);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(fixed_area),
        },
    )?;
    let mut dirty = true;

    loop {
        let drain_started = Instant::now();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = host.try_read_output()? else {
                break;
            };
            parser.process(&bytes);
            dirty = true;
        }
        if host.output_closed() {
            break;
        }

        if dirty {
            terminal.draw(|frame| {
                let screen = parser.screen();
                let area = frame.area();
                frame.render_widget(VtScreen::new(screen), area);
                let (row, col) = screen.cursor_position();
                if !screen.hide_cursor() && row < area.height && col < area.width {
                    frame.set_cursor_position((area.x + col, area.y + row));
                }
            })?;
            dirty = false;
        }

        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if is_quit(key) {
                    break;
                }
                if let Some(bytes) = encode_key(key, parser.screen()) {
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
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    let footer = host_footer(&runtime.join_code);
    let mut dirty = true;
    let mut chrome = None;
    loop {
        while let Ok(event) = runtime.control_rx.try_recv() {
            match event {
                HostControlEvent::Input { peer_id, input } => {
                    if let Some(bytes) = handle_input_event(
                        &mut runtime.lease,
                        &runtime.lease_tx,
                        &peer_id,
                        input.lease_epoch,
                        input.data,
                        Instant::now(),
                    ) {
                        runtime.host.write_input(&bytes)?;
                    }
                }
                HostControlEvent::TakeControl { peer_id, request } => {
                    handle_take_control_event(
                        &mut runtime.lease,
                        &runtime.lease_tx,
                        peer_id,
                        request,
                        Instant::now(),
                    )?;
                }
            }
        }
        let drain_started = Instant::now();
        for _ in 0..64 {
            if drain_started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = runtime.host.try_read_output()? else {
                break;
            };
            if let Ok(frame) = runtime.screen.process_pty(&bytes) {
                runtime.screen_tx.send_replace(frame);
            }
            dirty = true;
        }
        if runtime.host.output_closed() {
            break;
        }
        let next_chrome = ControlChrome::from_lease(Some(runtime.lease.state()), Instant::now());
        if chrome != next_chrome {
            dirty = true;
            chrome = next_chrome;
        }
        if dirty {
            terminal.draw(|frame| {
                let screen = runtime.screen.screen();
                render_host_screen(frame, screen, &footer, chrome.expect("host lease state"));
            })?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if is_quit(key) {
                    break;
                }
                if let Some(bytes) = encode_key(key, runtime.screen.screen()) {
                    let now = Instant::now();
                    let epoch = runtime.lease.state().epoch;
                    if runtime.lease.state().controller_peer_id == runtime.host_peer_id {
                        if let Some(bytes) = handle_input_event(
                            &mut runtime.lease,
                            &runtime.lease_tx,
                            &runtime.host_peer_id,
                            epoch,
                            bytes,
                            now,
                        ) {
                            runtime.host.write_input(&bytes)?;
                        }
                    } else if let LeaseDecision::Publish(state) =
                        runtime
                            .lease
                            .take_control(runtime.host_peer_id.clone(), epoch, now)?
                    {
                        runtime.lease_tx.send_replace(state);
                        runtime.host.write_input(&bytes)?;
                    }
                }
            }
            Event::Paste(text) => {
                let bytes = encode_paste(&text, runtime.screen.screen().bracketed_paste());
                let now = Instant::now();
                let epoch = runtime.lease.state().epoch;
                if runtime.lease.state().controller_peer_id == runtime.host_peer_id {
                    if let Some(bytes) = handle_input_event(
                        &mut runtime.lease,
                        &runtime.lease_tx,
                        &runtime.host_peer_id,
                        epoch,
                        bytes,
                        now,
                    ) {
                        runtime.host.write_input(&bytes)?;
                    }
                } else {
                    let decision =
                        runtime
                            .lease
                            .take_control(runtime.host_peer_id.clone(), epoch, now)?;
                    if let LeaseDecision::Publish(state) = decision {
                        runtime.lease_tx.send_replace(state);
                        runtime.host.write_input(&bytes)?;
                    }
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
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

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )?;
    let mut remote = GuestScreen::new();
    let mut footer = guest_footer(None);
    let mut lease = None;
    let mut last_lease = None;
    let mut pending_control = false;
    let mut held_input = Vec::new();
    let mut dirty = true;
    let mut previous_chrome = None;

    loop {
        loop {
            match pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    if remote
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok()
                    {
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    if remote
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok()
                    {
                        dirty = true;
                    }
                }
                Ok(GuestEvent::ScreenGap { .. }) => {}
                Ok(GuestEvent::Lease(state)) => {
                    footer = guest_footer(Some(&state.controller_peer_id));
                    last_lease = Some(Instant::now());
                    if let Some(bytes) = resolve_guest_claim(
                        &mut pending_control,
                        &mut held_input,
                        state.controller_peer_id == pane.controls.peer_id(),
                    ) && pane
                        .controls
                        .try_input(state.lease_epoch, bytes.clone())
                        .is_err()
                    {
                        held_input = bytes;
                    }
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

        if !pending_control
            && !held_input.is_empty()
            && let Some(state) = lease.as_ref()
            && state.controller_peer_id == pane.controls.peer_id()
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

        let chrome = ControlChrome::from_receipt(last_lease, Instant::now());
        if chrome != previous_chrome {
            dirty = true;
            previous_chrome = chrome;
        }

        if dirty {
            terminal.draw(|frame| {
                if let Some(screen) = remote.screen() {
                    render_guest_screen(frame, screen, &footer, chrome);
                }
            })?;
            dirty = false;
        }

        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && is_quit(key) =>
            {
                break;
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen())
                    && let Some(bytes) = encode_key(key, screen)
                {
                    if state.controller_peer_id == pane.controls.peer_id() {
                        if held_input.is_empty() {
                            let _ = pane.controls.try_input(state.lease_epoch, bytes);
                        } else {
                            held_input.extend_from_slice(&bytes);
                        }
                    } else if last_lease.is_some_and(|receipt| receipt.elapsed() >= IDLE_AFTER) {
                        held_input.extend_from_slice(&bytes);
                        if !pending_control {
                            pending_control = true;
                            if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                pending_control = false;
                                held_input.clear();
                            }
                        }
                    }
                }
            }
            Event::Paste(text) => {
                if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen()) {
                    let bytes = encode_paste(&text, screen.bracketed_paste());
                    if state.controller_peer_id == pane.controls.peer_id() {
                        if held_input.is_empty() {
                            let _ = pane.controls.try_input(state.lease_epoch, bytes);
                        } else {
                            held_input.extend_from_slice(&bytes);
                        }
                    } else if last_lease.is_some_and(|receipt| receipt.elapsed() >= IDLE_AFTER) {
                        held_input.extend_from_slice(&bytes);
                        if !pending_control {
                            pending_control = true;
                            if pane.controls.try_take_control(state.lease_epoch).is_err() {
                                pending_control = false;
                                held_input.clear();
                            }
                        }
                    }
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };

    use crate::{
        lease::{LeaseManager, LeaseState},
        protocol::TakeControl,
        screen::{GuestScreen, HostScreen},
    };
    use tokio::sync::watch;

    use super::{
        CONTROL_HELP, ControlChrome, VtScreen, encode_key, encode_paste, guest_footer,
        handle_input_event, handle_take_control_event, host_footer, is_quit, render_guest_screen,
        resolve_guest_claim,
    };

    #[test]
    fn shared_control_help_is_exact_and_used_by_every_footer() {
        assert_eq!(
            CONTROL_HELP,
            "type to claim idle | active typing is protected | Ctrl+Q quit"
        );

        let host = host_footer("abc123");
        let guest = guest_footer(Some(b"controller"));
        let pre_lease_guest = guest_footer(None);

        assert!(host.starts_with("join: p2pmux join abc123 | "));
        assert!(guest.starts_with("controller: 636f6e74 typing | "));
        assert!(pre_lease_guest.starts_with("waiting for control state | "));
        for footer in [&host, &guest, &pre_lease_guest] {
            assert!(footer.ends_with(CONTROL_HELP));
        }
    }

    #[test]
    fn renders_control_chrome_for_an_active_controller() {
        let now = Instant::now();
        let chrome = ControlChrome::from_lease(
            Some(&LeaseState {
                controller_peer_id: b"host".to_vec(),
                epoch: 1,
                last_activity: now,
            }),
            now,
        );
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"abc");
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).expect("test terminal");

        terminal
            .draw(|frame| render_guest_screen(frame, parser.screen(), "footer", chrome))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(255, 69, 0));
        let title: String = (1..20).map(|x| buffer[(x, 0)].symbol()).collect();
        assert_eq!(title, "this user is typing");
        assert_eq!(buffer[(1, 1)].symbol(), "a");
        assert_eq!(buffer[(3, 1)].symbol(), "c");
        assert_eq!(buffer[(0, 4)].symbol(), "f");
    }

    #[test]
    fn renders_control_chrome_for_an_idle_controller() {
        let now = Instant::now();
        let chrome = ControlChrome::from_lease(
            Some(&LeaseState {
                controller_peer_id: b"host".to_vec(),
                epoch: 1,
                last_activity: now - Duration::from_secs(8),
            }),
            now,
        );
        let parser = vt100::Parser::new(1, 1, 0);
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("test terminal");

        terminal
            .draw(|frame| render_guest_screen(frame, parser.screen(), "footer", chrome))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(140, 91, 68));
        let title: String = (1..22).map(|x| buffer[(x, 0)].symbol()).collect();
        assert_eq!(title, "this user has control");
    }

    #[test]
    fn guest_pre_lease_renders_without_a_control_border() {
        let mut parser = vt100::Parser::new(1, 1, 0);
        parser.process(b"x");
        let mut terminal = Terminal::new(TestBackend::new(4, 3)).expect("test terminal");

        terminal
            .draw(|frame| render_guest_screen(frame, parser.screen(), "footer", None))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "x");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
    }

    #[test]
    fn bordered_renderer_crops_the_fixed_grid_to_its_inner_rect() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let now = Instant::now();
        let chrome = ControlChrome::from_lease(
            Some(&LeaseState {
                controller_peer_id: b"host".to_vec(),
                epoch: 1,
                last_activity: now,
            }),
            now,
        );
        let mut terminal = Terminal::new(TestBackend::new(5, 5)).expect("test terminal");

        terminal
            .draw(|frame| render_guest_screen(frame, parser.screen(), "footer", chrome))
            .expect("render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1)].symbol(), "a");
        assert_eq!(buffer[(2, 1)].symbol(), "b");
        assert_eq!(buffer[(3, 1)].symbol(), "c");
        assert_eq!(buffer[(1, 2)].symbol(), "e");
        assert_eq!(buffer[(3, 2)].symbol(), "g");
    }

    #[test]
    fn bordered_renderer_shifts_and_clips_the_cursor_to_its_inner_rect() {
        let now = Instant::now();
        let chrome = ControlChrome::from_lease(
            Some(&LeaseState {
                controller_peer_id: b"host".to_vec(),
                epoch: 1,
                last_activity: now,
            }),
            now,
        );
        let mut parser = vt100::Parser::new(2, 4, 0);
        parser.process(b"ab");
        let mut terminal = Terminal::new(TestBackend::new(5, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_guest_screen(frame, parser.screen(), "footer", chrome))
            .expect("render");

        terminal.backend_mut().assert_cursor_position((3, 1));

        parser.process(b"cd");
        let mut clipped_terminal = Terminal::new(TestBackend::new(5, 4)).expect("test terminal");
        clipped_terminal
            .draw(|frame| render_guest_screen(frame, parser.screen(), "footer", chrome))
            .expect("render");

        clipped_terminal
            .backend_mut()
            .assert_cursor_position((0, 0));
    }

    #[test]
    fn remote_renderer_keeps_host_grid_fixed_and_draws_a_footer() {
        let mut host = HostScreen::new(1, 3).expect("host screen");
        let frame = host.process_pty(b"abc").expect("frame");
        let mut guest = GuestScreen::new();
        guest
            .apply_snapshot(frame.sequence, &frame.snapshot)
            .expect("snapshot");
        let mut terminal = Terminal::new(TestBackend::new(5, 3)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_guest_screen(
                    frame,
                    guest.screen().expect("guest screen"),
                    "controller: abcdef idle",
                    None,
                )
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 0)].symbol(), "c");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].symbol(), " ");
        assert_eq!(buffer[(0, 2)].symbol(), "c");
    }

    #[test]
    fn remote_renderer_places_the_host_cursor() {
        let mut host = HostScreen::new(1, 3).expect("host screen");
        let frame = host.process_pty(b"ab").expect("frame");
        let mut guest = GuestScreen::new();
        guest
            .apply_snapshot(frame.sequence, &frame.snapshot)
            .expect("snapshot");
        let mut terminal = Terminal::new(TestBackend::new(5, 3)).expect("test terminal");

        terminal
            .draw(|frame| {
                render_guest_screen(
                    frame,
                    guest.screen().expect("guest screen"),
                    "controller: peer typing",
                    None,
                );
            })
            .expect("render");

        terminal.backend_mut().assert_cursor_position((2, 0));
    }

    #[test]
    fn renders_vt100_cell_styles() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"\x1b[31;44;1mX");
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render should work");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "X");
        assert_eq!(buffer[(0, 0)].fg, Color::Indexed(1));
        assert_eq!(buffer[(0, 0)].bg, Color::Indexed(4));
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn renderer_keeps_the_parser_grid_fixed() {
        let mut parser = vt100::Parser::new(2, 3, 0);
        parser.process(b"abc\r\ndef");
        let mut terminal = Terminal::new(TestBackend::new(5, 4)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("render should work");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 1)].symbol(), "f");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
    }

    #[test]
    fn renderer_erases_cells_cleared_by_the_pty() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");
        parser.process(b"abc");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("initial render");

        parser.process(b"\x1b[2J\x1b[H");
        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
            .expect("clear render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), " ");
    }

    #[test]
    fn up_respects_application_cursor_mode() {
        let normal = vt100::Parser::new(1, 1, 0);
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                normal.screen()
            ),
            Some(b"\x1b[A".to_vec())
        );

        let mut application = vt100::Parser::new(1, 1, 0);
        application.process(b"\x1b[?1h");
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                application.screen()
            ),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn paste_respects_bracketed_paste_mode() {
        assert_eq!(encode_paste("one\ntwo", false), b"one\ntwo");
        assert_eq!(
            encode_paste("one\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }

    #[test]
    fn encodes_supported_keys_and_reserves_ctrl_q() {
        let parser = vt100::Parser::new(1, 1, 0);
        let screen = parser.screen();
        let cases = [
            (
                KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE),
                Some("é"),
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some("\r"),
            ),
            (KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), Some("\t")),
            (
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                Some("\x7f"),
            ),
            (
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Some("\x1b"),
            ),
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Some("\x03"),
            ),
            (
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
                Some("\x1bx"),
            ),
            (
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                Some("\x1b[H"),
            ),
            (
                KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                Some("\x1b[F"),
            ),
            (
                KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                Some("\x1b[3~"),
            ),
            (
                KeyEvent::new(KeyCode::Insert, KeyModifiers::NONE),
                Some("\x1b[2~"),
            ),
            (
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                Some("\x1b[5~"),
            ),
            (
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                Some("\x1b[6~"),
            ),
            (
                KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
                Some("\x1bOP"),
            ),
            (
                KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE),
                Some("\x1b[20~"),
            ),
            (
                KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE),
                Some("\x1b[21~"),
            ),
            (
                KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
                Some("\x1b[24~"),
            ),
            (
                KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
                Some("\x1b[1;5C"),
            ),
            (
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                None,
            ),
            (KeyEvent::new(KeyCode::Null, KeyModifiers::NONE), None),
        ];

        for (event, expected) in cases {
            assert_eq!(
                encode_key(event, screen).as_deref(),
                expected.map(str::as_bytes),
                "{event:?}"
            );
        }
    }

    #[test]
    fn only_ctrl_q_quits() {
        assert!(is_quit(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_quit(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert!(!is_quit(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE)));
    }

    #[test]
    fn host_take_control_dispatch_rejects_an_active_controller() {
        let now = Instant::now();
        let host_peer_id = b"host".to_vec();
        let mut lease = LeaseManager::new(host_peer_id.clone(), now);
        let (lease_tx, lease_rx) = watch::channel(lease.state().clone());

        handle_take_control_event(
            &mut lease,
            &lease_tx,
            b"guest".to_vec(),
            TakeControl {
                pane_id: b"default-pane".to_vec(),
                requester_peer_id: b"guest".to_vec(),
                known_lease_epoch: 1,
            },
            now + Duration::from_secs(1),
        )
        .expect("active claim is handled");

        assert_eq!(lease.state().controller_peer_id, host_peer_id);
        assert_eq!(lease.state().epoch, 1);
        assert_eq!(lease_rx.borrow().controller_peer_id, host_peer_id);
        assert_eq!(lease_rx.borrow().epoch, 1);
    }

    #[test]
    fn accepted_host_control_input_republishes_the_current_lease() {
        let now = Instant::now();
        let controller_peer_id = b"guest".to_vec();
        let mut lease = LeaseManager::new(controller_peer_id.clone(), now);
        let (lease_tx, mut lease_rx) = watch::channel(lease.state().clone());

        assert_eq!(
            handle_input_event(
                &mut lease,
                &lease_tx,
                &controller_peer_id,
                1,
                b"x".to_vec(),
                now + Duration::from_secs(1),
            ),
            Some(b"x".to_vec())
        );

        assert!(lease_rx.has_changed().expect("lease sender remains open"));
        let published = lease_rx.borrow_and_update().clone();
        assert_eq!(published.controller_peer_id, controller_peer_id);
        assert_eq!(published.epoch, 1);
    }

    #[test]
    fn accepted_guest_claim_releases_the_buffered_first_byte_once() {
        let mut pending_control = true;
        let mut held_input = b"x".to_vec();

        assert_eq!(
            resolve_guest_claim(&mut pending_control, &mut held_input, true),
            Some(b"x".to_vec())
        );
        assert!(!pending_control);
        assert!(held_input.is_empty());
        assert_eq!(
            resolve_guest_claim(&mut pending_control, &mut held_input, true),
            None
        );
    }

    #[test]
    fn serialized_idle_claim_winner_keeps_its_byte_and_loser_buffer_is_cleared() {
        let mut winner_pending_control = true;
        let mut winner_held_input = b"w".to_vec();
        let mut loser_pending_control = true;
        let mut loser_held_input = b"l".to_vec();

        assert_eq!(
            resolve_guest_claim(&mut winner_pending_control, &mut winner_held_input, true,),
            Some(b"w".to_vec())
        );
        assert_eq!(
            resolve_guest_claim(&mut loser_pending_control, &mut loser_held_input, false),
            None
        );
        assert!(loser_held_input.is_empty());
    }
}
