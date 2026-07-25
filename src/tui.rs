//! The fixed-grid local terminal renderer and input loop.

use std::{
    collections::BTreeMap,
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
    widgets::{Block, Paragraph, Widget},
};

use crate::{
    layout::{Axis, LayoutError, LayoutSnapshot, Node, PaneId, TabId},
    lease::{IDLE_AFTER, LeaseDecision, LeaseManager, LeaseState},
    pty_host::PtyHost,
    screen::{GuestScreen, HostScreen, ScreenFrame},
    session::{GuestEvent, GuestPane, HostControlEvent},
};

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

/// The in-progress multi-pane command prefix, kept entirely local to one terminal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChordMode {
    #[default]
    None,
    Pane,
    Tab,
}

/// Metadata used to draw a pane before its runtime has delivered a screen and lease.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PaneViewState {
    pub ready: bool,
    pub host_peer_id: Vec<u8>,
    pub controller_peer_id: Option<Vec<u8>>,
    pub controller_active: bool,
}

/// User operations emitted by the TUI. Session code owns all resulting mutations and PTYs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiIntent {
    CreatePane {
        target_pane_id: PaneId,
        axis: Axis,
        grid_rows: u16,
        grid_cols: u16,
    },
    DeletePane {
        pane_id: PaneId,
    },
    CreateTab {
        grid_rows: u16,
        grid_cols: u16,
    },
    DeleteTab {
        tab_id: TabId,
    },
    FocusPane {
        pane_id: PaneId,
    },
    SwitchTab {
        tab_id: TabId,
    },
}

/// Whether a terminal key belongs to the mux or should later be offered to the focused pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyHandling {
    Forward,
    Consumed(Vec<UiIntent>),
    TakeControl,
    Quit,
}

/// Rectangles for one rendered terminal frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneGeometry {
    pub tab_bar: Rect,
    pub content: Rect,
    pub footer: Rect,
    pub panes: BTreeMap<PaneId, Rect>,
}

/// Pure local rendering and selection state for a revisioned shared layout.
#[derive(Clone, Debug)]
pub struct MultiPaneTui {
    snapshot: LayoutSnapshot,
    current_tab: TabId,
    focused_pane: PaneId,
    chord_mode: ChordMode,
    pane_views: BTreeMap<PaneId, PaneViewState>,
}

impl MultiPaneTui {
    pub fn new(snapshot: LayoutSnapshot) -> Result<Self, LayoutError> {
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let current_tab = snapshot.tabs[0].tab_id;
        let focused_pane = first_leaf(&snapshot.tabs[0].root).expect("validated layout has a leaf");
        let pane_views = snapshot
            .panes
            .values()
            .map(|pane| {
                (
                    pane.pane_id,
                    PaneViewState {
                        host_peer_id: pane.host_peer_id.clone(),
                        ..PaneViewState::default()
                    },
                )
            })
            .collect();
        Ok(Self {
            snapshot,
            current_tab,
            focused_pane,
            chord_mode: ChordMode::None,
            pane_views,
        })
    }

    pub fn snapshot(&self) -> &LayoutSnapshot {
        &self.snapshot
    }

    pub fn current_tab(&self) -> TabId {
        self.current_tab
    }

    pub fn focused_pane(&self) -> PaneId {
        self.focused_pane
    }

    pub fn chord_mode(&self) -> ChordMode {
        self.chord_mode
    }

    pub fn pane_view(&self, pane_id: PaneId) -> Option<&PaneViewState> {
        self.pane_views.get(&pane_id)
    }

