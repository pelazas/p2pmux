//! The fixed-grid local terminal renderer and input loop.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    io,
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{
        self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use iroh::EndpointAddr;
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
    protocol::{
        CreatePane, CreateTab, DeletePane, DeleteTab, LayoutRequest, PaneDescriptor, PaneFailed,
        PaneReady, SplitAxis,
    },
    pty_host::PtyHost,
    screen::{GuestScreen, HostScreen, SCROLLBACK_LINES, ScreenFrame},
    session::{
        CoordinatorResponse, GuestEvent, GuestPane, HostControlEvent, HostPaneChannels,
        LayoutControlEvent, LayoutControlQueueError, PaneLayoutReconciler, PaneServer,
        SharedLayoutHost, SharedLayoutMember, layout_snapshot_from_state, pane_wire_id,
        subscribe_pane,
    },
    transport::Transport,
};

/// Kept as the module's public marker from the scaffold.
pub struct Tui;

const FOOTER_BACKGROUND: Color = Color::Rgb(30, 30, 30);
const FOOTER_MUTED: Color = Color::White;
const FOOTER_ACCENT: Color = Color::Rgb(220, 50, 47);
const TOP_BAR_BRAND: &str = "p2pmux";
const TOP_BAR_BRAND_SEPARATOR: &str = " │ ";
const TAB_BAR_SEPARATOR: &str = " · ";
const CONTROL_HELP: &str = "Ctrl+ <p> PANE   <t> TAB   <q> QUIT    type to claim when free";

type FooterSegment = (&'static str, bool);

const NORMAL_FOOTER: &[FooterSegment] = &[
    ("Ctrl+ <", false),
    ("p", true),
    ("> PANE   <", false),
    ("t", true),
    ("> TAB   <", false),
    ("q", true),
    ("> QUIT    type to claim when free", false),
];
const PANE_FOOTER: &[FooterSegment] = &[
    ("Pane  <", false),
    ("←↓↑→", true),
    ("> FOCUS   <", false),
    ("n", true),
    ("> NEW   <", false),
    ("x", true),
    ("> CLOSE   <", false),
    ("Esc", true),
    ("> BACK", false),
];
const TAB_FOOTER: &[FooterSegment] = &[
    ("Tab  <", false),
    ("←→", true),
    ("> SWITCH   <", false),
    ("n", true),
    ("> NEW   <", false),
    ("x", true),
    ("> CLOSE   <", false),
    ("Esc", true),
    ("> BACK", false),
];

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
    pub controller_peer_id: Option<Vec<u8>>,
    pub controller_active: bool,
    scrollback: usize,
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
    Quit,
}

/// Rectangles for one rendered terminal frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneGeometry {
    pub tab_bar: Rect,
    pub tab_labels: BTreeMap<TabId, Rect>,
    pub content: Rect,
    pub footer: Rect,
    pub panes: BTreeMap<PaneId, Rect>,
}

/// One cell in a pane's fixed VT grid.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ScreenCell {
    row: u16,
    col: u16,
}

/// A local, pane-scoped text selection. Both ends are inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaneTextSelection {
    pane_id: PaneId,
    anchor: ScreenCell,
    cursor: ScreenCell,
}

impl PaneTextSelection {
    fn is_empty(self) -> bool {
        self.anchor == self.cursor
    }

    fn contains(self, cell: ScreenCell) -> bool {
        let (start, end) = self.bounds();
        match cell.row {
            row if row == start.row && row == end.row => (start.col..=end.col).contains(&cell.col),
            row if row == start.row => cell.col >= start.col,
            row if row == end.row => cell.col <= end.col,
            row => (start.row..end.row).contains(&row),
        }
    }

    fn bounds(self) -> (ScreenCell, ScreenCell) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}

/// Pure local rendering and selection state for a revisioned shared layout.
#[derive(Clone, Debug)]
pub struct MultiPaneTui {
    snapshot: LayoutSnapshot,
    current_tab: TabId,
    focused_pane: PaneId,
    chord_mode: ChordMode,
    pane_views: BTreeMap<PaneId, PaneViewState>,
    pending_created_tab: Option<TabId>,
    selection: Option<PaneTextSelection>,
    selection_dragging: bool,
}

impl MultiPaneTui {
    pub fn new(snapshot: LayoutSnapshot) -> Result<Self, LayoutError> {
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let current_tab = snapshot.tabs[0].tab_id;
        let focused_pane = first_leaf(&snapshot.tabs[0].root).expect("validated layout has a leaf");
        let pane_views = snapshot
            .panes
            .keys()
            .map(|pane_id| (*pane_id, PaneViewState::default()))
            .collect();
        Ok(Self {
            snapshot,
            current_tab,
            focused_pane,
            chord_mode: ChordMode::None,
            pane_views,
            pending_created_tab: None,
            selection: None,
            selection_dragging: false,
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

    fn exit_chord_mode(&mut self) {
        self.chord_mode = ChordMode::None;
    }

    fn clear_selection(&mut self) -> bool {
        let changed = self.selection.take().is_some();
        self.selection_dragging = false;
        changed
    }

    fn begin_selection_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        self.selection = Some(PaneTextSelection {
            pane_id,
            anchor: cell,
            cursor: cell,
        });
        self.selection_dragging = true;
        true
    }

    fn extend_selection_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        if !self.selection_dragging {
            return false;
        }
        let Some((pane_id, cell)) = self.screen_cell_at(column, row, area) else {
            return false;
        };
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        if selection.pane_id != pane_id || selection.cursor == cell {
            return false;
        }
        selection.cursor = cell;
        true
    }

    fn end_selection_drag(&mut self) -> bool {
        std::mem::replace(&mut self.selection_dragging, false)
    }

    fn selection(&self) -> Option<PaneTextSelection> {
        self.selection.filter(|selection| !selection.is_empty())
    }

    fn screen_cell_at(&self, column: u16, row: u16, area: Rect) -> Option<(PaneId, ScreenCell)> {
        let geometry = self.geometry(area);
        let (pane_id, pane_rect) = geometry.panes.iter().find_map(|(pane_id, rect)| {
            rect_contains(*rect, column, row).then_some((*pane_id, *rect))
        })?;
        let pane = self.snapshot.panes.get(&pane_id)?;
        let viewport = fixed_grid_viewport(
            Block::bordered().inner(pane_rect),
            pane.grid_rows,
            pane.grid_cols,
        );
        mouse_to_screen_cell(viewport, column, row).map(|cell| (pane_id, cell))
    }

    pub fn pane_view(&self, pane_id: PaneId) -> Option<&PaneViewState> {
        self.pane_views.get(&pane_id)
    }

    fn scrollback_offset(&self, pane_id: PaneId) -> usize {
        self.pane_views
            .get(&pane_id)
            .map_or(0, |view| view.scrollback)
    }

    fn pane_at_or_focused(&self, column: u16, row: u16, area: Rect) -> PaneId {
        pane_at(&self.geometry(area).panes, column, row).unwrap_or(self.focused_pane)
    }

    fn scroll_pane(&mut self, pane_id: PaneId, scrollback_len: usize, up: bool) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        let scrollback = if up {
            view.scrollback.saturating_add(1).min(scrollback_len)
        } else {
            view.scrollback.saturating_sub(1)
        };
        if view.scrollback == scrollback {
            return false;
        }
        view.scrollback = scrollback;
        true
    }

    fn reset_scrollback(&mut self, pane_id: PaneId) -> bool {
        let Some(view) = self.pane_views.get_mut(&pane_id) else {
            return false;
        };
        if view.scrollback == 0 {
            return false;
        }
        view.scrollback = 0;
        true
    }

    pub fn set_pane_view(&mut self, pane_id: PaneId, mut state: PaneViewState) -> bool {
        if self.snapshot.panes.contains_key(&pane_id) {
            state.scrollback = self.scrollback_offset(pane_id);
            if self.pane_views.get(&pane_id) == Some(&state) {
                return false;
            }
            self.pane_views.insert(pane_id, state);
            return true;
        }
        false
    }

    /// Select a tab only when the coordinator publishes the exact ID reserved for this member.
    pub fn select_created_tab(&mut self, tab_id: TabId) {
        self.pending_created_tab = Some(tab_id);
        self.repair_selection();
    }

    pub fn apply_snapshot(&mut self, snapshot: LayoutSnapshot) -> Result<(), LayoutError> {
        crate::layout::SessionState::validate_snapshot(&snapshot)?;
        let old_views = std::mem::take(&mut self.pane_views);
        self.pane_views = snapshot
            .panes
            .values()
            .map(|pane| {
                let state = old_views.get(&pane.pane_id).cloned().unwrap_or_default();
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

    fn focus_pane_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some(pane_id) = pane_at(&self.geometry(area).panes, column, row) else {
            return false;
        };
        if self.focused_pane == pane_id {
            return false;
        }
        self.focused_pane = pane_id;
        true
    }

    fn switch_tab_at(&mut self, column: u16, row: u16, area: Rect) -> Option<UiIntent> {
        let tab_id = self
            .geometry(area)
            .tab_labels
            .iter()
            .find_map(|(tab_id, rect)| rect_contains(*rect, column, row).then_some(*tab_id))?;
        self.select_tab(tab_id)
            .expect("tab came from current snapshot");
        Some(UiIntent::SwitchTab { tab_id })
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
            tab_labels: self.tab_label_rects(tab_bar),
            content,
            footer,
            panes,
        }
    }

    fn tab_label_rects(&self, tab_bar: Rect) -> BTreeMap<TabId, Rect> {
        let mut x = tab_bar
            .x
            .saturating_add(text_width(TOP_BAR_BRAND))
            .saturating_add(text_width(TOP_BAR_BRAND_SEPARATOR));
        let right = tab_bar.x.saturating_add(tab_bar.width);
        self.snapshot
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                if index > 0 {
                    x = x.saturating_add(text_width(TAB_BAR_SEPARATOR));
                }
                let label_width = text_width(&tab_label(index + 1, tab.tab_id == self.current_tab));
                let width = right.saturating_sub(x).min(label_width);
                let rect = Rect::new(x, tab_bar.y, width, tab_bar.height);
                x = x.saturating_add(label_width);
                (tab.tab_id, rect)
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent, area: Rect) -> KeyHandling {
        if is_quit(key) {
            self.chord_mode = ChordMode::None;
            return KeyHandling::Quit;
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
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.chord_mode = ChordMode::Pane;
            return KeyHandling::Consumed(vec![]);
        }
        if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            self.chord_mode = ChordMode::Tab;
            return KeyHandling::Consumed(vec![]);
        }
        let intent = match chord {
            ChordMode::Pane => self.handle_pane_chord(key, area),
            ChordMode::Tab => self.handle_tab_chord(key, area),
            ChordMode::None => None,
        };
        if intent.is_some() || is_chord_command(chord, key) {
            KeyHandling::Consumed(intent.into_iter().collect())
        } else {
            self.chord_mode = ChordMode::None;
            KeyHandling::Forward
        }
    }

