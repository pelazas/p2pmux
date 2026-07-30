//! What a keystroke does: quit, chord entry, chord commands, modal input,
//! and everything that falls through to the focused PTY.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::{
    layout::{Axis, NewPanePosition, TabId, normalize_title},
    tui::{
        AGENT_TOGGLE_WINDOW, ChordMode, KeyHandling, ModalState, MultiPaneTui, RenamePrompt,
        RenameTarget, ShareCopy, UiIntent,
        debug_log::ui_debug_log,
        geometry::{
            direction_distance, grid_for_pane, is_in_direction, rect_center, visible_leaf_panes,
        },
        input::keys::{is_chord_command, is_chord_navigation, is_option_arrow, is_quit},
    },
};

pub(in crate::tui) const CHORD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

impl MultiPaneTui {
    pub(in crate::tui) fn exit_chord_mode(&mut self) {
        self.chord_mode = ChordMode::None;
        self.chord_last_activity = None;
    }

    pub(in crate::tui) fn enter_chord_mode(&mut self, mode: ChordMode) {
        self.chord_mode = mode;
        self.chord_last_activity = Some(Instant::now());
    }

    pub(in crate::tui) fn touch_chord_activity(&mut self) {
        if self.chord_mode != ChordMode::None {
            self.chord_last_activity = Some(Instant::now());
        }
    }