    pub fn set_pane_view(&mut self, pane_id: PaneId, state: PaneViewState) {
        if self.snapshot.panes.contains_key(&pane_id) {
            self.pane_views.insert(pane_id, state);
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: LayoutSnapshot) -> Result<(), LayoutError> {
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let old_views = std::mem::take(&mut self.pane_views);
        self.pane_views = snapshot
            .panes
            .values()
            .map(|pane| {
                let mut state = old_views.get(&pane.pane_id).cloned().unwrap_or_default();
                state.host_peer_id = pane.host_peer_id.clone();
                (pane.pane_id, state)
            })
            .collect();
        self.snapshot = snapshot;
        self.repair_selection();
        Ok(())
    }

    pub fn select_tab(&mut self, tab_id: TabId) -> Result<(), LayoutError> {
        let tab = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        self.current_tab = tab_id;
        self.focused_pane = first_leaf(&tab.root).expect("validated layout has a leaf");
        Ok(())
    }

    pub fn geometry(&self, area: Rect) -> PaneGeometry {
        let tab_bar = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let footer_height = area.height.saturating_sub(tab_bar.height).min(1);
        let footer = Rect::new(
            area.x,
            area.y
                .saturating_add(area.height.saturating_sub(footer_height)),
            area.width,
            footer_height,
        );
        let content = Rect::new(
            area.x,
            area.y.saturating_add(tab_bar.height),
            area.width,
            area.height.saturating_sub(tab_bar.height + footer_height),
        );
        let mut panes = BTreeMap::new();
        if let Some(tab) = self.current_tab_layout() {
            allocate_node(&tab.root, content, &mut panes);
        }
        PaneGeometry {
            tab_bar,
            content,
            footer,
            panes,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, area: Rect) -> KeyHandling {
        if is_quit(key) {
            self.chord_mode = ChordMode::None;
            return KeyHandling::Quit;
        }
        if is_take_control(key) {
            self.chord_mode = ChordMode::None;
            return KeyHandling::TakeControl;
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.chord_mode != ChordMode::None
        {
            self.chord_mode = ChordMode::None;
            return KeyHandling::Consumed(vec![]);
        }
        if self.chord_mode == ChordMode::None {
            if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
                self.chord_mode = ChordMode::Pane;
                return KeyHandling::Consumed(vec![]);
            }
            if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
                self.chord_mode = ChordMode::Tab;
                return KeyHandling::Consumed(vec![]);
            }
            return KeyHandling::Forward;
        }

        let chord = self.chord_mode;
        self.chord_mode = ChordMode::None;
        let intent = match chord {
            ChordMode::Pane => self.handle_pane_chord(key, area),
            ChordMode::Tab => self.handle_tab_chord(key, area),
            ChordMode::None => None,
        };
        KeyHandling::Consumed(intent.into_iter().collect())
    }

    fn handle_pane_chord(&mut self, key: KeyEvent, area: Rect) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
                let (grid_rows, grid_cols) = grid_for_pane(rect);
                Some(UiIntent::CreatePane {
                    target_pane_id: self.focused_pane,
                    axis: if rect.width >= rect.height {
                        Axis::LeftRight
                    } else {
                        Axis::TopBottom
                    },
                    grid_rows,
                    grid_cols,
                })
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => Some(UiIntent::DeletePane {
                pane_id: self.focused_pane,
            }),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.is_empty() =>
            {
                self.move_focus(key.code, area)
            }
            _ => None,
        }
    }

    fn handle_tab_chord(&mut self, key: KeyEvent, area: Rect) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let (grid_rows, grid_cols) = grid_for_pane(self.geometry(area).content);
                Some(UiIntent::CreateTab {
                    grid_rows,
                    grid_cols,
                })
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => Some(UiIntent::DeleteTab {
                tab_id: self.current_tab,
            }),
            KeyCode::Left if key.modifiers.is_empty() => self.switch_tab(false),
            KeyCode::Right if key.modifiers.is_empty() => self.switch_tab(true),
            _ => None,
        }
    }

    fn move_focus(&mut self, direction: KeyCode, area: Rect) -> Option<UiIntent> {
        let geometry = self.geometry(area);
        let source = *geometry.panes.get(&self.focused_pane)?;
        let source_center = rect_center(source);
        let mut candidates = geometry
            .panes
            .iter()
            .filter(|(pane_id, _)| **pane_id != self.focused_pane)
            .map(|(pane_id, rect)| (*pane_id, *rect))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let directed = candidates
            .iter()
            .copied()
            .filter(|(_, rect)| is_in_direction(source_center, rect_center(*rect), direction))
            .min_by_key(|(pane_id, rect)| {
                direction_distance(source_center, rect_center(*rect), direction, *pane_id)
            });
        let pane_id = directed
            .or_else(|| {
                candidates.sort_by_key(|(pane_id, rect)| {
                    let center = rect_center(*rect);
                    (
                        source_center.0.abs_diff(center.0) + source_center.1.abs_diff(center.1),
                        *pane_id,
                    )
                });
                candidates.first().copied()
            })?
            .0;
        self.focused_pane = pane_id;
        Some(UiIntent::FocusPane { pane_id })
    }

    fn switch_tab(&mut self, forward: bool) -> Option<UiIntent> {
        let index = self
            .snapshot
            .tabs
            .iter()
            .position(|tab| tab.tab_id == self.current_tab)?;
        let len = self.snapshot.tabs.len();
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        let tab_id = self.snapshot.tabs[next].tab_id;
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(UiIntent::SwitchTab { tab_id })
    }

    fn current_tab_layout(&self) -> Option<&crate::layout::Tab> {
        self.snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == self.current_tab)
    }

    fn repair_selection(&mut self) {
        let current_tab = self.current_tab_layout();
        let current_tab = if let Some(tab) = current_tab {
            tab
        } else {
            self.current_tab = self.snapshot.tabs[0].tab_id;
            &self.snapshot.tabs[0]
        };
        if !contains_leaf(&current_tab.root, self.focused_pane) {
            self.focused_pane = first_leaf(&current_tab.root).expect("validated layout has a leaf");
        }
    }
}