    fn handle_pane_chord(&mut self, key: KeyEvent, area: Rect) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
                let (grid_rows, grid_cols) = grid_for_pane(rect);
                Some(UiIntent::CreatePane {
                    target_pane_id: self.focused_pane,
                    axis: if rect.width > rect.height {
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
        let candidates = geometry
            .panes
            .iter()
            .filter(|(pane_id, _)| **pane_id != self.focused_pane)
            .map(|(pane_id, rect)| (*pane_id, *rect))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let pane_id = candidates
            .iter()
            .copied()
            .filter(|(_, rect)| is_in_direction(source_center, rect_center(*rect), direction))
            .min_by_key(|(pane_id, rect)| {
                direction_distance(source_center, rect_center(*rect), direction, *pane_id)
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
        if let Some(tab_id) = self.pending_created_tab
            && let Some(tab) = self.snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id)
        {
            self.current_tab = tab_id;
            self.focused_pane = first_leaf(&tab.root).expect("validated layout has a leaf");
            self.pending_created_tab = None;
            return;
        }
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

fn is_chord_command(mode: ChordMode, key: KeyEvent) -> bool {
    if !key.modifiers.is_empty() {
        return false;
    }
    match mode {
        ChordMode::Pane => matches!(
            key.code,
            KeyCode::Char('n')
                | KeyCode::Char('x')
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
        ),
        ChordMode::Tab => matches!(
            key.code,
            KeyCode::Char('n') | KeyCode::Char('x') | KeyCode::Left | KeyCode::Right
        ),
        ChordMode::None => false,
    }
}

fn first_leaf(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf { pane_id } => Some(*pane_id),
        Node::Split { first, .. } => first_leaf(first),
    }
}

fn visible_leaf_panes(node: &Node) -> Vec<PaneId> {
    match node {
        Node::Leaf { pane_id } => vec![*pane_id],
        Node::Split { first, second, .. } => {
            let mut panes = visible_leaf_panes(first);
            panes.extend(visible_leaf_panes(second));
            panes
        }
    }
}

fn pane_at(panes: &BTreeMap<PaneId, Rect>, column: u16, row: u16) -> Option<PaneId> {
    panes
        .iter()
        .find_map(|(pane_id, rect)| rect_contains(*rect, column, row).then_some(*pane_id))
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    u32::from(column) >= u32::from(rect.x)
        && u32::from(column) < u32::from(rect.x) + u32::from(rect.width)
        && u32::from(row) >= u32::from(rect.y)
        && u32::from(row) < u32::from(rect.y) + u32::from(rect.height)
}

fn text_width(text: &str) -> u16 {
    text.chars().count() as u16
}

fn tab_label(index: usize, active: bool) -> String {
    let label = format!("Tab #{index}");
    if active { format!(" {label} ") } else { label }
}

fn pane_title(
    index: usize,
    host_peer_id: &[u8],
    controller_peer_id: Option<&[u8]>,
    members: &[crate::layout::Member],
) -> String {
    let control = match controller_peer_id {
        Some([]) => "free".to_owned(),
        Some(peer_id) => member_label(peer_id, members),
        None => "…".to_owned(),
    };
    format!(
        "Pane #{index} host: {} control: {control}",
        member_label(host_peer_id, members)
    )
}

fn pane_border_color(
    controller_peer_id: Option<&[u8]>,
    _controller_active: bool,
    focused: bool,
) -> Color {
    match controller_peer_id {
        Some([]) if focused => Color::White,
        None if focused => Color::Yellow,
        Some([]) | None => Color::DarkGray,
        Some(_) => Color::Rgb(255, 69, 0),
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

pub(crate) fn grid_for_pane(rect: Rect) -> (u16, u16) {
    (
        rect.height.saturating_sub(2).max(1),
        rect.width.saturating_sub(2).max(1),
    )
}

pub(crate) fn initial_root_pane_grid(cols: u16, rows: u16) -> (u16, u16) {
    grid_for_pane(Rect::new(0, 0, cols, rows.saturating_sub(2)))
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

fn mouse_to_screen_cell(viewport: Rect, column: u16, row: u16) -> Option<ScreenCell> {
    rect_contains(viewport, column, row).then_some(ScreenCell {
        row: row.saturating_sub(viewport.y),
        col: column.saturating_sub(viewport.x),
    })
}

fn selection_text(screen: &vt100::Screen, selection: PaneTextSelection) -> Option<String> {
    if selection.is_empty() {
        return None;
    }
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }
    let (start, end) = selection.bounds();
    let start = ScreenCell {
        row: start.row.min(rows.saturating_sub(1)),
        col: start.col.min(cols.saturating_sub(1)),
    };
    let end = ScreenCell {
        row: end.row.min(rows.saturating_sub(1)),
        col: end.col.min(cols.saturating_sub(1)),
    };

    let lines = (start.row..=end.row)
        .map(|row| {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row { end.col } else { cols - 1 };
            let mut line = String::new();
            for col in first_col..=last_col {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if !cell.is_wide_continuation() {
                    let contents = cell.contents();
                    line.push_str(if contents.is_empty() { " " } else { contents });
                }
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>();
    Some(lines.join("\n"))
}

fn copy_to_macos_clipboard(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("pbcopy stdin unavailable"))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    if child.wait()?.success() {
        Ok(())
    } else {
        Err(io::Error::other("pbcopy failed"))
    }
}

/// Renders layout chrome plus any currently available fixed-size VT screens.
pub fn render_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
) {
    render_shared_multi_pane(frame, tui, screens, "", None);
}

fn contextual_footer(chord_mode: ChordMode) -> (&'static str, &'static [FooterSegment]) {
    match chord_mode {
        ChordMode::None => (CONTROL_HELP, NORMAL_FOOTER),
        ChordMode::Pane => (
            "Pane  <←↓↑→> FOCUS   <n> NEW   <x> CLOSE   <Esc> BACK",
            PANE_FOOTER,
        ),
        ChordMode::Tab => (
            "Tab  <←→> SWITCH   <n> NEW   <x> CLOSE   <Esc> BACK",
            TAB_FOOTER,
        ),
    }
}

fn render_footer_segments(
    buffer: &mut Buffer,
    mut x: u16,
    y: u16,
    end_x: u16,
    segments: &[FooterSegment],
) -> u16 {
    for (text, accent) in segments {
        let style = Style::default()
            .fg(if *accent { FOOTER_ACCENT } else { FOOTER_MUTED })
            .bg(FOOTER_BACKGROUND);
        x = buffer
            .set_stringn(x, y, text, usize::from(end_x.saturating_sub(x)), style)
            .0;
    }
    x
}

fn footer_suffix(text: &str, width: usize) -> &str {
    if width == 0 {
        return "";
    }
    let start = text
        .char_indices()
        .nth(text.chars().count().saturating_sub(width))
        .map_or(0, |(index, _)| index);
    &text[start..]
}

fn render_contextual_footer(
    buffer: &mut Buffer,
    area: Rect,
    status: &str,
    join_code: Option<&str>,
    chord_mode: ChordMode,
) {
    buffer.set_stringn(
        area.x,
        area.y,
        " ".repeat(usize::from(area.width)),
        usize::from(area.width),
        Style::default().bg(FOOTER_BACKGROUND),
    );

    let end_x = area.right();
    let mut x = area.x;
    if !status.is_empty() {
        x = buffer
            .set_stringn(
                x,
                area.y,
                format!("{status}  "),
                usize::from(end_x.saturating_sub(x)),
                Style::default().fg(FOOTER_MUTED).bg(FOOTER_BACKGROUND),
            )
            .0;
    }

    let join = join_code.map(|code| format!("join: p2pmux join {code}"));
    let (join_x, join_text) = join
        .as_deref()
        .map(|text| {
            let text = footer_suffix(text, usize::from(end_x.saturating_sub(x)));
            (
                end_x.saturating_sub(text.chars().count() as u16),
                Some(text),
            )
        })
        .unwrap_or((end_x, None));
    let (_, segments) = contextual_footer(chord_mode);
    render_footer_segments(buffer, x, area.y, join_x, segments);
    if let Some(text) = join_text {
        buffer.set_stringn(
            join_x,
            area.y,
            text,
            usize::from(end_x.saturating_sub(join_x)),
            Style::default().fg(FOOTER_MUTED).bg(FOOTER_BACKGROUND),
        );
    }
}

fn render_shared_multi_pane(
    frame: &mut Frame<'_>,
    tui: &MultiPaneTui,
    screens: &BTreeMap<PaneId, &vt100::Screen>,
    status: &str,
    join_code: Option<&str>,
) {
    let geometry = tui.geometry(frame.area());
    if geometry.tab_bar.width > 0 && geometry.tab_bar.height > 0 {
        let buffer = frame.buffer_mut();
        buffer.set_stringn(
            geometry.tab_bar.x,
            geometry.tab_bar.y,
            " ".repeat(usize::from(geometry.tab_bar.width)),
            usize::from(geometry.tab_bar.width),
            Style::default().bg(FOOTER_BACKGROUND),
        );
        let mut x = buffer
            .set_stringn(
                geometry.tab_bar.x,
                geometry.tab_bar.y,
                TOP_BAR_BRAND,
                usize::from(geometry.tab_bar.width),
                Style::default().fg(Color::White).bg(FOOTER_BACKGROUND),
            )
            .0;
        x = buffer
            .set_stringn(
                x,
                geometry.tab_bar.y,
                TOP_BAR_BRAND_SEPARATOR,
                usize::from(geometry.tab_bar.right().saturating_sub(x)),
                Style::default().fg(Color::DarkGray).bg(FOOTER_BACKGROUND),
            )
            .0;
        for (index, tab) in tui.snapshot.tabs.iter().enumerate() {
            if index > 0 {
                x = buffer
                    .set_stringn(
                        x,
                        geometry.tab_bar.y,
                        TAB_BAR_SEPARATOR,
                        usize::from(geometry.tab_bar.right().saturating_sub(x)),
                        Style::default().fg(Color::DarkGray).bg(FOOTER_BACKGROUND),
                    )
                    .0;
            }
            let active = tab.tab_id == tui.current_tab;
            let style = if active {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(220, 50, 47))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White).bg(FOOTER_BACKGROUND)
            };
            let label = tab_label(index + 1, active);
            let label_rect = geometry.tab_labels[&tab.tab_id];
            if label_rect.width == 0 {
                continue;
            }
            buffer.set_stringn(
                label_rect.x,
                label_rect.y,
                &label,
                usize::from(label_rect.width),
                style,
            );
            x = x.saturating_add(text_width(&label));
        }
    }
    if geometry.footer.width > 0 && geometry.footer.height > 0 {
        render_contextual_footer(
            frame.buffer_mut(),
            geometry.footer,
            status,
            join_code,
            tui.chord_mode,
        );
    }

    let pane_ids = tui
        .current_tab_layout()
        .map(|tab| visible_leaf_panes(&tab.root))
        .unwrap_or_default();
    for (index, pane_id) in pane_ids.into_iter().enumerate() {
        let Some(rect) = geometry.panes.get(&pane_id).copied() else {
            continue;
        };
        let pane = &tui.snapshot.panes[&pane_id];
        let view = tui.pane_views.get(&pane_id).cloned().unwrap_or_default();
        let focused = pane_id == tui.focused_pane;
        let title = pane_title(
            index + 1,
            &pane.host_peer_id,
            view.controller_peer_id.as_deref(),
            &tui.snapshot.members,
        );
        let border_color = pane_border_color(
            view.controller_peer_id.as_deref(),
            view.controller_active,
            focused,
        );
        let block = Block::bordered()
            .title(format!(" {title} "))
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if let Some(screen) = screens.get(&pane_id) {
            let viewport = fixed_grid_viewport(inner, pane.grid_rows, pane.grid_cols);
            let screen = viewed_screen(screen, tui.scrollback_offset(pane_id));
            frame.render_widget(
                VtScreen::new(&screen).with_selection(
                    tui.selection()
                        .filter(|selection| selection.pane_id == pane_id),
                ),
                viewport,
            );
            let (row, col) = screen.cursor_position();
            if focused
                && view.ready
                && screen.scrollback() == 0
                && !screen.hide_cursor()
                && row < viewport.height
                && col < viewport.width
            {
                frame.set_cursor_position((
                    viewport.x.saturating_add(col),
                    viewport.y.saturating_add(row),
                ));
            }
        } else if !view.ready {
            frame.render_widget(Paragraph::new("waiting for pane snapshot/lease"), inner);
        }
    }
}

/// One fixed-grid PTY owned by this process. Its watch channels are registered with the pane
/// service before the layout coordinator is told that the pane is ready.
pub struct SharedLocalPane {
    pane_id: PaneId,
    host: PtyHost,
    screen: HostScreen,
    lease: LeaseManager,
    host_peer_id: Vec<u8>,
    screen_tx: watch::Sender<ScreenFrame>,
    lease_tx: watch::Sender<LeaseState>,
    control_tx: mpsc::Sender<HostControlEvent>,
    control_rx: mpsc::Receiver<HostControlEvent>,
}

impl SharedLocalPane {
    pub fn spawn(
        pane_id: PaneId,
        grid_rows: u16,
        grid_cols: u16,
        host_peer_id: Vec<u8>,
    ) -> Result<Self, Box<dyn Error>> {
        let size = PtySize {
            rows: grid_rows,
            cols: grid_cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let screen = HostScreen::new(grid_rows, grid_cols)?;
        let (screen_tx, _) = watch::channel(screen.current_frame().clone());
        let lease = LeaseManager::new(Vec::new(), Instant::now());
        let (lease_tx, _) = watch::channel(lease.state().clone());
        let (control_tx, control_rx) = mpsc::channel(256);
        Ok(Self {
            pane_id,
            host: PtyHost::spawn_default_shell(size)?,
            screen,
            lease,
            host_peer_id,
            screen_tx,
            lease_tx,
            control_tx,
            control_rx,
        })
    }

    pub fn channels(&self) -> HostPaneChannels {
        HostPaneChannels {
            pane_id: pane_wire_id(self.pane_id),
            host_peer_id: self.host_peer_id.clone(),
            screen_rx: self.screen_tx.subscribe(),
            lease_rx: self.lease_tx.subscribe(),
            control_tx: self.control_tx.clone(),
        }
    }

    fn view_state(&self) -> PaneViewState {
        PaneViewState {
            ready: true,
            controller_peer_id: Some(self.lease.state().controller_peer_id.clone()),
            controller_active: !self.lease.state().is_idle_at(Instant::now()),
            scrollback: 0,
        }
    }

    fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut changed = false;
        if let Some(state) = self.lease.clear_if_idle(Instant::now())? {
            self.lease_tx.send_replace(state);
            changed = true;
        }
        while let Ok(event) = self.control_rx.try_recv() {
            match event {
                HostControlEvent::Input { peer_id, input } => {
                    if let LeaseDecision::AcceptInput(bytes) =
                        self.lease
                            .input(&peer_id, input.lease_epoch, input.data, Instant::now())
                    {
                        self.host.write_input(&bytes)?;
                        self.lease_tx.send_replace(self.lease.state().clone());
                        changed = true;
                    }
                }
                HostControlEvent::TakeControl { peer_id, request } => {
                    let decision = self.lease.take_control(
                        peer_id,
                        request.known_lease_epoch,
                        Instant::now(),
                    )?;
                    match decision {
                        LeaseDecision::Publish(state) => {
                            self.lease_tx.send_replace(state);
                            changed = true;
                        }
                        // A normal request while the holder is active does not change the lease,
                        // but the requester needs an authoritative re-publication to clear its
                        // pending claim and try again after the idle timeout.
                        LeaseDecision::RejectActiveController => {
                            self.lease_tx.send_replace(self.lease.state().clone());
                            changed = true;
                        }
                        LeaseDecision::AcceptInput(_)
                        | LeaseDecision::RejectStaleInput
                        | LeaseDecision::RejectStaleRequest => {}
                    }
                }
            }
        }
        let started = Instant::now();
        for _ in 0..64 {
            if started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(bytes) = self.host.try_read_output()? else {
                break;
            };
            let frame = self.screen.process_pty(&bytes)?;
            self.screen_tx.send_replace(frame);
            changed = true;
        }
        Ok(changed)
    }

    fn input(&mut self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        let epoch = self.lease.state().epoch;
        let decision = self
            .lease
            .input(&self.host_peer_id, epoch, bytes, Instant::now());
        match decision {
            LeaseDecision::AcceptInput(bytes) => {
                self.host.write_input(&bytes)?;
                self.lease_tx.send_replace(self.lease.state().clone());
            }
            LeaseDecision::Publish(_) => {}
            LeaseDecision::RejectStaleInput
            | LeaseDecision::RejectStaleRequest
            | LeaseDecision::RejectActiveController => {}
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), Box<dyn Error>> {
        self.host.shutdown()
    }
}

struct SharedRemotePane {
    pane: GuestPane,
    screen: GuestScreen,
    lease: Option<LeaseState>,
    last_lease: Instant,
    pending_control: bool,
    held_input: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemotePaneDrain {
    Unchanged,
    Changed,
    Disconnected,
}

fn lease_allows_held_input(controller_peer_id: &[u8], peer_id: &[u8]) -> bool {
    controller_peer_id.is_empty() || controller_peer_id == peer_id
}

fn reconcile_remote_control_attempt(
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

impl SharedRemotePane {
    fn new(pane: GuestPane) -> Self {
        Self {
            pane,
            screen: GuestScreen::new(),
            lease: None,
            last_lease: Instant::now(),
            pending_control: false,
            held_input: Vec::new(),
        }
    }

    fn view_state(&self) -> PaneViewState {
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
        }
    }

    fn drain(&mut self) -> RemotePaneDrain {
        let mut changed = false;
        let mut received_lease = false;
        loop {
            match self.pane.events.try_recv() {
                Ok(GuestEvent::ScreenSnapshot(snapshot)) => {
                    changed |= self
                        .screen
                        .apply_snapshot(snapshot.sequence, &snapshot.screen)
                        .is_ok();
                }
                Ok(GuestEvent::ScreenDelta(delta)) => {
                    changed |= self
                        .screen
                        .apply_delta(delta.base_sequence, delta.sequence, &delta.changes)
                        .is_ok();
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

    fn input(&mut self, bytes: Vec<u8>) {
        let Some(lease) = self.lease.as_ref() else {
            return;
        };
        if lease.controller_peer_id == self.pane.controls.peer_id() {
            if self.held_input.is_empty() {
                let _ = self.pane.controls.try_input(lease.epoch, bytes);
            } else {
                self.held_input.extend_from_slice(&bytes);
            }
        } else if lease.controller_peer_id.is_empty() {
            let _ = self.pane.controls.try_input(lease.epoch, bytes);
        } else if self.last_lease.elapsed() >= IDLE_AFTER && !self.pending_control {
            self.held_input.extend_from_slice(&bytes);
            self.pending_control = self.pane.controls.try_take_control(lease.epoch).is_ok();
            if !self.pending_control {
                self.held_input.clear();
            }
        }
    }
}

enum SharedControl {
    Host(SharedLayoutHost),
    Member(SharedLayoutMember),
}

impl SharedControl {
    fn peer_id(&self) -> Vec<u8> {
        match self {
            Self::Host(host) => host.ticket().endpoint_addr().id.as_bytes().to_vec(),
            Self::Member(member) => member.peer_id.clone(),
        }
    }

    fn try_request(&self, request: LayoutRequest) -> Result<Option<CoordinatorResponse>, String> {
        match self {
            Self::Host(host) => host
                .handle_local_request(request)
                .map(Some)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_request(request)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    fn try_ready(&self, ready: PaneReady) -> Result<Option<CoordinatorResponse>, String> {
        match self {
            Self::Host(host) => host
                .handle_local_ready(ready)
                .map(Some)
                .map_err(|error| error.to_string()),
            Self::Member(member) => member
                .try_ready(ready)
                .map(|()| None)
                .map_err(layout_queue_message),
        }
    }

    fn try_failed(&self, failed: PaneFailed) -> Result<(), String> {
        match self {
            Self::Host(host) => host
                .handle_local_failed(failed)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Self::Member(member) => member.try_failed(failed).map_err(layout_queue_message),
        }
    }

    fn try_event(&mut self, current_revision: u64) -> Option<LayoutControlEvent> {
        match self {
            // The coordinator is the authority, so it does not receive its own broadcasts. Poll
            // its tiny in-memory snapshot to observe joins, departures, and member-originated
            // commits without adding a second internal control stream.
            Self::Host(host) => host
                .session_snapshot()
                .ok()
                .filter(|snapshot| {
                    snapshot
                        .state
                        .as_ref()
                        .is_some_and(|state| state.revision > current_revision)
                })
                .map(LayoutControlEvent::Snapshot),
            Self::Member(member) => member.events.try_recv().ok(),
        }
    }

    async fn shutdown(self) {
        match self {
            Self::Host(host) => host.close().await,
            Self::Member(member) => member.shutdown().await,
        }
    }
}

fn layout_queue_message(error: LayoutControlQueueError) -> String {
    match error {
        LayoutControlQueueError::Full => String::from("layout request queue is busy"),
        LayoutControlQueueError::Closed => String::from("layout coordinator disconnected"),
    }
}

#[derive(Clone, Copy)]
struct PendingCreate {
    request_id: u64,
    base_revision: u64,
    grid_rows: u16,
    grid_cols: u16,
}

/// Pure retry state for direct remote-pane subscriptions. Ticks are supplied by the UI drain
/// loop, which keeps retry behavior deterministic in tests and prevents a failed connect from
/// either becoming permanent or spinning a busy loop.
#[derive(Default)]
struct RemoteSubscriptionState {
    in_flight: BTreeSet<PaneId>,
    retry_at: BTreeMap<PaneId, u64>,
    failures: BTreeMap<PaneId, u8>,
}

impl RemoteSubscriptionState {
    const MAX_BACKOFF_TICKS: u64 = 32;

    fn start(&mut self, pane_id: PaneId, tick: u64) -> bool {
        if self.in_flight.contains(&pane_id)
            || self
                .retry_at
                .get(&pane_id)
                .is_some_and(|retry_at| *retry_at > tick)
        {
            return false;
        }
        self.in_flight.insert(pane_id);
        true
    }

    fn succeeded(&mut self, pane_id: PaneId) {
        self.in_flight.remove(&pane_id);
        self.retry_at.remove(&pane_id);
        self.failures.remove(&pane_id);
    }

    fn failed(&mut self, pane_id: PaneId, tick: u64) {
        self.in_flight.remove(&pane_id);
        let failures = self.failures.entry(pane_id).or_default();
        *failures = failures.saturating_add(1);
        let delay = 1_u64
            .checked_shl(u32::from((*failures).saturating_sub(1)))
            .unwrap_or(Self::MAX_BACKOFF_TICKS)
            .min(Self::MAX_BACKOFF_TICKS);
        self.retry_at.insert(pane_id, tick.saturating_add(delay));
    }

    fn nudge(&mut self) {
        for retry_at in self.retry_at.values_mut() {
            *retry_at = 0;
        }
    }

    fn retain(&mut self, pane_ids: &BTreeSet<PaneId>) {
        self.in_flight.retain(|pane_id| pane_ids.contains(pane_id));
        self.retry_at
            .retain(|pane_id, _| pane_ids.contains(pane_id));
        self.failures
            .retain(|pane_id, _| pane_ids.contains(pane_id));
    }

    #[cfg(test)]
    fn next_retry_at(&self, pane_id: PaneId) -> Option<u64> {
        self.retry_at.get(&pane_id).copied()
    }
}

/// Blocking terminal runtime for the shared layout. Network tasks keep streams independent while
/// this loop only drains ready channels and renders the current fixed grids.
pub struct SharedLayoutRuntime {
    tui: MultiPaneTui,
    control: SharedControl,
    panes: PaneServer,
    reconciler: PaneLayoutReconciler,
    transport: Transport,
    session_id: Vec<u8>,
    runtime: tokio::runtime::Handle,
    local: BTreeMap<PaneId, SharedLocalPane>,
    remote: BTreeMap<PaneId, SharedRemotePane>,
    remote_descriptors: BTreeMap<PaneId, (EndpointAddr, PaneDescriptor)>,
    subscriptions: RemoteSubscriptionState,
    retry_tick: u64,
    subscription_tx: tokio::sync::mpsc::UnboundedSender<(PaneId, Result<GuestPane, String>)>,
    subscription_rx: tokio::sync::mpsc::UnboundedReceiver<(PaneId, Result<GuestPane, String>)>,
    pending_create: Option<PendingCreate>,
    provisional: BTreeMap<u64, PaneId>,
    next_request_id: u64,
    status: String,
    join_code: Option<String>,
}

impl SharedLayoutRuntime {
    pub fn host(
        host: SharedLayoutHost,
        panes: PaneServer,
        snapshot: LayoutSnapshot,
        initial: SharedLocalPane,
        join_code: String,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = host.transport();
        Self::new(
            SharedControl::Host(host),
            panes,
            transport,
            snapshot,
            Some(initial),
            Some(join_code),
            runtime,
        )
    }

    pub fn member(
        member: SharedLayoutMember,
        panes: PaneServer,
        session_id: Vec<u8>,
        snapshot: LayoutSnapshot,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = member.transport();
        let mut value = Self::new(
            SharedControl::Member(member),
            panes,
            transport,
            snapshot,
            None,
            None,
            runtime,
        )?;
        value.session_id = session_id;
        Ok(value)
    }

    /// Builds a member runtime from the first authoritative snapshot. Applying the state before
    /// entering raw-terminal mode both establishes the direct-pane admission roster and starts
    /// nonblocking subscriptions for panes hosted by other members.
    pub fn member_from_state(
        member: SharedLayoutMember,
        panes: PaneServer,
        session_id: Vec<u8>,
        state: crate::protocol::LayoutState,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let snapshot = layout_snapshot_from_state(&state)
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let mut value = Self::member(member, panes, session_id, snapshot, runtime)?;
        value.apply_layout_state(&state)?;
        Ok(value)
    }

    fn new(
        control: SharedControl,
        panes: PaneServer,
        transport: Transport,
        snapshot: LayoutSnapshot,
        initial: Option<SharedLocalPane>,
        join_code: Option<String>,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Box<dyn Error>> {
        let (subscription_tx, subscription_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut local = BTreeMap::new();
        if let Some(initial) = initial {
            local.insert(initial.pane_id, initial);
        }
        let session_id = Vec::new();
        let reconciler = PaneLayoutReconciler::new(panes.clone());
        let mut value = Self {
            tui: MultiPaneTui::new(snapshot)
                .map_err(|error| io::Error::other(format!("invalid layout: {error:?}")))?,
            control,
            panes,
            reconciler,
            transport,
            session_id,
            runtime,
            local,
            remote: BTreeMap::new(),
            remote_descriptors: BTreeMap::new(),
            subscriptions: RemoteSubscriptionState::default(),
            retry_tick: 0,
            subscription_tx,
            subscription_rx,
            pending_create: None,
            provisional: BTreeMap::new(),
            next_request_id: 1,
            status: String::new(),
            join_code,
        };
        value.refresh_local_views();
        Ok(value)
    }

    pub fn set_session_id(&mut self, session_id: Vec<u8>) {
        self.session_id = session_id;
    }

    pub fn run(mut self) -> Result<(), Box<dyn Error>> {
        let (cols, rows) = terminal::size()?;
        let mut guard = TerminalGuard::new();
        enable_raw_mode()?;
        guard.raw_mode = true;
        guard.alternate_screen = true;
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.bracketed_paste = true;
        execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
        guard.mouse_capture = true;
        execute!(io::stdout(), EnableMouseCapture)?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;
        let mut dirty = true;
        loop {
            dirty |= self.drain()?;
            if dirty {
                let mut screens = BTreeMap::new();
                for (pane_id, pane) in &self.local {
                    screens.insert(*pane_id, pane.screen.screen());
                }
                for (pane_id, pane) in &self.remote {
                    if let Some(screen) = pane.screen.screen() {
                        screens.insert(*pane_id, screen);
                    }
                }
                terminal.draw(|frame| {
                    render_shared_multi_pane(
                        frame,
                        &self.tui,
                        &screens,
                        &self.status,
                        self.join_code.as_deref(),
                    );
                })?;
                dirty = false;
            }
            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match self.tui.handle_key(key, Rect::new(0, 0, cols, rows)) {
                        KeyHandling::Quit => break,
                        KeyHandling::Consumed(intents) => {
                            for intent in intents {
                                self.handle_intent(intent)?;
                            }
                        }
                        KeyHandling::Forward => self.forward_key(key)?,
                    }
                    dirty = true;
                }
                Event::Paste(text) => {
                    self.tui.exit_chord_mode();
                    self.forward_paste(&text)?;
                    dirty = true;
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    let area = Rect::new(0, 0, cols, rows);
                    dirty |= self.tui.clear_selection();
                    if let Some(intent) = self.tui.switch_tab_at(mouse.column, mouse.row, area) {
                        self.handle_intent(intent)?;
                        dirty = true;
                    } else {
                        dirty |= self.tui.focus_pane_at(mouse.column, mouse.row, area);
                        dirty |= self.tui.begin_selection_at(mouse.column, mouse.row, area);
                    }
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) =>
                {
                    dirty |= self.tui.extend_selection_at(
                        mouse.column,
                        mouse.row,
                        Rect::new(0, 0, cols, rows),
                    );
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) =>
                {
                    if self.tui.end_selection_drag() {
                        self.copy_selection_to_clipboard();
                    }
                    dirty = true;
                }
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    let area = Rect::new(0, 0, cols, rows);
                    let pane_id = self.tui.pane_at_or_focused(mouse.column, mouse.row, area);
                    let scrollback_len = self
                        .local
                        .get(&pane_id)
                        .map(|pane| available_scrollback(pane.screen.screen()))
                        .or_else(|| {
                            self.remote
                                .get(&pane_id)
                                .and_then(|pane| pane.screen.screen())
                                .map(available_scrollback)
                        })
                        .unwrap_or(0);
                    dirty |= self.tui.scroll_pane(
                        pane_id,
                        scrollback_len,
                        matches!(mouse.kind, MouseEventKind::ScrollUp),
                    );
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        self.shutdown();
        Ok(())
    }

    fn copy_selection_to_clipboard(&mut self) {
        let Some(selection) = self.tui.selection() else {
            return;
        };
        let scrollback = self.tui.scrollback_offset(selection.pane_id);
        let text = self
            .local
            .get(&selection.pane_id)
            .and_then(|pane| {
                selection_text(&viewed_screen(pane.screen.screen(), scrollback), selection)
            })
            .or_else(|| {
                self.remote
                    .get(&selection.pane_id)
                    .and_then(|pane| pane.screen.screen())
                    .and_then(|screen| {
                        selection_text(&viewed_screen(screen, scrollback), selection)
                    })
            });
        let Some(text) = text else {
            return;
        };
        if let Err(error) = copy_to_macos_clipboard(&text) {
            self.status = format!("clipboard copy failed: {error}");
        }
    }

    fn drain(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut changed = false;
        self.retry_tick = self.retry_tick.saturating_add(1);
        while let Some(event) = self.control.try_event(self.tui.snapshot().revision) {
            self.handle_control_event(event)?;
            changed = true;
        }
        while let Ok((pane_id, result)) = self.subscription_rx.try_recv() {
            match result {
                Ok(pane) => {
                    if self.remote_descriptors.contains_key(&pane_id) {
                        self.subscriptions.succeeded(pane_id);
                        self.remote.insert(pane_id, SharedRemotePane::new(pane));
                    } else {
                        self.spawn_remote_shutdown(pane);
                    }
                }
                Err(error) => {
                    self.subscriptions.failed(pane_id, self.retry_tick);
                    self.status = format!("pane {pane_id}: {error}; retrying");
                }
            }
            changed = true;
        }
        self.start_eligible_subscriptions();
        for pane in self.local.values_mut() {
            changed |= pane.drain()?;
        }
        let disconnected = self
            .remote
            .iter_mut()
            .filter_map(|(pane_id, pane)| match pane.drain() {
                RemotePaneDrain::Unchanged => None,
                RemotePaneDrain::Changed => {
                    changed = true;
                    None
                }
                RemotePaneDrain::Disconnected => Some(*pane_id),
            })
            .collect::<Vec<_>>();
        for pane_id in disconnected {
            if let Some(pane) = self.remote.remove(&pane_id) {
                self.spawn_remote_shutdown(pane.pane);
            }
            if self.remote_descriptors.contains_key(&pane_id) {
                self.subscriptions.failed(pane_id, self.retry_tick);
                self.status = format!("pane {pane_id} disconnected; retrying");
            }
            changed = true;
        }
        changed |= self.refresh_local_views();
        Ok(changed)
    }

    fn spawn_remote_shutdown(&self, pane: GuestPane) {
        self.runtime.spawn(async move { pane.shutdown().await });
    }

    fn refresh_local_views(&mut self) -> bool {
        let mut changed = false;
        for (pane_id, pane) in &self.local {
            changed |= self.tui.set_pane_view(*pane_id, pane.view_state());
        }
        for (pane_id, pane) in &self.remote {
            changed |= self.tui.set_pane_view(*pane_id, pane.view_state());
        }
        changed
    }

    fn handle_control_event(&mut self, event: LayoutControlEvent) -> Result<(), Box<dyn Error>> {
        self.reconciler.apply(&event)?;
        match event {
            LayoutControlEvent::Snapshot(snapshot) => {
                self.apply_layout_state(snapshot.state.as_ref().ok_or("missing layout state")?)?;
            }
            LayoutControlEvent::Commit(commit) => {
                self.apply_layout_state(commit.state.as_ref().ok_or("missing layout state")?)?;
            }
            LayoutControlEvent::Reservation(reservation) => self.accept_reservation(reservation)?,
            LayoutControlEvent::Reject(reject) => self.reject_request(reject.request_id),
            LayoutControlEvent::Disconnected => {
                self.status = String::from("layout coordinator disconnected")
            }
        }
        Ok(())
    }

    fn apply_layout_state(
        &mut self,
        state: &crate::protocol::LayoutState,
    ) -> Result<(), Box<dyn Error>> {
        let snapshot = layout_snapshot_from_state(state)
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let current_ids = snapshot.panes.keys().copied().collect::<BTreeSet<_>>();
        // A successful authoritative commit is the only point at which a provisional local PTY
        // becomes a real pane. Forget the request bookkeeping then; rejection handles the other
        // path and tears the provisional PTY down.
        self.provisional
            .retain(|_, pane_id| !current_ids.contains(pane_id));
        let local_ids = self.local.keys().copied().collect::<Vec<_>>();
        for pane_id in local_ids {
            if !current_ids.contains(&pane_id) {
                let _ = self.panes.remove_local_pane(pane_id)?;
                if let Some(mut pane) = self.local.remove(&pane_id) {
                    pane.shutdown()?;
                }
            }
        }
        let remote_ids = self.remote.keys().copied().collect::<Vec<_>>();
        for pane_id in remote_ids {
            if !current_ids.contains(&pane_id)
                && let Some(pane) = self.remote.remove(&pane_id)
            {
                self.spawn_remote_shutdown(pane.pane);
            }
        }
        self.tui
            .apply_snapshot(snapshot.clone())
            .map_err(|error| io::Error::other(format!("invalid layout state: {error:?}")))?;
        let me = self.control.peer_id();
        self.remote_descriptors.clear();
        for pane in state.panes.iter().filter(|pane| pane.host_peer_id != me) {
            let endpoint = state
                .members
                .iter()
                .find(|member| member.peer_id == pane.host_peer_id)
                .and_then(|member| serde_json::from_slice(&member.endpoint_addr).ok());
            let Some(endpoint) = endpoint else {
                self.status = format!("pane {} has no usable host address", pane.pane_id);
                continue;
            };
            self.remote_descriptors
                .insert(pane.pane_id, (endpoint, pane.clone()));
        }
        let remote_ids = self
            .remote_descriptors
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        self.subscriptions.retain(&remote_ids);
        self.subscriptions.nudge();
        self.start_eligible_subscriptions();
        self.refresh_local_views();
        Ok(())
    }

    fn start_eligible_subscriptions(&mut self) {
        for (pane_id, (endpoint, descriptor)) in self.remote_descriptors.clone() {
            if self.remote.contains_key(&pane_id)
                || !self.subscriptions.start(pane_id, self.retry_tick)
            {
                continue;
            }
            let tx = self.subscription_tx.clone();
            let transport = self.transport.clone();
            let session_id = self.session_id.clone();
            self.runtime.spawn(async move {
                let result = subscribe_pane(transport, session_id, endpoint, descriptor)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send((pane_id, result));
            });
        }
    }

    fn handle_intent(&mut self, intent: UiIntent) -> Result<(), Box<dyn Error>> {
        match intent {
            UiIntent::CreatePane {
                target_pane_id,
                axis,
                grid_rows,
                grid_cols,
            } => {
                self.begin_create(Some((target_pane_id, axis)), grid_rows, grid_cols)?;
            }
            UiIntent::CreateTab {
                grid_rows,
                grid_cols,
            } => {
                self.begin_create(None, grid_rows, grid_cols)?;
            }
            UiIntent::DeletePane { pane_id } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: Some(DeletePane { pane_id }),
                    create_tab: None,
                    delete_tab: None,
                })?
            }
            UiIntent::DeleteTab { tab_id } => {
                let request_id = self.next_id();
                self.send_request(LayoutRequest {
                    request_id,
                    base_revision: self.tui.snapshot().revision,
                    create_pane: None,
                    delete_pane: None,
                    create_tab: None,
                    delete_tab: Some(DeleteTab { tab_id }),
                })?
            }
            UiIntent::FocusPane { .. } | UiIntent::SwitchTab { .. } => {}
        }
        Ok(())
    }

    fn begin_create(
        &mut self,
        pane: Option<(PaneId, Axis)>,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<(), Box<dyn Error>> {
        if self.pending_create.is_some() {
            self.status = String::from("waiting for current pane reservation");
            return Ok(());
        }
        let request_id = self.next_id();
        let base_revision = self.tui.snapshot().revision;
        self.pending_create = Some(PendingCreate {
            request_id,
            base_revision,
            grid_rows,
            grid_cols,
        });
        self.send_request(LayoutRequest {
            request_id,
            base_revision,
            create_pane: pane.map(|(target_pane_id, axis)| CreatePane {
                target_pane_id,
                axis: Some(match axis {
                    Axis::LeftRight => SplitAxis::LeftRight as i32,
                    Axis::TopBottom => SplitAxis::TopBottom as i32,
                }),
                grid_rows: u32::from(grid_rows),
                grid_cols: u32::from(grid_cols),
            }),
            delete_pane: None,
            create_tab: pane.is_none().then_some(CreateTab {
                grid_rows: u32::from(grid_rows),
                grid_cols: u32::from(grid_cols),
            }),
            delete_tab: None,
        })
    }

    fn send_request(&mut self, request: LayoutRequest) -> Result<(), Box<dyn Error>> {
        if let Some(response) = self.control.try_request(request)? {
            match response {
                CoordinatorResponse::Reservation(reservation) => {
                    self.accept_reservation(reservation)?
                }
                CoordinatorResponse::Commit(commit) => {
                    self.handle_control_event(LayoutControlEvent::Commit(commit))?
                }
                CoordinatorResponse::Reject(reject) => self.reject_request(reject.request_id),
            }
        }
        Ok(())
    }

    fn accept_reservation(
        &mut self,
        reservation: crate::protocol::PaneReservation,
    ) -> Result<(), Box<dyn Error>> {
        let Some(pending) = self.pending_create.take() else {
            self.status = String::from("unexpected pane reservation");
            return Ok(());
        };
        let host_peer_id = self.control.peer_id();
        let pane = match SharedLocalPane::spawn(
            reservation.pane_id,
            pending.grid_rows,
            pending.grid_cols,
            host_peer_id.clone(),
        ) {
            Ok(pane) => pane,
            Err(error) => {
                let _ = self.control.try_failed(PaneFailed {
                    reservation_id: reservation.reservation_id,
                    request_id: pending.request_id,
                    base_revision: pending.base_revision,
                });
                self.status = format!("pane spawn failed: {error}");
                return Ok(());
            }
        };
        let descriptor = PaneDescriptor {
            pane_id: reservation.pane_id,
            host_peer_id,
            grid_rows: u32::from(pending.grid_rows),
            grid_cols: u32::from(pending.grid_cols),
        };
        if let Err(error) = self.panes.register_local_pane(descriptor, pane.channels()) {
            let _ = self.control.try_failed(PaneFailed {
                reservation_id: reservation.reservation_id,
                request_id: pending.request_id,
                base_revision: pending.base_revision,
            });
            self.status = format!("pane registration failed: {error}");
            return Ok(());
        }
        self.provisional
            .insert(pending.request_id, reservation.pane_id);
        self.local.insert(reservation.pane_id, pane);
        if let Some(tab_id) = reservation.tab_id {
            self.tui.select_created_tab(tab_id);
        }
        match self.control.try_ready(PaneReady {
            reservation_id: reservation.reservation_id,
            request_id: pending.request_id,
            base_revision: pending.base_revision,
        })? {
            Some(CoordinatorResponse::Commit(commit)) => {
                self.handle_control_event(LayoutControlEvent::Commit(commit))?
            }
            Some(CoordinatorResponse::Reject(reject)) => self.reject_request(reject.request_id),
            Some(CoordinatorResponse::Reservation(_)) | None => {}
        }
        Ok(())
    }

    fn reject_request(&mut self, request_id: u64) {
        self.pending_create = self
            .pending_create
            .filter(|pending| pending.request_id != request_id);
        if let Some(pane_id) = self.provisional.remove(&request_id) {
            let _ = self.panes.remove_local_pane(pane_id);
            if let Some(mut pane) = self.local.remove(&pane_id) {
                let _ = pane.shutdown();
            }
        }
        self.status = format!("layout request {request_id} rejected");
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1).max(1);
        id
    }

    fn forward_key(&mut self, key: KeyEvent) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
        let mut sent = false;
        if let Some(pane) = self.local.get_mut(&pane_id)
            && let Some(bytes) = encode_key(key, pane.screen.screen())
        {
            pane.input(bytes)?;
            sent = true;
        }
        if let Some(pane) = self.remote.get_mut(&pane_id)
            && let Some(screen) = pane.screen.screen()
            && let Some(bytes) = encode_key(key, screen)
        {
            pane.input(bytes);
            sent = true;
        }
        if sent {
            self.tui.reset_scrollback(pane_id);
        }
        Ok(())
    }

    fn forward_paste(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        let pane_id = self.tui.focused_pane();
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

    fn shutdown(mut self) {
        for (_, mut pane) in std::mem::take(&mut self.local) {
            let _ = self.panes.remove_local_pane(pane.pane_id);
            let _ = pane.shutdown();
        }
        for (_, pane) in std::mem::take(&mut self.remote) {
            self.runtime.block_on(pane.pane.shutdown());
        }
        self.runtime.block_on(self.control.shutdown());
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

struct VtScreen<'a> {
    screen: &'a vt100::Screen,
    selection: Option<PaneTextSelection>,
}

impl<'a> VtScreen<'a> {
    fn new(screen: &'a vt100::Screen) -> Self {
        Self {
            screen,
            selection: None,
        }
    }

    fn with_selection(mut self, selection: Option<PaneTextSelection>) -> Self {
        self.selection = selection;
        self
    }
}

fn viewed_screen(screen: &vt100::Screen, scrollback: usize) -> vt100::Screen {
    let mut screen = screen.clone();
    screen.set_scrollback(scrollback);
    screen
}

fn available_scrollback(screen: &vt100::Screen) -> usize {
    let mut screen = screen.clone();
    screen.set_scrollback(SCROLLBACK_LINES);
    screen.scrollback()
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
                let style = if self
                    .selection
                    .is_some_and(|selection| selection.contains(ScreenCell { row, col }))
                {
                    vt_style(source)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::REVERSED)
                } else {
                    vt_style(source)
                };
                target.set_style(style);
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
    mouse_capture: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self {
            raw_mode: false,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_capture: false,
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
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
    let footer = format!("{CONTROL_HELP} | join: p2pmux join {}", runtime.join_code);
    let mut dirty = true;
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
                if let Some(bytes) = encode_key(key, runtime.screen.screen()) {
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
    let mut pending_control = false;
    let mut held_input = Vec::new();
    let mut dirty = true;

    loop {
        let mut received_lease = false;
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
                if let (Some(state), Some(screen)) = (lease.as_ref(), remote.screen())
                    && let Some(bytes) = encode_key(key, screen)
                {
                    if state.controller_peer_id == pane.controls.peer_id() {
                        if held_input.is_empty() {
                            let _ = pane.controls.try_input(state.lease_epoch, bytes);
                        } else {
                            held_input.extend_from_slice(&bytes);
                        }
                    } else if state.controller_peer_id.is_empty() {
                        let _ = pane.controls.try_input(state.lease_epoch, bytes);
                    } else if last_lease.elapsed() >= IDLE_AFTER {
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
                    } else if state.controller_peer_id.is_empty() {
                        let _ = pane.controls.try_input(state.lease_epoch, bytes);
                    } else if last_lease.elapsed() >= IDLE_AFTER {
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
    use std::{
        collections::BTreeMap,
        net::Ipv4Addr,
        thread,
        time::{Duration, Instant},
    };
    use tokio::sync::{mpsc, watch};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use portable_pty::PtySize;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Color, Modifier},
    };

    use crate::layout::{Axis, LayoutSnapshot, Node, Pane, Tab};
    use crate::lease::{IDLE_AFTER, LeaseManager, LeaseState};
    use crate::screen::{GuestScreen, HostScreen};
    use crate::{
        protocol::PaneDescriptor,
        session::{HostSession, SharedLayoutHost, layout_snapshot_from_state},
        transport::{ALPN, Transport},
    };
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    use super::{
        ChordMode, HostControlEvent, HostPaneChannels, HostPaneRuntime, KeyHandling,
        LayoutControlEvent, MultiPaneTui, PaneTextSelection, PaneViewState,
        RemoteSubscriptionState, ScreenCell, SharedLayoutRuntime, SharedLocalPane, UiIntent,
        VtScreen, contextual_footer, encode_key, encode_paste, grid_for_pane,
        initial_root_pane_grid, lease_allows_held_input, member_label, mouse_to_screen_cell,
        pane_border_color, pane_title, pane_wire_id, reconcile_remote_control_attempt,
        render_guest_screen, render_multi_pane, render_shared_multi_pane, selection_text,
        viewed_screen, visible_leaf_panes,
    };

    fn layout(tabs: Vec<Tab>, panes: &[(u64, u16, u16)]) -> LayoutSnapshot {
        LayoutSnapshot {
            revision: 1,
            members: vec![crate::layout::Member {
                peer_id: b"host".to_vec(),
                endpoint_addr: b"endpoint".to_vec(),
                display_name: String::new(),
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

    #[test]
    fn failed_remote_subscription_retries_with_bounded_backoff_and_commit_nudge() {
        let mut subscriptions = RemoteSubscriptionState::default();
        assert!(subscriptions.start(7, 0));
        subscriptions.failed(7, 0);
        assert!(!subscriptions.start(7, 0));
        assert!(subscriptions.start(7, 1));
        subscriptions.failed(7, 1);
        assert!(!subscriptions.start(7, 2));
        assert!(subscriptions.start(7, 3));

        for tick in [3, 7, 15, 31, 63, 95] {
            subscriptions.failed(7, tick);
            assert!(
                subscriptions
                    .next_retry_at(7)
                    .is_some_and(|next| next <= tick + 32)
            );
        }
        subscriptions.nudge();
        assert!(subscriptions.start(7, 95));
    }

    async fn loopback_transport() -> Transport {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("localhost address")
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .expect("loopback endpoint");
        Transport::from_endpoint(endpoint)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_create_then_committed_delete_updates_its_local_pane_lifecycle() {
        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            2,
            8,
        )
        .expect("shared host");
        let pane_server = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let initial = SharedLocalPane::spawn(1, 2, 8, host_id.clone()).expect("initial pty");
        pane_server
            .register_local_pane(
                PaneDescriptor {
                    pane_id: 1,
                    host_peer_id: host_id,
                    grid_rows: 2,
                    grid_cols: 8,
                },
                initial.channels(),
            )
            .expect("initial pane registered");
        let state = host
            .session_snapshot()
            .expect("snapshot")
            .state
            .expect("layout state");
        let snapshot = layout_snapshot_from_state(&state).expect("render layout");
        let mut runtime = SharedLayoutRuntime::host(
            host,
            pane_server,
            snapshot,
            initial,
            String::from("TESTCODE"),
            tokio::runtime::Handle::current(),
        )
        .expect("runtime");
        runtime.set_session_id(b"session".to_vec());

        runtime
            .handle_intent(UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::LeftRight,
                grid_rows: 2,
                grid_cols: 8,
            })
            .expect("create intent commits after registering a local pane");
        assert!(runtime.local.contains_key(&2));
        assert!(runtime.panes.has_registered_pane(2).expect("pane registry"));
        assert!(
            runtime.provisional.is_empty(),
            "committed panes clear provisional state"
        );
        assert_eq!(runtime.tui.snapshot().panes.len(), 2);

        runtime
            .handle_intent(UiIntent::DeletePane { pane_id: 2 })
            .expect("host-owned deletion commits");
        assert!(!runtime.local.contains_key(&2));
        assert!(
            !runtime.panes.has_registered_pane(2).expect("pane registry"),
            "committed removal revokes the direct-pane service before the PTY is shut down"
        );
        assert_eq!(runtime.tui.snapshot().panes.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn member_runtime_reconciles_snapshot_into_a_direct_remote_pane_attachment() {
        let host = SharedLayoutHost::new(
            HostSession::from_transport(loopback_transport().await).expect("host session"),
            1,
            1,
        )
        .expect("shared host");
        let host_panes = host.pane_server();
        let host_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
        let screen = HostScreen::new(1, 1).expect("screen");
        let (_screen_tx, screen_rx) = watch::channel(screen.current_frame().clone());
        let (_lease_tx, lease_rx) = watch::channel(LeaseState {
            controller_peer_id: host_id.clone(),
            epoch: 1,
            last_activity: Instant::now(),
        });
        let (control_tx, _control_rx) = mpsc::channel(8);
        let descriptor = PaneDescriptor {
            pane_id: 1,
            host_peer_id: host_id.clone(),
            grid_rows: 1,
            grid_cols: 1,
        };
        host_panes
            .register_local_pane(
                descriptor.clone(),
                HostPaneChannels {
                    pane_id: pane_wire_id(1),
                    host_peer_id: host_id,
                    screen_rx: screen_rx.clone(),
                    lease_rx: lease_rx.clone(),
                    control_tx: control_tx.clone(),
                },
            )
            .expect("host pane");
        let dispatcher = host
            .incoming_dispatcher(host_panes.clone())
            .expect("single dispatcher");
        let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });

        let mut member =
            crate::session::join_layout(loopback_transport().await, host.ticket().clone())
                .await
                .expect("member joins");
        let LayoutControlEvent::Snapshot(snapshot) = member.events.recv().await.expect("snapshot")
        else {
            panic!("member must receive snapshot first");
        };
        let state = snapshot.state.expect("state");
        let member_panes = member
            .pane_server(host.ticket().session_id().to_vec())
            .expect("member pane server");
        let mut runtime = SharedLayoutRuntime::member_from_state(
            member,
            member_panes,
            host.ticket().session_id().to_vec(),
            state.clone(),
            tokio::runtime::Handle::current(),
        )
        .expect("member runtime");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain");
            if runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            runtime.remote.contains_key(&1),
            "remote pane attached from snapshot"
        );

        host_panes
            .remove_local_pane(1)
            .expect("remove direct pane")
            .expect("registered pane");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain after direct close");
            if !runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !runtime.remote.contains_key(&1),
            "a direct stream close removes the stale remote pane"
        );
        host_panes
            .register_local_pane(
                descriptor,
                HostPaneChannels {
                    pane_id: pane_wire_id(1),
                    host_peer_id: host.ticket().endpoint_addr().id.as_bytes().to_vec(),
                    screen_rx,
                    lease_rx,
                    control_tx,
                },
            )
            .expect("restore direct pane");
        runtime
            .apply_layout_state(&state)
            .expect("authoritative snapshot nudges reconnect");
        for _ in 0..20 {
            runtime.drain().expect("runtime drain after restore");
            if runtime.remote.contains_key(&1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            runtime.remote.contains_key(&1),
            "remote pane reconnects after a transient direct close"
        );
        dispatcher_task.abort();
    }

    #[test]
    fn shared_footer_keeps_status_visible_when_a_join_code_is_present() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(80, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "layout request rejected",
                    Some("TESTCODE"),
                );
            })
            .expect("draw");
        let footer = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        assert!(footer.starts_with("layout request rejected"));
        assert!(footer.ends_with("join: p2pmux join TESTCODE"));
        assert_eq!(terminal.backend().buffer()[(0, 4)].fg, Color::White);
        let join_x = footer.find("join:").expect("join code rendered") as u16;
        assert_eq!(terminal.backend().buffer()[(join_x, 4)].fg, Color::White);

        let mut narrow_terminal = Terminal::new(TestBackend::new(20, 5)).expect("terminal");
        narrow_terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "layout request rejected",
                    Some("TESTCODE"),
                );
            })
            .expect("draw");
        let narrow_footer = (0..20)
            .map(|x| narrow_terminal.backend().buffer()[(x, 4)].symbol())
            .collect::<String>();
        assert!(narrow_footer.starts_with("layout request"));
        assert!(!narrow_footer.contains("join"));
    }

    #[test]
    fn pane_view_activity_transition_reports_a_redraw() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 1)],
        );
        let mut tui = MultiPaneTui::new(snapshot).expect("layout");
        let active = PaneViewState {
            ready: true,
            controller_peer_id: Some(b"controller".to_vec()),
            controller_active: true,
            scrollback: 0,
        };
        assert!(tui.set_pane_view(1, active.clone()));
        assert!(!tui.set_pane_view(1, active));
        assert!(tui.set_pane_view(
            1,
            PaneViewState {
                controller_active: false,
                ..tui.pane_view(1).expect("pane view").clone()
            },
        ));
    }

    #[test]
    fn shared_renderer_draws_status_before_join_code_in_the_footer() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 1)],
        );
        let tui = MultiPaneTui::new(snapshot).expect("layout");
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render_shared_multi_pane(
                    frame,
                    &tui,
                    &BTreeMap::new(),
                    "layout request rejected",
                    Some("TESTCODE"),
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("layout request rejected"));
        assert!(rendered.contains("join: p2pmux join TESTCODE"));
    }

    #[test]
    fn rejected_busy_claim_republishes_lease_and_idle_claim_succeeds() {
        let owner = b"owner".to_vec();
        let requester = b"guest".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, owner.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::new(owner.clone(), Instant::now());
        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: requester.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: requester.clone(),
                    known_lease_epoch: 1,
                },
            })
            .expect("busy takeover event");
        pane.drain().expect("drain busy takeover");
        assert!(lease_rx.has_changed().expect("lease watch"));
        assert_eq!(lease_rx.borrow_and_update().controller_peer_id, owner);

        pane.lease = LeaseManager::with_epoch_for_test(
            pane.host_peer_id.clone(),
            1,
            Instant::now() - IDLE_AFTER,
        );
        pane.control_tx
            .try_send(HostControlEvent::TakeControl {
                peer_id: requester.clone(),
                request: crate::protocol::TakeControl {
                    pane_id: pane_wire_id(99),
                    requester_peer_id: requester.clone(),
                    known_lease_epoch: 2,
                },
            })
            .expect("idle takeover event");
        pane.drain().expect("drain idle takeover");
        assert_eq!(pane.lease.state().controller_peer_id, requester);
        assert_eq!(pane.lease.state().epoch, 3);
    }

