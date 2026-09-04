//! Key handling while a modal owns the keyboard: the rename prompt, the share
//! panel, the delete-tab confirmation, the quit prompt, and the inbox update
//! confirm.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::{
    layout::{TabId, normalize_title},
    tui::{
        AddMachinePrompt, KeyHandling, ModalState, MultiPaneTui, QuitAction, RenamePrompt,
        RenameTarget, ShareCopy, UiIntent,
    },
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

    /// `a` on the inbox: put up the line the other machine has to run.
    ///
    /// The machines already here are remembered so the panel can say which one
    /// arrived, and the client is asked to record the session's ticket — that
    /// write is what makes bare `p2pmux` rejoin on both machines afterwards,
    /// and what lets the node remember the newcomer once it appears.
    pub(in crate::tui) fn open_add_machine(&mut self) {
        self.modal = ModalState::AddMachine(AddMachinePrompt {
            known: crate::tui::home::machine_rows(self)
                .into_iter()
                .map(|machine| machine.name)
                .collect(),
        });
        self.pending_pair_offer = true;
        self.exit_chord_mode();
    }

    pub fn add_machine_open(&self) -> bool {
        matches!(self.modal, ModalState::AddMachine(_))
    }

    /// Put the question to whoever is at this machine.
    ///
    /// It replaces whatever else was on screen, because it is about something
    /// that is about to happen on this box and it stops happening if it is not
    /// answered. Nothing here grants anything: a keystroke on this machine is
    /// the only way out other than expiry.
    pub fn ask_remote_work(&mut self, command: &[String]) {
        self.modal = ModalState::ConfirmRemoteWork {
            command: command.to_vec(),
        };
        self.exit_chord_mode();
    }

    pub fn remote_work_open(&self) -> bool {
        matches!(self.modal, ModalState::ConfirmRemoteWork { .. })
    }

    /// Take the question down without answering it.
    ///
    /// For the client, when the node says the request is no longer live. It
    /// deliberately records no answer: nothing was granted, and nothing was
    /// refused by this machine's owner — the clock did it.
    pub fn close_remote_work(&mut self) {
        if self.remote_work_open() {
            self.modal = ModalState::None;
        }
    }

    /// What the held request would run, for the panel to show.
    pub(in crate::tui) fn remote_work_command(&self) -> Option<&[String]> {
        match &self.modal {
            ModalState::ConfirmRemoteWork { command } => Some(command),
            _ => None,
        }
    }

    /// `y` allows this one terminal, `n` and `Esc` refuse it. Nothing else:
    /// the machine that asked is waiting, and a third option would be a fourth
    /// thing to read under time pressure.
    ///
    /// The answer leaves as an intent rather than as a flag, so it travels the
    /// one route that already exists from a client to the node holding the
    /// request — and so a foreground session, where the two are one process,
    /// takes exactly the same path.
    pub(in crate::tui) fn handle_remote_work_key(&mut self, key: KeyEvent) -> KeyHandling {
        let approved = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => true,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => false,
            _ => return KeyHandling::Consumed(vec![]),
        };
        self.modal = ModalState::None;
        KeyHandling::Consumed(vec![UiIntent::AnswerRemoteWork { approved }])
    }

    /// The machine that has joined since the panel went up, if one has.
    ///
    /// Read from the member list rather than from the pairing file: a machine
    /// that has joined is in the session, and that is the fact the user is
    /// waiting on. The pairing record catches up a moment later, out of process.
    pub(in crate::tui) fn add_machine_joined(&self) -> Option<String> {
        let ModalState::AddMachine(prompt) = &self.modal else {
            return None;
        };
        crate::tui::home::machine_rows(self)
            .into_iter()
            .map(|machine| machine.name)
            .find(|name| !prompt.known.contains(name))
    }

    /// Takes the request to record the session's ticket in the pairing file.
    ///
    /// The TUI never touches the filesystem — the attaching process owns it,
    /// exactly as it owns the clipboard for [`Self::take_share_copy_request`].
    pub fn take_pair_offer(&mut self) -> bool {
        std::mem::take(&mut self.pending_pair_offer)
    }

    /// `c` copies the line, `Esc` closes. Nothing else: the panel is waiting on
    /// another machine, and there is no third thing to do while it does.
    pub(in crate::tui) fn handle_add_machine_key(&mut self, key: KeyEvent) -> KeyHandling {
        match key.code {
            KeyCode::Enter | KeyCode::Char('c') if key.modifiers.is_empty() => {
                self.pending_share_copy = Some(ShareCopy::Pair);
            }
            KeyCode::Esc | KeyCode::Char('a') if key.modifiers.is_empty() => {
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

    /// The inbox update line: ask before running the command that replaces this
    /// binary.
    ///
    /// No-op when there is no notice. A key that opens a dialog about a line
    /// that is not on screen would be a dialog about nothing.
    pub(in crate::tui) fn open_update_confirm(&mut self) {
        if self.update_notice.is_none() {
            return;
        }
        self.modal = ModalState::ConfirmUpdate;
        self.exit_chord_mode();
    }

    pub fn update_confirm_open(&self) -> bool {
        matches!(self.modal, ModalState::ConfirmUpdate)
    }

    /// Enter runs it in a new terminal on this machine. Esc backs out.
    ///
    /// The command is the one the standing line already named. Running it here
    /// rather than on the selected fleet machine is the point: this is the
    /// copy of p2pmux the person is sitting in.
    pub(in crate::tui) fn handle_confirm_update_key(
        &mut self,
        key: KeyEvent,
        area: Rect,
    ) -> KeyHandling {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') if key.modifiers.is_empty() => {
                KeyHandling::Consumed(self.run_inbox_update(area))
            }
            KeyCode::Esc | KeyCode::Char('n') if key.modifiers.is_empty() => {
                self.modal = ModalState::None;
                KeyHandling::Consumed(vec![])
            }
            _ => KeyHandling::Consumed(vec![]),
        }
    }

    pub(in crate::tui) fn open_quit_prompt(&mut self) -> KeyHandling {
        self.modal = ModalState::Quit;
        self.clear_selection();
        self.cancel_resize_drag();
        KeyHandling::Consumed(vec![])
    }

    /// `d` leaves, `k` ends it, anything else backs out.
    ///
    /// Enter is detach rather than the other one, and there is no `y`: this is
    /// not a yes/no question, and a prompt where the reflex answer destroys
    /// work is a prompt that has failed at the only job it has. The two letters
    /// are the initials of the two words on screen, so neither is a guess.
    pub(in crate::tui) fn handle_quit_key(&mut self, key: KeyEvent) -> KeyHandling {
        if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
            return KeyHandling::Consumed(vec![]);
        }
        match key.code {
            KeyCode::Char('d' | 'D') | KeyCode::Enter => {
                self.modal = ModalState::None;
                KeyHandling::Quit(QuitAction::Detach)
            }
            KeyCode::Char('k' | 'K') => {
                self.modal = ModalState::None;
                KeyHandling::Quit(QuitAction::Kill)
            }
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
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
            PaneMouseProtocol, QuitAction, ShareCopy, UiIntent,
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
            KeyHandling::Quit(QuitAction::Detach)
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

    /// Detaching and ending a session were one keystroke and no question.
    #[test]
    pub(in crate::tui) fn ctrl_q_asks_which_leaving_was_meant_and_defaults_to_the_reversible_one() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");
        tui.set_detachable(true);
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);

        assert_eq!(tui.handle_key(ctrl_q, area), KeyHandling::Consumed(vec![]));
        assert!(tui.quit_open());
        assert!(
            tui.modal_open(),
            "the prompt owns the keyboard while it is up"
        );

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
        assert!(rendered.contains("Leave this session?"), "{rendered:?}");
        assert!(rendered.contains("detach — leave it running"));
        assert!(rendered.contains("kill — end it, panes and all"));

        // A key the prompt does not claim leaves it up rather than falling
        // through to a pane the user cannot see.
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.quit_open());

        // Answering "quit?" with the quit key backs out, the way a second
        // Ctrl+S closes the share panel.
        assert_eq!(tui.handle_key(ctrl_q, area), KeyHandling::Consumed(vec![]));
        assert!(!tui.modal_open());

        // Esc cancels too.
        let _ = tui.handle_key(ctrl_q, area);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(!tui.modal_open());

        // Enter is the reversible answer: a reflex press must not end a session.
        let _ = tui.handle_key(ctrl_q, area);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area),
            KeyHandling::Quit(QuitAction::Detach)
        );
        assert!(!tui.modal_open());

        let _ = tui.handle_key(ctrl_q, area);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), area),
            KeyHandling::Quit(QuitAction::Detach)
        );

        let _ = tui.handle_key(ctrl_q, area);
        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), area),
            KeyHandling::Quit(QuitAction::Kill)
        );
        assert!(!tui.modal_open());
    }

    /// A foreground session owns its panes outright, so there is no second
    /// answer to offer and no question worth asking.
    #[test]
    pub(in crate::tui) fn ctrl_q_still_leaves_at_once_where_nothing_outlives_the_client() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tui = MultiPaneTui::new(split_layout()).expect("layout");

        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Quit(QuitAction::Detach)
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

    /// Adding a machine is two halves — the line to run there, and finding out
    /// whether it worked — and the panel holds both. The second half is the one
    /// that used to mean going somewhere else.
    #[test]
    pub(in crate::tui) fn the_add_machine_panel_shows_the_line_then_reports_the_join() {
        let area = Rect::new(0, 0, 100, 24);
        let mut tui = crate::tui::test_support::home_tui(&[(
            "laptop",
            "claude",
            crate::protocol::AgentRosterState::Working,
        )]);
        tui.snapshot.members[0].display_name = String::from("laptop");
        tui.set_home_open(true, "test");

        assert_eq!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), area),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.add_machine_open());
        assert!(
            tui.take_pair_offer(),
            "the session's ticket has to be recorded, or nothing that joins is remembered"
        );
        assert!(!tui.take_pair_offer(), "a claimed request is claimed once");
        assert_eq!(tui.add_machine_joined(), None);

        // `c` copies the pair line rather than the join line: the same code,
        // but what the machine at the other end does with it is not the same.
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), area);
        assert_eq!(tui.take_share_copy_request(), Some(ShareCopy::Pair));

        // The machine turns up in the member list, which is the fact the user
        // is waiting on — the pairing file catches up out of process.
        tui.snapshot.members.push(crate::layout::Member {
            peer_id: b"droplet".to_vec(),
            endpoint_addr: b"endpoint-droplet".to_vec(),
            display_name: String::from("droplet"),
            kind: Default::default(),
            machine_proof: Default::default(),
            machine_id: Default::default(),
        });
        assert_eq!(tui.add_machine_joined().as_deref(), Some("droplet"));

        let _ = tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), area);
        assert!(!tui.add_machine_open());
    }

    /// The panel is waiting on another machine. A key that walked out from
    /// under it would leave a dialog owning the keyboard with nothing on
    /// screen, so it holds every key but the one that ends the session.
    #[test]
    pub(in crate::tui) fn the_add_machine_panel_holds_the_keyboard_until_it_is_dismissed() {
        let area = Rect::new(0, 0, 100, 24);
        let mut tui = crate::tui::test_support::home_tui(&[]);
        tui.set_home_open(true, "test");
        let _ = tui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), area);

        for key in [KeyCode::Char('n'), KeyCode::Char('q'), KeyCode::Down] {
            assert_eq!(
                tui.handle_key(KeyEvent::new(key, KeyModifiers::NONE), area),
                KeyHandling::Consumed(vec![]),
                "{key:?} must not reach Home while the panel is up"
            );
            assert!(tui.add_machine_open());
        }
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Consumed(vec![])
        );
        assert!(tui.home_open(), "and never closes the screen underneath it");

        // Ctrl+Q outranks every panel, here as everywhere else.
        assert_eq!(
            tui.handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                area
            ),
            KeyHandling::Quit(QuitAction::Detach)
        );
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