fn first_leaf(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf { pane_id } => Some(*pane_id),
        Node::Split { first, .. } => first_leaf(first),
    }
}

fn contains_leaf(node: &Node, pane_id: PaneId) -> bool {
    match node {
        Node::Leaf { pane_id: candidate } => *candidate == pane_id,
        Node::Split { first, second, .. } => {
            contains_leaf(first, pane_id) || contains_leaf(second, pane_id)
        }
    }
}

fn allocate_node(node: &Node, area: Rect, panes: &mut BTreeMap<PaneId, Rect>) {
    match node {
        Node::Leaf { pane_id } => {
            panes.insert(*pane_id, area);
        }
        Node::Split {
            axis,
            first,
            second,
        } => {
            let (first_area, second_area) = match axis {
                Axis::LeftRight => {
                    let first_width = area.width / 2;
                    (
                        Rect::new(area.x, area.y, first_width, area.height),
                        Rect::new(
                            area.x.saturating_add(first_width),
                            area.y,
                            area.width - first_width,
                            area.height,
                        ),
                    )
                }
                Axis::TopBottom => {
                    let first_height = area.height / 2;
                    (
                        Rect::new(area.x, area.y, area.width, first_height),
                        Rect::new(
                            area.x,
                            area.y.saturating_add(first_height),
                            area.width,
                            area.height - first_height,
                        ),
                    )
                }
            };
            allocate_node(first, first_area, panes);
            allocate_node(second, second_area, panes);
        }
    }
}

fn grid_for_pane(rect: Rect) -> (u16, u16) {
    (
        rect.height.saturating_sub(2).max(1),
        rect.width.saturating_sub(2).max(1),
    )
}

fn rect_center(rect: Rect) -> (u32, u32) {
    (
        u32::from(rect.x) * 2 + u32::from(rect.width),
        u32::from(rect.y) * 2 + u32::from(rect.height),
    )
}