    /// Clears sticky pane/tab mode after [`CHORD_IDLE_TIMEOUT`] without a key.
    pub(crate) fn expire_chord_mode(&mut self, now: Instant) -> bool {
        let Some(last) = self.chord_last_activity else {
            return false;
        };
        if self.chord_mode == ChordMode::None
            || now.saturating_duration_since(last) < CHORD_IDLE_TIMEOUT
        {
            return false;
        }
        self.exit_chord_mode();
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent, area: Rect) -> KeyHandling {
        if is_quit(key) {
            self.modal = ModalState::None;
            self.pending_agent_toggle = None;
            self.exit_chord_mode();
            return KeyHandling::Quit;
        }
        if matches!(self.modal, ModalState::Rename(_)) {
            return self.handle_rename_key(key);
        }
        if matches!(self.modal, ModalState::ConfirmDeleteTab { .. }) {
            return self.handle_confirm_delete_tab_key(key);
        }
        // Ctrl+S is claimed before the pane sees it, so a pane never receives XOFF from this
        // binding — the same trade already made for Ctrl+A.
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            self.modal = if self.share_open() {
                ModalState::None
            } else {
                ModalState::Share
            };
            self.exit_chord_mode();
            return KeyHandling::Consumed(vec![]);
        }
        if self.share_open() {
            return self.handle_share_key(key);
        }
        if key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL {
            if self.overlay_open() {
                let forward = self
                    .pending_agent_toggle
                    .is_some_and(|then| then.elapsed() <= AGENT_TOGGLE_WINDOW);
                self.modal = ModalState::None;
                self.pending_agent_toggle = None;
                return if forward {
                    ui_debug_log(
                        "agents_toggle_forward",
                        format_args!("window_ms={}", AGENT_TOGGLE_WINDOW.as_millis()),
                    );
                    KeyHandling::Forward
                } else {
                    ui_debug_log("agents_overlay_close", format_args!("reason=ctrl_a"));
                    KeyHandling::Consumed(vec![])
                };
            }
            self.modal = ModalState::Agents;
            self.pending_agent_toggle = Some(Instant::now());
            self.exit_chord_mode();
            ui_debug_log(
                "agents_overlay_open",
                format_args!("window_ms={}", AGENT_TOGGLE_WINDOW.as_millis()),
            );
            return KeyHandling::Consumed(vec![]);
        }
        if self.overlay_open() {
            self.set_agent_overlay_viewport(area);
            return self.handle_agent_overlay_key(key);
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.chord_mode != ChordMode::None
        {
            self.exit_chord_mode();
            return KeyHandling::Consumed(vec![]);
        }
        if matches!(self.chord_mode, ChordMode::None | ChordMode::Pane) && is_option_arrow(key) {
            self.touch_chord_activity();
            return KeyHandling::Consumed(self.move_focus(key.code, area).into_iter().collect());
        }
        if self.chord_mode == ChordMode::None {
            if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
                self.enter_chord_mode(ChordMode::Pane);
                return KeyHandling::Consumed(vec![]);
            }
            if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
                self.enter_chord_mode(ChordMode::Tab);
                return KeyHandling::Consumed(vec![]);
            }
            return KeyHandling::Forward;
        }

        let chord = self.chord_mode;
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.enter_chord_mode(ChordMode::Pane);
            return KeyHandling::Consumed(vec![]);
        }
        if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            self.enter_chord_mode(ChordMode::Tab);
            return KeyHandling::Consumed(vec![]);
        }
        let intent = match chord {
            ChordMode::Pane => self.handle_pane_chord(key, area),
            ChordMode::Tab => self.handle_tab_chord(key, area),
            ChordMode::None => None,
        };
        if is_chord_command(chord, key) {
            if is_chord_navigation(chord, key) {
                self.touch_chord_activity();
            } else {
                self.exit_chord_mode();
            }
            KeyHandling::Consumed(intent.into_iter().collect())
        } else {
            self.exit_chord_mode();
            KeyHandling::Forward
        }
    }

    pub(in crate::tui) fn handle_agent_overlay_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Esc => {
                self.modal = ModalState::None;
                self.pending_agent_toggle = None;
                ui_debug_log("agents_overlay_close", format_args!("reason=esc"));
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_agent_selection(false);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_agent_selection(true);
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter => {
                let Some(pane_id) = self.agent_selected_pane else {
                    return KeyHandling::Consumed(vec![]);
                };
                KeyHandling::Consumed(self.jump_to_agent_pane(pane_id, "enter"))
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    pub(in crate::tui) fn open_rename(&mut self, target: RenameTarget) {
        let value = match target {
            RenameTarget::Pane(pane_id) => self
                .snapshot
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.title.clone()),
            RenameTarget::Tab(tab_id) => self
                .snapshot
                .tabs
                .iter()
                .find(|tab| tab.tab_id == tab_id)
                .and_then(|tab| tab.title.clone()),
        }
        .unwrap_or_default();
        self.modal = ModalState::Rename(RenamePrompt {
            target,
            value,
            error: None,
        });
        self.exit_chord_mode();
        self.clear_selection();
        self.cancel_resize_drag();
    }

    pub(in crate::tui) fn handle_rename_key(&mut self, key: KeyEvent) -> KeyHandling {
        let ModalState::Rename(prompt) = &mut self.modal else {
            unreachable!("rename handler requires an active rename prompt");
        };
        match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                prompt.value.pop();
                prompt.error = None;
                KeyHandling::Consumed(vec![])
            }
            KeyCode::Enter if key.modifiers.is_empty() => match normalize_title(&prompt.value) {
                Ok(normalized) => {
                    let title = normalized.unwrap_or_default();
                    let intent = match prompt.target {
                        RenameTarget::Pane(pane_id) => UiIntent::RenamePane { pane_id, title },
                        RenameTarget::Tab(tab_id) => UiIntent::RenameTab { tab_id, title },
                    };
                    self.modal = ModalState::None;
                    KeyHandling::Consumed(vec![intent])
                }
                Err(_) => {
                    prompt.error = Some(String::from("Max 32 characters; no controls"));
                    KeyHandling::Consumed(vec![])
                }
            },
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                prompt.value.push(character);
                prompt.error = None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    /// Keys for the share modal.
    ///
    /// Enter takes the ticket because that is the only thing a peer on another machine can
    /// use; the code is the secondary key precisely because it only resolves on this Mac.
    /// Every key is consumed so no invite material leaks into the focused pane.
    pub(in crate::tui) fn handle_share_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Ticket);
            }
            KeyCode::Char('c') if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Code);
            }
            KeyCode::Esc if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
            }
            _ => {}
        }
        KeyHandling::Consumed(vec![])
    }

    pub(in crate::tui) fn confirm_delete_tab(&mut self, tab_id: TabId, pane_count: usize) {
        self.modal = ModalState::ConfirmDeleteTab { tab_id, pane_count };
        self.exit_chord_mode();
        self.clear_selection();
        self.cancel_resize_drag();
    }

    pub(in crate::tui) fn handle_confirm_delete_tab_key(&mut self, key: KeyEvent) -> KeyHandling {
        let tab_id = match &self.modal {
            ModalState::ConfirmDeleteTab { tab_id, .. } => *tab_id,
            _ => unreachable!("delete confirmation handler requires an active confirmation"),
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id }])
            }
            KeyCode::Esc | KeyCode::Char('n') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    pub(in crate::tui) fn handle_pane_chord(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_rename(RenameTarget::Pane(self.focused_pane));
                None
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
                self.create_pane(
                    if rect.width > rect.height {
                        Axis::LeftRight
                    } else {
                        Axis::TopBottom
                    },
                    NewPanePosition::Second,
                    area,
                )
            }
            KeyCode::Char('r') if key.modifiers.is_empty() => {
                self.create_pane(Axis::LeftRight, NewPanePosition::Second, area)
            }
            KeyCode::Char('l') if key.modifiers.is_empty() => {
                self.create_pane(Axis::LeftRight, NewPanePosition::First, area)
            }
            KeyCode::Char('d') if key.modifiers.is_empty() => {
                self.create_pane(Axis::TopBottom, NewPanePosition::Second, area)
            }
            KeyCode::Char('u') if key.modifiers.is_empty() => {
                self.create_pane(Axis::TopBottom, NewPanePosition::First, area)
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => Some(UiIntent::DeletePane {
                pane_id: self.focused_pane,
            }),
            KeyCode::Char('k') if key.modifiers.is_empty() => self
                .snapshot
                .panes
                .get(&self.focused_pane)
                .map(|pane| UiIntent::SetPaneLock {
                    pane_id: self.focused_pane,
                    locked: !pane.locked,
                }),
            // Shift+L, deliberately a different key from the lowercase `k` pane lock:
            // locking one pane and locking the front door are very different acts to
            // perform by accident.
            KeyCode::Char('L') => Some(UiIntent::SetSessionLock {
                locked: !self.session_locked,
            }),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.is_empty() =>
            {
                self.move_focus(key.code, area)
            }
            _ => None,
        }
    }

    pub(in crate::tui) fn create_pane(
        &self,
        axis: Axis,
        position: NewPanePosition,
        area: Rect,
    ) -> Option<UiIntent> {
        let rect = self.geometry(area).panes.get(&self.focused_pane).copied()?;
        let (grid_rows, grid_cols) = grid_for_pane(rect);
        Some(UiIntent::CreatePane {
            target_pane_id: self.focused_pane,
            axis,
            position,
            grid_rows,
            grid_cols,
        })
    }

    pub(in crate::tui) fn handle_tab_chord(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> Option<UiIntent> {
        match key.code {
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_rename(RenameTarget::Tab(self.current_tab));
                None
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let (grid_rows, grid_cols) = grid_for_pane(self.geometry(area).content);
                Some(UiIntent::CreateTab {
                    grid_rows,
                    grid_cols,
                })
            }
            KeyCode::Char('x') if key.modifiers.is_empty() => {
                let tab = self
                    .snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == self.current_tab)?;
                let pane_count = visible_leaf_panes(&tab.root).len();
                if pane_count > 1 {
                    self.confirm_delete_tab(tab.tab_id, pane_count);
                    None
                } else {
                    Some(UiIntent::DeleteTab { tab_id: tab.tab_id })
                }
            }
            KeyCode::Left if key.modifiers.is_empty() => self.switch_tab(false),
            KeyCode::Right if key.modifiers.is_empty() => self.switch_tab(true),
            _ => None,
        }
    }

    pub(in crate::tui) fn move_focus(
        &mut self,
        direction: KeyCode,
        area: Rect,
    ) -> Option<UiIntent> {
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
        self.select_pane(self.current_tab, pane_id, "key");
        Some(UiIntent::FocusPane { pane_id })
    }

    pub(in crate::tui) fn switch_tab(&mut self, forward: bool) -> Option<UiIntent> {
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
}
