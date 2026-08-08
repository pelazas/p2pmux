//! Key handling while a modal owns the keyboard: the rename prompt, the share
//! panel, and the delete-tab confirmation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    layout::{TabId, normalize_title},
    tui::{KeyHandling, ModalState, MultiPaneTui, RenamePrompt, RenameTarget, ShareCopy, UiIntent},
};

impl MultiPaneTui {
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
    /// Enter and `c` take the code, `t` takes the ticket; either way the clipboard gets a
    /// runnable `p2pmux join` line rather than the bare invite. The client resolves Enter to
    /// the ticket when there is no code, so the primary key always copies something usable
    /// rather than reporting nothing to copy. Every key is consumed so no invite material
    /// leaks into the focused pane.
    pub(in crate::tui) fn handle_share_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Enter | KeyCode::Char('c') if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Code);
            }
            KeyCode::Char('t') if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Ticket);
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
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    use crate::{
        layout::{Node, Tab},
        tui::{
            ChordMode, HOME_TOGGLE_WINDOW, KeyHandling, MouseHandling, MultiPaneTui,
            PaneMouseProtocol, ShareCopy, UiIntent,
            render::panes::render_multi_pane,
            test_support::{agent_row, layout, split_layout},
        },
    };

    /// Ctrl+A is the legacy way to Home, and it keeps the doubled-press escape
    /// hatch a decade of screen and tmux users expect: a second press inside
    /// the window sends a literal Ctrl+A to the pane instead.
    #[test]
    pub(in crate::tui) fn ctrl_a_opens_home_and_a_doubled_press_forwards_it() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);

        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
        assert!(tui.home_open());
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Forward);
        assert!(!tui.home_open());

        // Outside the window the second press is simply a second press: it
        // closes Home and reaches no pane.
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
        assert!(tui.home_open());
        tui.pending_home_toggle = Some(Instant::now() - HOME_TOGGLE_WINDOW);
        assert!(!tui.expire_home_toggle(Instant::now()));
        assert!(tui.home_open());
        assert_eq!(tui.pending_home_toggle, None);
        assert_eq!(tui.handle_key(ctrl_a, area), KeyHandling::Consumed(vec![]));
        assert!(!tui.home_open());
    }

    /// Ctrl+O has no legacy to honour, so it never forwards: two presses are
    /// open then close, and nothing reaches the pane either time.
    #[test]
    pub(in crate::tui) fn ctrl_o_never_forwards_however_fast_it_is_pressed() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

        assert_eq!(tui.handle_key(ctrl_o, area), KeyHandling::Consumed(vec![]));
        assert_eq!(tui.handle_key(ctrl_o, area), KeyHandling::Consumed(vec![]));
        assert!(!tui.home_open());
    }

    #[test]
    pub(in crate::tui) fn home_claims_the_keyboard_and_enter_jumps_to_another_tab() {
        let area = Rect::new(0, 0, 80, 24);
        let snapshot = layout(
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
            &[(1, 2, 8), (2, 2, 8)],
        );
        let mut tui = MultiPaneTui::new(snapshot).unwrap();
        tui.set_agent_rows(vec![agent_row(2, 2, 1)]);
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        tui.handle_key(ctrl_a, area);

        // An unclaimed key is swallowed rather than forwarded: there is no pane
        // in view to forward it to.
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::FocusPane { pane_id: 2 }])
        );
        assert!(!tui.home_open());
        assert_eq!(tui.current_tab(), 2);
        assert_eq!(tui.focused_pane(), 2);
    }

    #[test]
    pub(in crate::tui) fn rename_prompt_captures_chord_target_edits_and_consumes_modal_input() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        tui.snapshot.panes.get_mut(&1).unwrap().title = Some(String::from("old"));

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(matches!(
            &tui.modal,
            super::ModalState::Rename(prompt) if prompt.value == "old"
        ));
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('🙂'), KeyModifiers::SHIFT),
                area
            ),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::RenamePane {
                pane_id: 1,
                title: String::from("old"),
            }])
        );
        assert!(!tui.modal_open());
    }

    #[test]
    pub(in crate::tui) fn rename_prompt_rejects_invalid_input_and_ctrl_q_wins() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .unwrap();
        tui.open_rename(super::RenameTarget::Tab(1));
        for character in "x".repeat(33).chars() {
            tui.handle_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                area,
            );
        }
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(matches!(tui.modal, super::ModalState::Rename(_)));
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Quit
        );
        assert!(matches!(tui.modal, super::ModalState::None));
    }

    #[test]
    pub(in crate::tui) fn multi_pane_tab_delete_requires_confirmation_and_blocks_pane_input() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert_eq!(tui.chord_mode(), ChordMode::None);
        assert!(tui.modal_open());
        assert!(matches!(
            &tui.modal,
            super::ModalState::ConfirmDeleteTab {
                tab_id: 1,
                pane_count: 3
            }
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render_multi_pane(frame, &tui, &BTreeMap::new()))
            .expect("render");
        let mut rendered = String::new();
        for row in 0..24 {
            for column in 0..80 {
                rendered.push_str(terminal.backend().buffer()[(column, row)].symbol());
            }
        }
        assert!(rendered.contains("Delete tab?"));
        assert!(rendered.contains("3 panes"));
        assert!(rendered.contains("Enter/y yes · Esc/n no"));

        let mouse = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 60,
            row: 17,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            tui.handle_mouse(mouse, area, PaneMouseProtocol::default()),
            MouseHandling::default()
        );
        assert_eq!(tui.focused_pane(), 1);
        assert!(!tui.scroll_mouse_pane(60, 17, area, 10, true));
        assert!(tui.resize_drag.is_none());
        assert!(tui.selection.is_none());
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.modal_open());

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(!tui.modal_open());

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 1 }])
        );
        assert!(!tui.modal_open());
    }

    #[test]
    pub(in crate::tui) fn single_pane_tab_delete_is_immediate() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(layout(
            vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
                title: None,
            }],
            &[(1, 2, 8)],
        ))
        .expect("layout");

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![UiIntent::DeleteTab { tab_id: 1 }])
        );
        assert!(!tui.modal_open());
    }

    #[test]
    pub(in crate::tui) fn ctrl_s_toggles_the_share_modal_and_leaves_any_chord_mode() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            area,
        );
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Consumed(vec![]),
            "share is a mux command and never reaches the PTY"
        );
        assert!(tui.share_open());
        assert_eq!(tui.chord_mode(), ChordMode::None);

        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            area,
        );
        assert!(!tui.share_open(), "Ctrl+S closes what it opened");
    }

    #[test]
    pub(in crate::tui) fn share_modal_claims_each_copy_once_and_closes_on_escape() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);
        let _ = tui.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            area,
        );

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area);
        assert_eq!(tui.take_share_copy_request(), Some(ShareCopy::Code));
        assert_eq!(
            tui.take_share_copy_request(),
            None,
            "a claimed request must not copy again on the next key"
        );

        // `c` is muscle memory for copy and must not fall through to the pane.
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), area);
        assert_eq!(tui.take_share_copy_request(), Some(ShareCopy::Code));

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), area);
        assert_eq!(tui.take_share_copy_request(), Some(ShareCopy::Ticket));

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area);
        assert!(!tui.share_open());
    }

    #[test]
    pub(in crate::tui) fn plain_share_key_reaches_the_pty_without_control() {
        let mut tui = MultiPaneTui::new(split_layout()).expect("valid layout");
        let area = Rect::new(0, 0, 80, 24);

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), area),
            KeyHandling::Forward
        );
        assert!(!tui.share_open());
    }
}