fn is_in_direction(source: (u32, u32), target: (u32, u32), direction: KeyCode) -> bool {
    match direction {
        KeyCode::Left => target.0 < source.0,
        KeyCode::Right => target.0 > source.0,
        KeyCode::Up => target.1 < source.1,
        KeyCode::Down => target.1 > source.1,
        _ => false,
    }
}

fn direction_distance(
    source: (u32, u32),
    target: (u32, u32),
    direction: KeyCode,
    pane_id: PaneId,
) -> (u32, u32, u32, PaneId) {
    match direction {
        KeyCode::Left | KeyCode::Right => {
            let primary = source.0.abs_diff(target.0);
            let secondary = source.1.abs_diff(target.1);
            (primary + secondary, secondary, primary, pane_id)
        }
        KeyCode::Up | KeyCode::Down => {
            let primary = source.1.abs_diff(target.1);
            let secondary = source.0.abs_diff(target.0);
            (primary + secondary, secondary, primary, pane_id)
        }
        _ => (u32::MAX, u32::MAX, u32::MAX, pane_id),
    }
}

fn fixed_grid_viewport(inner: Rect, rows: u16, cols: u16) -> Rect {
    let width = inner.width.min(cols);
    let height = inner.height.min(rows);
    Rect::new(
        inner
            .x
            .saturating_add(inner.width.saturating_sub(width) / 2),
        inner
            .y
            .saturating_add(inner.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

/// Renders layout chrome plus any currently available fixed-size VT screens.
pub fn render_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
) {
    let geometry = tui.geometry(frame.area());
    let tabs = tui
        .snapshot
        .tabs
        .iter()
        .map(|tab| {
            if tab.tab_id == tui.current_tab {
                format!("[*{}]", tab.tab_id)
            } else {
                format!("[{}]", tab.tab_id)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if geometry.tab_bar.width > 0 && geometry.tab_bar.height > 0 {
        frame.buffer_mut().set_string(
            geometry.tab_bar.x,
            geometry.tab_bar.y,
            tabs,
            Style::default().fg(Color::Cyan),
        );
    }
    if geometry.footer.width > 0 && geometry.footer.height > 0 {
        frame.buffer_mut().set_string(
            geometry.footer.x,
            geometry.footer.y,
            "Ctrl+P panes | Ctrl+T tabs | F9 control | F10 quit",
            Style::default().fg(Color::DarkGray),
        );
    }

    for (pane_id, rect) in geometry.panes {
        let pane = &tui.snapshot.panes[&pane_id];
        let view = tui.pane_views.get(&pane_id).cloned().unwrap_or_default();
        let focused = pane_id == tui.focused_pane;
        let lease = match view.controller_peer_id.as_deref() {
            Some(peer) if view.controller_active => format!("ctrl:{} typing", short_peer(peer)),
            Some(peer) => format!("ctrl:{} idle", short_peer(peer)),
            None => String::from("lease: waiting"),
        };
        let title = format!(
            "{} host:{} {lease}",
            if focused { "*" } else { " " },
            short_peer(&view.host_peer_id)
        );
        let border_color = if focused {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        let block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if let Some(screen) = screens.get(&pane_id) {
            frame.render_widget(
                VtScreen::new(screen),
                fixed_grid_viewport(inner, pane.grid_rows, pane.grid_cols),
            );
        } else if !view.ready {
            frame.render_widget(Paragraph::new("waiting for pane snapshot/lease"), inner);
        }
    }
}

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

fn render_guest_screen(frame: &mut Frame<'_>, screen: &vt100::Screen, footer: &str) {
    let area = frame.area();
    let screen_height = screen.size().0.min(area.height.saturating_sub(1));
    let screen_area = Rect::new(area.x, area.y, area.width, screen_height);
    frame.render_widget(VtScreen::new(screen), screen_area);
    let (row, col) = screen.cursor_position();
    if !screen.hide_cursor() && row < screen_height && col < area.width {
        frame.set_cursor_position((area.x + col, area.y + row));
    }
    if area.height > 0 {
        let footer_y = area.y + screen_area.height;
        frame
            .buffer_mut()
            .set_string(area.x, footer_y, footer, Style::default());
    }
}

fn render_host_screen(frame: &mut Frame<'_>, screen: &vt100::Screen, footer: &str) {
    render_guest_screen(frame, screen, footer);
}

fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::F(10) && key.modifiers.is_empty()
}

fn is_take_control(key: KeyEvent) -> bool {
    key.code == KeyCode::F(9) && key.modifiers.is_empty()
}

fn encode_key(key: KeyEvent, screen: &vt100::Screen) -> Option<Vec<u8>> {
    if is_quit(key) || is_take_control(key) {
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
    let footer = format!(
        "join: p2pmux join {} | F9 take control | F10 quit",
        runtime.join_code
    );
    let mut dirty = true;
    loop {
        while let Ok(event) = runtime.control_rx.try_recv() {
            match event {
                HostControlEvent::Input { peer_id, input } => match runtime.lease.input(
                    &peer_id,
                    input.lease_epoch,
                    input.data,
                    Instant::now(),
                ) {
                    LeaseDecision::AcceptInput(bytes) => runtime.host.write_input(&bytes)?,
                    LeaseDecision::Publish(_)
                    | LeaseDecision::RejectStaleInput
                    | LeaseDecision::RejectStaleRequest
                    | LeaseDecision::RejectActiveController => {}
                },
                HostControlEvent::TakeControl { peer_id, request } => {
                    let decision = if request.force {
                        runtime.lease.force_take_control(
                            peer_id,
                            request.known_lease_epoch,
                            Instant::now(),
                        )?
                    } else {
                        runtime.lease.take_control(
                            peer_id,
                            request.known_lease_epoch,
                            Instant::now(),
                        )?
                    };
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
        if dirty {
            terminal.draw(|frame| {
                let screen = runtime.screen.screen();
                render_host_screen(frame, screen, &footer);
                let (row, col) = screen.cursor_position();
                let screen_height = screen.size().0.min(frame.area().height.saturating_sub(1));
                if !screen.hide_cursor() && row < screen_height && col < frame.area().width {
                    frame.set_cursor_position((frame.area().x + col, frame.area().y + row));
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
                if is_take_control(key) {
                    if runtime.lease.state().controller_peer_id != runtime.host_peer_id {
                        let known_epoch = runtime.lease.state().epoch;
                        if let LeaseDecision::Publish(state) = runtime.lease.force_take_control(
                            runtime.host_peer_id.clone(),
                            known_epoch,
                            Instant::now(),
                        )? {
                            runtime.lease_tx.send_replace(state);
                        }
                    }
                } else if let Some(bytes) = encode_key(key, runtime.screen.screen()) {
                    let now = Instant::now();
                    let epoch = runtime.lease.state().epoch;
                    let decision =
                        if runtime.lease.state().controller_peer_id == runtime.host_peer_id {
                            runtime
                                .lease
                                .input(&runtime.host_peer_id, epoch, bytes.clone(), now)
                        } else {
                            runtime
                                .lease
                                .take_control(runtime.host_peer_id.clone(), epoch, now)?
                        };
                    match decision {
                        LeaseDecision::AcceptInput(bytes) => runtime.host.write_input(&bytes)?,
                        LeaseDecision::Publish(state) => {
                            runtime.lease_tx.send_replace(state);
                            runtime.host.write_input(&bytes)?;
                        }
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
                let decision = if runtime.lease.state().controller_peer_id == runtime.host_peer_id {
                    runtime
                        .lease
                        .input(&runtime.host_peer_id, epoch, bytes.clone(), now)
                } else {
                    runtime
                        .lease
                        .take_control(runtime.host_peer_id.clone(), epoch, now)?
                };
                match decision {
                    LeaseDecision::AcceptInput(bytes) => runtime.host.write_input(&bytes)?,
                    LeaseDecision::Publish(state) => {
                        runtime.lease_tx.send_replace(state);
                        runtime.host.write_input(&bytes)?;
                    }
                    LeaseDecision::RejectStaleInput
                    | LeaseDecision::RejectStaleRequest
                    | LeaseDecision::RejectActiveController => {}
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
    let mut footer = String::from("controller: waiting spectator");
    let mut lease = None;
    let mut last_lease = Instant::now();
    let mut received_host_lease = false;
    let mut pending_control = false;
    let mut held_input = Vec::new();
    let mut dirty = true;

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
                Ok(GuestEvent::InitialLease(state)) => {
                    footer = format!(
                        "controller: {} typing",
                        short_peer(&state.controller_peer_id)
                    );
                    lease = Some(state);
                    last_lease = Instant::now();
                    dirty = true;
                }
                Ok(GuestEvent::Lease(state)) => {
                    let already_received_host_lease = received_host_lease;
                    received_host_lease = true;
                    footer = format!(
                        "controller: {} typing",
                        short_peer(&state.controller_peer_id)
                    );
                    last_lease = Instant::now();
                    if pending_control && state.controller_peer_id == pane.controls.peer_id() {
                        pending_control = false;
                    } else if pending_control && already_received_host_lease {
                        pending_control = false;
                        held_input.clear();
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

        if dirty {
            terminal.draw(|frame| {
                if let Some(screen) = remote.screen() {
                    render_guest_screen(frame, screen, &footer);
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
                if is_take_control(key) {
                    if let Some(state) = lease.as_ref()
                        && state.controller_peer_id != pane.controls.peer_id()
                    {
                        pending_control = false;
                        held_input.clear();
                        let _ = pane.controls.try_take_control(state.lease_epoch, true);
                    }
                } else if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen())
                    && let Some(bytes) = encode_key(key, screen)
                {
                    if state.controller_peer_id == pane.controls.peer_id() {
                        if held_input.is_empty() {
                            let _ = pane.controls.try_input(state.lease_epoch, bytes);
                        } else {
                            held_input.extend_from_slice(&bytes);
                        }
                    } else if last_lease.elapsed() >= IDLE_AFTER {
                        held_input.extend_from_slice(&bytes);
                        if !pending_control {
                            pending_control = true;
                            if pane
                                .controls
                                .try_take_control(state.lease_epoch, false)
                                .is_err()
                            {
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
                    } else if last_lease.elapsed() >= IDLE_AFTER {
                        held_input.extend_from_slice(&bytes);
                        if !pending_control {
                            pending_control = true;
                            if pane
                                .controls
                                .try_take_control(state.lease_epoch, false)
                                .is_err()
                            {
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
    use std::collections::BTreeMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Color, Modifier},
    };

    use crate::layout::{Axis, LayoutSnapshot, Node, Pane, Tab};
    use crate::screen::{GuestScreen, HostScreen};

    use super::{
        ChordMode, KeyHandling, MultiPaneTui, PaneViewState, UiIntent, VtScreen, encode_key,
        encode_paste, render_guest_screen, render_multi_pane,
    };

    fn layout(tabs: Vec<Tab>, panes: &[(u64, u16, u16)]) -> LayoutSnapshot {
        LayoutSnapshot {
            revision: 1,
            members: vec![crate::layout::Member {
                peer_id: b"host".to_vec(),
                endpoint_addr: b"endpoint".to_vec(),
            }],
            tabs,
            panes: panes
                .iter()
                .map(|(pane_id, rows, cols)| {
                    (
                        *pane_id,
                        Pane {
                            pane_id: *pane_id,
                            host_peer_id: b"host".to_vec(),
                            grid_rows: *rows,
                            grid_cols: *cols,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn split_layout() -> LayoutSnapshot {
        layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first: Box::new(Node::Leaf { pane_id: 1 }),
                    second: Box::new(Node::Split {
                        axis: Axis::TopBottom,
                        first: Box::new(Node::Leaf { pane_id: 2 }),
                        second: Box::new(Node::Leaf { pane_id: 3 }),
                    }),
                },
            }],
            &[(1, 4, 10), (2, 4, 10), (3, 4, 10)],
        )
    }

    #[test]
    fn multi_pane_geometry_recursively_splits_the_content_area() {
        let tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let geometry = tui.geometry(Rect::new(0, 0, 80, 24));

        assert_eq!(geometry.tab_bar, Rect::new(0, 0, 80, 1));
        assert_eq!(geometry.footer, Rect::new(0, 23, 80, 1));
        assert_eq!(geometry.panes[&1], Rect::new(0, 1, 40, 22));
        assert_eq!(geometry.panes[&2], Rect::new(40, 1, 40, 11));
        assert_eq!(geometry.panes[&3], Rect::new(40, 12, 40, 11));
    }

    #[test]
    fn tiny_terminal_geometry_stays_in_bounds() {
        let tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let geometry = tui.geometry(Rect::new(u16::MAX, u16::MAX, 1, 1));

        assert_eq!(geometry.tab_bar, Rect::new(u16::MAX, u16::MAX, 1, 1));
        assert_eq!(geometry.footer, Rect::new(u16::MAX, u16::MAX, 1, 0));
        assert_eq!(geometry.content, Rect::new(u16::MAX, u16::MAX, 1, 0));
        assert!(
            geometry
                .panes
                .values()
                .all(|rect| rect.x == u16::MAX && rect.y == u16::MAX)
        );
    }

    #[test]
    fn snapshot_commit_repairs_removed_tab_and_pane_selection() {
        let initial = layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        );
        let mut tui = MultiPaneTui::new(initial).expect("valid layout");
        tui.select_tab(2).expect("select second tab");

        tui.apply_snapshot(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid commit");

        assert_eq!(tui.current_tab(), 1);
        assert_eq!(tui.focused_pane(), 1);
    }

    #[test]
    fn chrome_marks_focus_and_reports_host_and_lease() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                host_peer_id: b"host".to_vec(),
                controller_peer_id: Some(b"peer".to_vec()),
                controller_active: true,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(36, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), "┌");
        assert_eq!(buffer[(1, 1)].symbol(), "*");
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "h"));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "t"));
    }

    #[test]
    fn fixed_grid_view_is_centered_and_clipped_inside_pane_chrome() {
        let mut parser = vt100::Parser::new(1, 5, 0);
        parser.process(b"abcde");
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 5)],
        ))
        .expect("valid layout");
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(6, 5)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(1, 2)].symbol(), "a");
        assert_eq!(buffer[(4, 2)].symbol(), "d");
        assert_eq!(buffer[(5, 2)].symbol(), "│");
    }

    #[test]
    fn pane_chord_consumes_commands_and_uses_focused_rect_aspect() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::LeftRight,
                grid_rows: 20,
                grid_cols: 38,
            }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.focused_pane(), 2);
    }

    #[test]
    fn pane_focus_uses_nearest_directional_leaf_then_a_stable_fallback() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 3 }])
        );
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
    }

    #[test]
    fn tab_chord_switches_and_creates_or_deletes_tabs_without_forwarding_keys() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 12, 8);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SwitchTab { tab_id: 2 }])
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreateTab {
                grid_rows: 4,
                grid_cols: 10,
            }])
        );

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 2 }])
        );
    }

    #[test]
    fn normal_keys_escape_and_function_keys_are_classified_without_pty_encoding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), area),
            KeyHandling::TakeControl
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE), area),
            KeyHandling::Quit
        );
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
                )
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(2, 0)].symbol(), "c");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].symbol(), "c");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
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
    fn encodes_supported_keys_and_reserves_f9_and_f10() {
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
            (KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), None),
            (KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE), None),
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
                Some("\x11"),
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
}
