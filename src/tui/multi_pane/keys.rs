//! What a keystroke does: quit, chord entry, chord commands, modal input,
//! and everything that falls through to the focused PTY.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::{
    layout::{Axis, NewPanePosition},
    tui::{
        ChordMode, HOME_TOGGLE_WINDOW, KeyHandling, ModalState, MultiPaneTui, QuitAction,
        RenameTarget, UiIntent,
        geometry::{grid_for_pane, nearest_in_direction, visible_leaf_panes},
        input::keys::{
            ends_chord_mode, is_chord_command, is_chord_navigation, is_focus_arrow, is_quit,
        },
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
        // Ctrl+Q outranks every other modal: it is the way out of a dialog you
        // opened by accident as much as the way out of the session.
        if is_quit(key) {
            self.pending_home_toggle = None;
            self.exit_chord_mode();
            // A second Ctrl+Q backs out, the way a second Ctrl+S closes the
            // share panel. Answering "quit?" with the quit key should not quit.
            if self.quit_open() {
                self.modal = ModalState::None;
                return KeyHandling::Consumed(vec![]);
            }
            if !self.detachable {
                self.modal = ModalState::None;
                return KeyHandling::Quit(QuitAction::Detach);
            }
            return self.open_quit_prompt();
        }
        if matches!(self.modal, ModalState::Quit) {
            return self.handle_quit_key(key);
        }
        // Above the rest, because it is the only modal with another machine
        // waiting on the answer and a clock running out on it.
        if self.remote_work_open() {
            return self.handle_remote_work_key(key);
        }
        if matches!(self.modal, ModalState::Rename(_)) {
            return self.handle_rename_key(key);
        }
        if matches!(self.modal, ModalState::ConfirmDeleteTab { .. }) {
            return self.handle_confirm_delete_tab_key(key);
        }
        // Above Ctrl+O, so the key that opens the inbox does not walk out from
        // under a panel that is waiting on another machine to answer.
        if self.add_machine_open() {
            return self.handle_add_machine_key(key);
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
            self.pending_home_toggle = None;
            return KeyHandling::Consumed(vec![]);
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
        // Ctrl+A is the legacy way in, kept for the muscle memory the old
        // agents overlay built. It carries the doubled-press escape hatch that
        // screen and tmux users expect: a second Ctrl+A inside the window
        // closes Home and sends a literal one to the pane. Ctrl+O -- the
        // binding this build teaches -- has no such history and no such window.
        if key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL {
            if self.home_open {
                let forward = self
                    .pending_home_toggle
                    .is_some_and(|then| then.elapsed() <= HOME_TOGGLE_WINDOW);
                self.set_home_open(false, if forward { "double_ctrl_a" } else { "ctrl_a" });
                self.pending_home_toggle = None;
                return if forward {
                    KeyHandling::Forward
                } else {
                    KeyHandling::Consumed(vec![])
                };
            }
            self.set_home_open(true, "ctrl_a");
            self.clear_zoom();
            self.pending_home_toggle = Some(Instant::now());
            return KeyHandling::Consumed(vec![]);
        }
        // Everything below belongs to the panes, so Home claims the rest of the
        // keyboard before it gets there. Reached after the bindings above so
        // that the two ways out of Home still work from on it.
        if self.home_open {
            return self.handle_home_key(key, area);
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.chord_mode != ChordMode::None
        {
            self.exit_chord_mode();
            return KeyHandling::Consumed(vec![]);
        }
        if matches!(self.chord_mode, ChordMode::None | ChordMode::Pane) && is_focus_arrow(key) {
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
            // Typing leaves the mode; a modified key just passes through it.
            // See [`ends_chord_mode`].
            if ends_chord_mode(key) {
                self.exit_chord_mode();
            }
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
            // `z`, the letter tmux taught a decade of users. Local to this
            // client, so it never reaches the layout and no peer sees it.
            KeyCode::Char('z') if key.modifiers.is_empty() => {
                self.toggle_zoom();
                None
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
            // Shift+R rather than `r`, which has split-to-the-right. The
            // capital is not a hierarchy, it is what was free next to the
            // letter tmux users will reach for first.
            KeyCode::Char('R') => {
                self.repaint_requested = true;
                None
            }
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

    /// Moving focus is a decision to look somewhere else, and a zoom exists to
    /// hide somewhere else — so the zoom stands down first.
    ///
    /// Without this the arrows are silently inert while a pane is zoomed: the
    /// zoomed geometry holds exactly one pane, so there is never anything to
    /// move to. Put back if the move had nowhere to go, so a right arrow at the
    /// right-hand edge does not quietly unzoom instead of doing nothing.
    pub(in crate::tui) fn move_focus(
        &mut self,
        direction: KeyCode,
        area: Rect,
    ) -> Option<UiIntent> {
        let zoomed = self.zoomed_pane.take();
        let moved = self.move_focus_within_layout(direction, area);
        if moved.is_none() {
            self.zoomed_pane = zoomed;
        }
        moved
    }

    fn move_focus_within_layout(&mut self, direction: KeyCode, area: Rect) -> Option<UiIntent> {
        let geometry = self.geometry(area);
        let source = *geometry.panes.get(&self.focused_pane)?;
        let candidates = geometry
            .panes
            .iter()
            .filter(|(pane_id, _)| **pane_id != self.focused_pane)
            .map(|(pane_id, rect)| (*pane_id, *rect))
            .collect::<Vec<_>>();
        let pane_id = nearest_in_direction(source, candidates, direction)?;
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
        // Home sits left of Tab #1 in the bar, so stepping left off the first
        // tab lands on it rather than wrapping to the last. A second path in,
        // for people who navigate by tab rather than by keybinding.
        if !forward && index == 0 {
            self.set_home_open(true, "tab_left");
            self.clear_zoom();
            return None;
        }
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
            ChordMode, KeyHandling, MultiPaneTui, QuitAction, UiIntent,
            input::keys::{
                ESC_PREFIX_WINDOW, PendingEscape, is_chord_command, is_chord_navigation,
            },
            test_support::{layout, split_layout},
        },
    };

    use super::CHORD_IDLE_TIMEOUT;

    /// Issue #120: the way out of a screen ratatui has stopped repainting.
    ///
    /// Everything else in this program answers "what should the frame contain".
    /// This one answers "throw away what you believe is on the screen", which
    /// nothing else here can ask for -- and until it existed, the only thing
    /// that did was resizing the terminal window, which people found by
    /// accident.
    #[test]
    fn the_redraw_chord_asks_for_the_screen_to_be_painted_again() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert!(!tui.take_repaint_request(), "nothing has asked yet");

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            ),
            KeyHandling::Consumed(vec![])
        );
        // Shift is how a capital is typed, not a modifier the user added.
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), area,),
            KeyHandling::Consumed(vec![]),
            "the chord is consumed here and never forwarded to the pane"
        );
        assert_eq!(
            tui.chord_mode(),
            ChordMode::None,
            "a command ends the chord, like every other one"
        );

        assert!(
            tui.take_repaint_request(),
            "the run loop is told to repaint"
        );
        assert!(
            !tui.take_repaint_request(),
            "and told once: a repaint every frame would be every frame drawn in full"
        );
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

    /// Issue #106: Ctrl+arrow, because that is what every other tiling thing
    /// on a desktop uses and Option+Shift+arrow is a lot of hand.
    #[test]
    fn control_arrows_move_focus_and_option_arrows_still_do() {
        let area = Rect::new(0, 0, 80, 24);
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
            assert_eq!(
                tui.handle_key(KeyEvent::new(KeyCode::Right, modifiers), area),
                KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }]),
                "modifiers: {modifiers:?}"
            );
            assert_eq!(tui.focused_pane(), 2, "modifiers: {modifiers:?}");
        }
    }

    /// The escape hatch the new binding owes the shell.
    ///
    /// Ctrl+arrow is word-jump in readline, and inside a pane it now stops at
    /// p2pmux. Holding Alt as well is how you still send it, so it must not be
    /// a focus key -- and must not move focus on its way past either.
    #[test]
    fn control_alt_arrows_are_forwarded_to_the_pane_untouched() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down] {
            assert_eq!(
                tui.handle_key(
                    KeyEvent::new(code, KeyModifiers::ALT | KeyModifiers::CONTROL),
                    area,
                ),
                KeyHandling::Forward,
                "{code:?} with both modifiers belongs to the shell"
            );
            assert_eq!(tui.focused_pane(), 1, "{code:?} must not move focus either");
        }
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
            tui.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
    }

    /// Ctrl+F is not a binding here — it reaches the shell, where it is `forward-char`
    /// and prints nothing. It used to end pane mode on the way past, which is what
    /// "Ctrl+F exits a pane" was: no pane went anywhere, but the mode the user was
    /// standing in did, for a keystroke never aimed at the mux.
    #[test]
    fn a_modified_key_the_chord_does_not_claim_reaches_the_pane_without_taking_the_mode() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        for modified in [
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
        ] {
            let _ = tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
            );
            assert_eq!(tui.chord_mode(), ChordMode::Pane);

            assert_eq!(tui.handle_key(modified, area), KeyHandling::Forward);
            assert_eq!(
                tui.chord_mode(),
                ChordMode::Pane,
                "{modified:?} is not somebody typing their way out of the mode"
            );
        }

        // The mode still clears itself; it just does so on the idle timeout
        // rather than under a keystroke the user aimed at their shell.
        let now = Instant::now();
        tui.chord_last_activity = now.checked_sub(CHORD_IDLE_TIMEOUT);
        assert!(tui.expire_chord_mode(now));
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
                tui.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE), area),
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

        // Pane 1 is the full-height left column, so its top edge is the top of
        // the tab and there is nothing above it. This used to move focus to
        // pane 2 -- up *and to the right* -- because pane 2 is half height and
        // so its centre sits higher. That is issue #106's "the arrows don't
        // work quite well": focus leaving sideways when you press Up, and not
        // coming back when you press Down.
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![]),
            "nothing is above the full-height pane, so Up stays put"
        );
        assert_eq!(tui.focused_pane(), 1);

        // The right-hand column is still two keys away, and by a route that
        // reads the way the screen looks.
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), area),
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
            KeyHandling::Quit(QuitAction::Detach)
        );
    }

    /// The zoom state Home already used to hand you into an agent, reachable
    /// on purpose rather than only as a side effect of arriving from the inbox.
    #[test]
    fn ctrl_p_z_gives_the_focused_pane_the_whole_screen_and_gives_it_back() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);

        assert_eq!(tui.geometry(area).panes.len(), 3);

        let _ = tui.handle_key(ctrl_p, area);
        assert_eq!(tui.handle_key(z, area), KeyHandling::Consumed(vec![]));
        assert_eq!(tui.zoomed_pane(), Some(tui.focused_pane()));
        assert_eq!(
            tui.geometry(area).panes.len(),
            1,
            "the siblings stop sharing the screen"
        );
        assert_eq!(
            tui.geometry(area).panes[&tui.focused_pane()],
            tui.geometry(area).content,
            "and the zoomed pane gets the whole content area"
        );

        let _ = tui.handle_key(ctrl_p, area);
        assert_eq!(tui.handle_key(z, area), KeyHandling::Consumed(vec![]));
        assert_eq!(tui.zoomed_pane(), None);
        assert_eq!(tui.geometry(area).panes.len(), 3);
    }

    /// Nothing is hidden on a tab with one pane, so lighting a `zoom` badge
    /// there would describe a change that did not happen.
    #[test]
    fn z_does_nothing_on_a_tab_that_is_already_one_pane() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 4, 10)],
        ))
        .expect("valid layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area);
        assert_eq!(tui.zoomed_pane(), None);
    }

    /// A zoomed geometry holds one pane, so without this the arrows are
    /// silently inert while a zoom is up.
    #[test]
    fn moving_focus_stands_the_zoom_down_but_only_when_it_has_somewhere_to_go() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        tui.select_pane(1, 1, "test");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area);
        assert_eq!(tui.zoomed_pane(), Some(1));

        // Pane 1 is the left half; nothing lies further left.
        assert_eq!(tui.move_focus(KeyCode::Left, area), None);
        assert_eq!(
            tui.zoomed_pane(),
            Some(1),
            "an arrow with nowhere to go must not quietly unzoom"
        );

        assert_eq!(
            tui.move_focus(KeyCode::Right, area),
            Some(UiIntent::FocusPane { pane_id: 2 })
        );
        assert_eq!(tui.zoomed_pane(), None);
        assert_eq!(tui.geometry(area).panes.len(), 3);
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
