//! What a keystroke does: quit, chord entry, chord commands, modal input,
//! and everything that falls through to the focused PTY.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::{
    layout::{Axis, NewPanePosition},
    tui::{
        AGENT_TOGGLE_WINDOW, ChordMode, KeyHandling, ModalState, MultiPaneTui, RenameTarget,
        UiIntent,
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
        // Home, from anywhere — including from inside a live pane, which is the
        // only reason it is claimed this early. `Ctrl+O` is free in shells and
        // transmits in every terminal. `Esc` deliberately is not the way back:
        // Claude Code interrupts on it and vim needs it constantly, so
        // swallowing it would break the pane the user just entered. `Ctrl+H` is
        // backspace and `Ctrl+0` does not transmit in most terminals.
        if key.code == KeyCode::Char('o') && key.modifiers == KeyModifiers::CONTROL {
            let open = !self.home_open;
            self.set_home_open(open, "ctrl_o");
            if open {
                self.clear_zoom();
            }
            return KeyHandling::Consumed(vec![]);
        }
        if self.home_open {
            return self.handle_home_key(key, area);
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use crate::{
        layout::{Axis, NewPanePosition, Node, Tab},
        tui::{
            ChordMode, KeyHandling, MultiPaneTui, UiIntent,
            input::keys::{
                ESC_PREFIX_WINDOW, PendingEscape, is_chord_command, is_chord_navigation,
            },
            test_support::{layout, split_layout},
        },
    };

    use super::CHORD_IDLE_TIMEOUT;

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
                position: NewPanePosition::Second,
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
    fn option_arrows_focus_in_normal_and_pane_modes_without_forwarding_at_edges() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), area),
            KeyHandling::Consumed(vec![]),
            "an edge Option-arrow is still consumed"
        );

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 3 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
    }

    #[test]
    fn option_arrows_accept_extra_modifiers_but_not_control() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Right, KeyModifiers::ALT | KeyModifiers::SHIFT),
                area,
            ),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Forward
        );
        assert_eq!(tui.focused_pane(), 2);
    }

    #[test]
    fn esc_then_arrow_uses_option_focus_and_expired_esc_keeps_its_prior_behavior() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let now = Instant::now();
        let mut pending_escape = PendingEscape::default();

        pending_escape.start(now);
        let option_arrow = pending_escape
            .take_option_arrow(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .expect("Esc followed by an arrow becomes Option-arrow focus");
        assert_eq!(
            tui.handle_key(option_arrow, area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        pending_escape.start(now);
        assert!(pending_escape.take_if_expired(now + ESC_PREFIX_WINDOW));
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn pane_chord_directional_splits_select_axis_and_child_position() {
        let area = Rect::new(0, 0, 80, 24);
        for (key, axis, position) in [
            ('r', Axis::LeftRight, NewPanePosition::Second),
            ('l', Axis::LeftRight, NewPanePosition::First),
            ('d', Axis::TopBottom, NewPanePosition::Second),
            ('u', Axis::TopBottom, NewPanePosition::First),
        ] {
            let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
            let _ = tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            );
            assert_eq!(
                tui.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE), area),
                KeyHandling::Consumed(vec![UiIntent::CreatePane {
                    target_pane_id: 1,
                    axis,
                    position,
                    grid_rows: 20,
                    grid_cols: 38,
                }]),
                "key: {key}"
            );
            assert_eq!(tui.focused_pane(), 1, "key: {key}");
            assert_eq!(tui.chord_mode(), ChordMode::None, "key: {key}");
        }
    }

    #[test]
    fn pane_lock_chord_toggles_lock_and_exits_mode() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::SetPaneLock {
                pane_id: 1,
                locked: true,
            }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
        ));
        assert!(!is_chord_navigation(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn pane_commands_exit_mode_even_when_no_intent_is_available() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        tui.snapshot.panes.clear();

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
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
    fn sticky_chord_mode_expires_two_seconds_after_last_key() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let now = Instant::now();
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);
        tui.chord_last_activity = now.checked_sub(CHORD_IDLE_TIMEOUT);
        assert!(tui.expire_chord_mode(now));
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(tui.chord_last_activity.is_none());

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        tui.chord_last_activity = now.checked_sub(Duration::from_millis(1_999));
        assert!(!tui.expire_chord_mode(now));
        assert_eq!(tui.chord_mode(), ChordMode::Tab);
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area);
        tui.chord_last_activity = now.checked_sub(CHORD_IDLE_TIMEOUT);
        assert!(tui.expire_chord_mode(now));
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

    /// Ctrl+F is not a binding here — it reaches the shell, where it is `forward-char`
    /// and prints nothing. That makes it the key that exposes a repaint gap: the pane
    /// sends back no output for the client to redraw on, so if the client does not
    /// notice the mode ending on its own, the footer keeps advertising PANE MODE long
    /// after the mode is gone.
    #[test]
    fn a_modified_key_the_chord_does_not_claim_still_ends_the_mode() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(tui.chord_mode(), ChordMode::Pane);

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
                area
            ),
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
    fn square_pane_create_chord_splits_top_to_bottom_without_a_tab_bar_gap() {
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },

                title: None,
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
                position: NewPanePosition::Second,
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

                title: None,
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
                position: NewPanePosition::Second,
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
            tui.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 1 }])
        );
        assert_eq!(tui.focused_pane(), 1);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert_eq!(tui.focused_pane(), 2);

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
        let _ = edge_tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = edge_tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), area);
        assert_eq!(edge_tui.focused_pane(), 3);
        for key in [KeyCode::Down, KeyCode::Right] {
            let _ = edge_tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            );
            assert_eq!(
                edge_tui.handle_key(KeyEvent::new(key, KeyModifiers::NONE), area),
                KeyHandling::Consumed(vec![])
            );
            assert_eq!(edge_tui.focused_pane(), 3);
        }
    }

    #[test]
    fn tab_chord_switches_and_creates_or_deletes_tabs_without_forwarding_keys() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
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
        assert_eq!(tui.chord_mode(), ChordMode::None);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 2 }])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    #[test]
    fn sticky_tab_mode_switches_tabs_repeatedly() {
        let mut tui = MultiPaneTui::new(layout(
            vec![
                Tab {
                    tab_id: 1,
                    root: Node::Leaf { pane_id: 1 },

                    title: None,
                },
                Tab {
                    tab_id: 2,
                    root: Node::Leaf { pane_id: 2 },

                    title: None,
                },
                Tab {
                    tab_id: 3,
                    root: Node::Leaf { pane_id: 3 },

                    title: None,
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
    fn shift_l_toggles_the_session_lock_from_whatever_state_it_is_in() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT), area),
            KeyHandling::Consumed(vec![UiIntent::SetSessionLock { locked: true }]),
            "Shift+L should offer to lock a session that is currently open"
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);

        tui.set_session_locked(true);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT), area),
            KeyHandling::Consumed(vec![UiIntent::SetSessionLock { locked: false }]),
            "and to unlock one that is locked"
        );
    }
}