    #[test]
    fn idle_controller_is_cleared_and_published_to_watchers() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease = LeaseManager::with_epoch_for_test(host_id, 1, Instant::now() - IDLE_AFTER);

        pane.drain().expect("drain idle controller");

        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert!(lease.controller_peer_id.is_empty());
        assert_eq!(lease.epoch, 2);
    }

    #[test]
    fn local_input_crossing_the_idle_timeout_reclaims_and_delivers_it() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");
        let mut lease_rx = pane.lease_tx.subscribe();
        pane.lease =
            LeaseManager::with_epoch_for_test(b"remote".to_vec(), 1, Instant::now() - IDLE_AFTER);

        pane.input(b"printf boundary-input-delivered\\n".to_vec())
            .expect("first input at idle boundary");

        assert_eq!(pane.lease.state().controller_peer_id, host_id);
        assert_eq!(pane.lease.state().epoch, 3);
        assert!(lease_rx.has_changed().expect("lease watch"));
        let lease = lease_rx.borrow_and_update().clone();
        assert_eq!(lease.controller_peer_id, b"host");
        assert_eq!(lease.epoch, 3);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            while let Some(bytes) = pane.host.try_read_output().expect("PTY reader") {
                output.extend(bytes);
            }
            if String::from_utf8_lossy(&output).contains("boundary-input-delivered") {
                pane.shutdown().expect("shutdown local pane");
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pane.shutdown().expect("shutdown local pane");
        panic!(
            "first input was not delivered to the PTY: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn new_local_pane_starts_free_while_the_host_retains_pty_ownership() {
        let host_id = b"host".to_vec();
        let mut pane = SharedLocalPane::spawn(99, 1, 1, host_id.clone()).expect("local pane");

        assert!(pane.lease.state().controller_peer_id.is_empty());
        assert_eq!(pane.host_peer_id, host_id);

        pane.shutdown().expect("shutdown local pane");
    }

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
    fn pane_title_uses_stable_leaf_order_and_control_state() {
        let snapshot = layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Split {
                    axis: Axis::LeftRight,
                    first: Box::new(Node::Leaf { pane_id: 8 }),
                    second: Box::new(Node::Leaf { pane_id: 3 }),
                },
            }],
            &[(3, 1, 1), (8, 1, 1)],
        );
        let mut members = snapshot.members.clone();
        members[0].display_name = String::from("Host");
        members.push(crate::layout::Member {
            peer_id: b"guest".to_vec(),
            endpoint_addr: b"guest-endpoint".to_vec(),
            display_name: String::from("Guest"),
        });

        assert_eq!(visible_leaf_panes(&snapshot.tabs[0].root), vec![8, 3]);
        assert_eq!(
            pane_title(1, b"host", Some(b""), &members),
            "Pane #1 host: Host control: free"
        );
        assert_eq!(
            pane_title(2, b"host", Some(b"guest"), &members),
            "Pane #2 host: Host control: Guest"
        );
        assert_eq!(
            pane_title(2, b"host", None, &members),
            "Pane #2 host: Host control: …"
        );
    }

    #[test]
    fn free_panes_use_white_when_focused_and_dark_gray_when_unfocused() {
        assert_eq!(pane_border_color(Some(b""), false, true), Color::White);
        assert_eq!(pane_border_color(Some(b""), false, false), Color::DarkGray);
        assert_eq!(pane_border_color(None, false, true), Color::Yellow);
        assert_eq!(pane_border_color(None, false, false), Color::DarkGray);
        assert_eq!(
            pane_border_color(Some(b"guest"), true, true),
            Color::Rgb(255, 69, 0)
        );
        assert_ne!(
            pane_border_color(Some(b"guest"), false, false),
            Color::Rgb(140, 91, 68)
        );
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
    fn mouse_hit_testing_focuses_the_clicked_pane_and_ignores_misses() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );

        assert!(tui.focus_pane_at(60, 17, area));
        assert_eq!(tui.focused_pane(), 3);
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert!(!tui.focus_pane_at(10, 0, area));
        assert_eq!(tui.focused_pane(), 3);
    }

    #[test]
    fn mouse_coordinates_map_to_visible_screen_cells_only() {
        let viewport = Rect::new(10, 5, 3, 2);

        assert_eq!(
            mouse_to_screen_cell(viewport, 12, 6),
            Some(ScreenCell { row: 1, col: 2 })
        );
        assert_eq!(mouse_to_screen_cell(viewport, 9, 5), None);
        assert_eq!(mouse_to_screen_cell(viewport, 13, 6), None);
    }

    #[test]
    fn mouse_wheel_adjusts_the_hovered_pane_scrollback_with_clamping() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let pane_id = tui.pane_at_or_focused(60, 17, area);
        assert_eq!(pane_id, 3);

        assert!(tui.scroll_pane(pane_id, 2, true));
        assert!(tui.scroll_pane(pane_id, 2, true));
        assert!(!tui.scroll_pane(pane_id, 2, true));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 2);
        assert!(tui.scroll_pane(pane_id, 2, false));
        assert!(tui.scroll_pane(pane_id, 2, false));
        assert!(!tui.scroll_pane(pane_id, 2, false));
        assert_eq!(tui.pane_view(pane_id).expect("pane view").scrollback, 0);
    }

    #[test]
    fn input_resets_the_pane_scrollback_to_the_live_edge() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        assert!(tui.scroll_pane(1, 4, true));
        assert!(tui.reset_scrollback(1));
        assert_eq!(tui.pane_view(1).expect("pane view").scrollback, 0);
        assert!(!tui.reset_scrollback(1));
    }

    #[test]
    fn selection_text_uses_stream_semantics_in_document_order() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 2, col: 1 },
            cursor: ScreenCell { row: 0, col: 2 },
        };

        assert_eq!(
            selection_text(parser.screen(), selection),
            Some("cd\nefgh\nij".to_owned())
        );
    }

    #[test]
    fn selection_text_uses_the_scrolled_view_cells() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let screen = viewed_screen(parser.screen(), 1);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 0, col: 0 },
            cursor: ScreenCell { row: 0, col: 1 },
        };

        assert_eq!(selection_text(&screen, selection), Some("on".to_owned()));
    }

    #[test]
    fn empty_selection_has_no_clipboard_text() {
        let parser = vt100::Parser::new(1, 1, 0);
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 0, col: 0 },
            cursor: ScreenCell { row: 0, col: 0 },
        };

        assert_eq!(selection_text(parser.screen(), selection), None);
    }

    #[test]
    fn tab_hit_testing_switches_the_clicked_rendered_label_without_exiting_tab_mode() {
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
        let area = Rect::new(0, 0, 32, 8);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );

        let second_tab = tui.geometry(area).tab_labels[&2];
        assert_eq!(
            tui.switch_tab_at(second_tab.x, 0, area),
            Some(UiIntent::SwitchTab { tab_id: 2 })
        );
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        assert_eq!(tui.switch_tab_at(0, 0, area), None);
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
    fn chrome_reports_the_pane_host_and_controller() {
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
                controller_peer_id: Some(b"peer".to_vec()),
                controller_active: true,
                scrollback: 0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(36, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), "┌");
        assert_eq!(buffer[(1, 1)].symbol(), " ");
        assert_eq!(buffer[(2, 1)].symbol(), "P");
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "h"));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "t"));
    }

    #[test]
    fn pane_badge_uses_the_snapshot_host_not_mutable_view_state() {
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
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let title = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();

        assert!(title.contains("host: 686f7374"));
        assert!(!title.contains("host: 66616b65"));
    }

    #[test]
    fn focused_ready_pane_maps_its_visible_vt_cursor_into_the_letterboxed_viewport() {
        let mut parser = vt100::Parser::new(1, 3, 0);
        parser.process(b"ab");
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 3)],
        ))
        .expect("valid layout");
        tui.set_pane_view(
            1,
            PaneViewState {
                ready: true,
                controller_peer_id: None,
                controller_active: false,
                scrollback: 0,
            },
        );
        let screens = BTreeMap::from([(1, parser.screen())]);
        let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &screens))
            .expect("render");

        terminal.backend_mut().assert_cursor_position((5, 3));
    }

    #[test]
    fn focused_pane_never_draws_a_hidden_or_clipped_vt_cursor() {
        for sequence in [b"\x1b[?25l".as_slice(), b"abcd".as_slice()] {
            let mut parser = vt100::Parser::new(1, 5, 0);
            parser.process(sequence);
            let mut tui = MultiPaneTui::new(layout(
                vec![Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                }],
                &[(1, 1, 3)],
            ))
            .expect("valid layout");
            tui.set_pane_view(
                1,
                PaneViewState {
                    ready: true,
                    controller_peer_id: None,
                    controller_active: false,
                    scrollback: 0,
                },
            );
            let screens = BTreeMap::from([(1, parser.screen())]);
            let mut terminal = Terminal::new(TestBackend::new(9, 7)).expect("test terminal");

            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &screens))
                .expect("render");

            assert!(!terminal.backend().cursor_visible());
        }
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
        assert_eq!(tui.chord_mode(), ChordMode::Pane);

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
    fn sticky_pane_mode_moves_focus_across_multiple_arrows() {
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
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 3 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
    }

    #[test]
    fn escape_clears_sticky_chord_mode_without_forwarding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn forwarding_key_exits_sticky_pane_mode_once() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn paste_exits_sticky_chord_mode_before_forwarding() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );

        tui.exit_chord_mode();

        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn shared_footer_uses_help_for_each_chord_mode() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        for (mode, expected) in [
            (
                ChordMode::None,
                "Ctrl+ <p> PANE   <t> TAB   <q> QUIT    type to claim when free",
            ),
            (
                ChordMode::Pane,
                "Pane  <←↓↑→> FOCUS   <n> NEW   <x> CLOSE   <Esc> BACK",
            ),
            (
                ChordMode::Tab,
                "Tab  <←→> SWITCH   <n> NEW   <x> CLOSE   <Esc> BACK",
            ),
        ] {
            tui.chord_mode = mode;
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = (0..120)
                .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
                .collect::<String>();
            assert!(
                footer.starts_with(expected),
                "mode: {mode:?}, footer: {footer}"
            );
        }
    }

    #[test]
    fn shared_footer_accents_key_glyphs_on_a_dark_background() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).expect("test terminal");

        for mode in [ChordMode::None, ChordMode::Pane, ChordMode::Tab] {
            tui.chord_mode = mode;
            terminal
                .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
                .expect("render");
            let footer = terminal.backend().buffer();
            assert_eq!(footer[(0, 3)].bg, Color::Rgb(30, 30, 30));
            let (_, segments) = contextual_footer(mode);
            let mut x = 0;
            for (text, accent) in segments {
                for key in text.chars() {
                    if *accent {
                        assert_eq!(
                            footer[(x, 3)].fg,
                            Color::Rgb(220, 50, 47),
                            "mode: {mode:?}, key: {key}"
                        );
                    } else {
                        assert_eq!(footer[(x, 3)].fg, Color::White, "mode: {mode:?}");
                    }
                    x += 1;
                }
            }
        }
    }

    #[test]
    fn tab_bar_uses_a_branded_footer_like_strip_and_highlights_the_active_tab() {
        let tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 10,
                    root: Node::Leaf { pane_id: 1 },
                },
                Tab {
                    tab_id: 20,
                    root: Node::Leaf { pane_id: 2 },
                },
            ],
            &[(1, 2, 2), (2, 2, 2)],
        ))
        .expect("valid layout");
        let mut terminal = Terminal::new(TestBackend::new(32, 4)).expect("test terminal");

        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let tab_bar = (0..32).map(|x| buffer[(x, 0)].symbol()).collect::<String>();

        assert!(tab_bar.starts_with("p2pmux │  Tab #1  · Tab #2"));
        assert!(tab_bar.contains("Tab #1"));
        assert!(tab_bar.contains("Tab #2"));
        assert_eq!(buffer[(0, 0)].fg, Color::White);
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(30, 30, 30));
        assert_eq!(buffer[(9, 0)].fg, Color::White);
        assert_eq!(buffer[(9, 0)].bg, Color::Rgb(220, 50, 47));
        assert_eq!(buffer[(20, 0)].fg, Color::White);
        assert_eq!(buffer[(20, 0)].bg, Color::Rgb(30, 30, 30));
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

    #[test]
    fn initial_root_pane_grid_matches_its_bordered_viewport() {
        let tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 1)],
        ))
        .expect("valid layout");

        let pane = tui
            .geometry(Rect::new(0, 0, 80, 24))
            .panes
            .get(&1)
            .copied()
            .expect("root pane");
        assert_eq!(grid_for_pane(pane), (20, 78));
        assert_eq!(initial_root_pane_grid(80, 24), (20, 78));
    }

    #[test]
    fn square_pane_create_chord_splits_top_to_bottom() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 1, 1)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 12, 14);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::TopBottom,
                grid_rows: 10,
                grid_cols: 10,
            }])
        );
    }

    #[test]
    fn pane_delete_chord_targets_the_focused_pane() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area);
        assert_eq!(tui.focused_pane(), 2);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeletePane { pane_id: 2 }])
        );
    }

    #[test]
    fn tall_pane_create_chord_uses_a_top_bottom_split_and_usable_grid() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");
        let area = Rect::new(0, 0, 20, 40);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::CreatePane {
                target_pane_id: 1,
                axis: Axis::TopBottom,
                grid_rows: 36,
                grid_cols: 18,
            }])
        );
    }

    #[test]
    fn forwarding_keys_exit_a_chord_mode_and_are_forwarded() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        for prefix in ['p', 't'] {
            assert_eq!(
                tui.handle_key(
                    KeyEvent::new(KeyCode::Char(prefix), KeyModifiers::CONTROL),
                    area
                ),
                KeyHandling::Consumed(vec![])
            );
            assert_eq!(
                tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
                KeyHandling::Forward
            );
            assert_eq!(tui.chord_mode(), ChordMode::None);
        }
    }

    #[test]
    fn pane_focus_uses_the_nearest_directional_leaf_and_stops_at_edges() {
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
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.focused_pane(), 3);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.focused_pane(), 3);

        let mut edge_tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let _ = edge_tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            edge_tui.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(edge_tui.focused_pane(), 1);

        let _ = edge_tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = edge_tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area);
        assert_eq!(edge_tui.focused_pane(), 2);
        for key in [KeyCode::Up, KeyCode::Right] {
            let _ = edge_tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            );
            assert_eq!(
                edge_tui.handle_key(KeyEvent::new(key, KeyModifiers::NONE), area),
                KeyHandling::Consumed(vec![])
            );
            assert_eq!(edge_tui.focused_pane(), 2);
        }
    }

    #[test]
    fn creator_selects_only_its_explicitly_reserved_tab_after_that_tab_commits() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            &[(1, 2, 2)],
        ))
        .expect("valid layout");

        tui.select_created_tab(2);
        tui.apply_snapshot(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },
                },
            ],
            &[(1, 2, 2), (3, 2, 2)],
        ))
        .expect("unrelated commit");
        assert_eq!(tui.current_tab(), 1);

        tui.apply_snapshot(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },
                },
            ],
            &[(1, 2, 2), (2, 2, 2), (3, 2, 2)],
        ))
        .expect("targeted tab commit");

        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
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
    fn sticky_tab_mode_switches_tabs_repeatedly() {
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
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },
                },
            ],
            &[(1, 2, 2), (2, 2, 2), (3, 2, 2)],
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
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SwitchTab { tab_id: 3 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
    }

    #[test]
    fn normal_keys_escape_and_function_keys_leave_f9_and_f10_for_the_pty() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
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
            KeyHandling::Forward
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                area,
            ),
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
    fn renderer_highlights_stream_selected_cells() {
        let mut parser = vt100::Parser::new(3, 4, 0);
        parser.process(b"abcd\r\nefgh\r\nijkl");
        let selection = PaneTextSelection {
            pane_id: 1,
            anchor: ScreenCell { row: 2, col: 1 },
            cursor: ScreenCell { row: 0, col: 2 },
        };
        let mut terminal = Terminal::new(TestBackend::new(4, 3)).expect("test terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(
                    VtScreen::new(parser.screen()).with_selection(Some(selection)),
                    frame.area(),
                )
            })
            .expect("render should work");

        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(1, 0)].bg, Color::DarkGray);
        assert_eq!(buffer[(2, 0)].bg, Color::DarkGray);
        assert_eq!(buffer[(0, 1)].bg, Color::DarkGray);
        assert_eq!(buffer[(1, 2)].bg, Color::DarkGray);
        assert_ne!(buffer[(2, 2)].bg, Color::DarkGray);
        assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
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
    fn renderer_uses_the_requested_scrollback_view() {
        let mut parser = vt100::Parser::new(1, 3, 10);
        parser.process(b"one\r\ntwo");
        let live = parser.screen().clone();
        let history = viewed_screen(parser.screen(), 1);
        assert_eq!(history.scrollback(), 1);
        assert_ne!(
            history.cell(0, 0).expect("history cell").contents(),
            live.cell(0, 0).expect("live cell").contents()
        );
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("test terminal");

        terminal
            .draw(|frame| frame.render_widget(VtScreen::new(&history), frame.area()))
            .expect("render should work");

        assert_eq!(
            terminal.backend().buffer()[(0, 0)].symbol(),
            history.cell(0, 0).expect("history cell").contents()
        );
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
    fn encodes_supported_keys_and_reserves_ctrl_q_only() {
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
}
