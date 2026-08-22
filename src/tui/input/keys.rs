//! Key classification and the byte encodings a PTY expects.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::ChordMode;

pub(in crate::tui) const ESC_PREFIX_WINDOW: Duration = Duration::from_millis(50);
/// Whether this key is one of the two ways to move focus between panes.
///
/// Option+arrow is the original and stays; Ctrl+arrow was asked for because it
/// is what every other tiling thing on a desktop uses, and reaching for Option
/// and Shift together to look at the pane next door is a lot of hand for a very
/// ordinary action.
///
/// Ctrl+arrow does cost something: it is word-jump in readline, and inside a
/// pane it no longer reaches the shell. **Ctrl+Alt+arrow** is the escape hatch
/// and is forwarded untouched, which is why holding both is deliberately not a
/// focus key. `Ctrl+P` then an arrow is the other way, for anyone who would
/// rather rebind nothing.
pub(in crate::tui) fn is_focus_arrow(key: KeyEvent) -> bool {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    // Exactly one of them. Both together is the way back to the shell.
    (alt != control)
        && matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
        )
}
#[derive(Default)]
pub(in crate::tui) struct PendingEscape {
    started: Option<Instant>,
}
impl PendingEscape {
    pub(in crate::tui) fn start(&mut self, now: Instant) {
        self.started = Some(now);
    }

    pub(in crate::tui) fn take_if_expired(&mut self, now: Instant) -> bool {
        if !self
            .started
            .is_some_and(|started| now.saturating_duration_since(started) >= ESC_PREFIX_WINDOW)
        {
            return false;
        }
        self.started = None;
        true
    }

    pub(in crate::tui) fn take_option_arrow(&mut self, key: KeyEvent) -> Option<KeyEvent> {
        (self.started.is_some() && key.modifiers.is_empty())
            .then_some(key.code)
            .filter(|code| {
                matches!(
                    code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                )
            })
            .map(|code| {
                self.started = None;
                KeyEvent::new(code, KeyModifiers::ALT)
            })
    }

    pub(in crate::tui) fn take(&mut self) -> bool {
        self.started.take().is_some()
    }
}
/// Whether an unclaimed key ends a sticky chord mode on its way to the pane.
///
/// The escape hatch is "type any normal key to leave the mode and send that key
/// to the focused PTY", and *type* is the operative word. A key held with Ctrl
/// or Option is not somebody typing: Ctrl+F is `forward-char`, Ctrl+C is an
/// interrupt, Option+B steps back a word. Ending pane mode on those made the
/// mux move under the user for a keystroke that was never aimed at it — which
/// is exactly what "Ctrl+F exits a pane" turned out to be. They still reach the
/// pane; they simply no longer take the mode with them, and the idle timeout
/// still clears it two seconds later.
///
/// Shift does not count. That is how a capital letter is typed, and rejecting
/// it would make the escape hatch unreachable for half the alphabet.
pub(in crate::tui) fn ends_chord_mode(key: KeyEvent) -> bool {
    (key.modifiers - KeyModifiers::SHIFT).is_empty()
}
pub(in crate::tui) fn is_chord_command(mode: ChordMode, key: KeyEvent) -> bool {
    // An uppercase chord letter arrives with SHIFT held, which is not a modifier the user
    // added on purpose -- it is how the letter is typed. Rejecting it outright would make
    // any uppercase chord permanently unreachable.
    let uppercase = matches!(key.code, KeyCode::Char(letter) if letter.is_ascii_uppercase());
    let permitted = if uppercase {
        (key.modifiers - KeyModifiers::SHIFT).is_empty()
    } else {
        key.modifiers.is_empty()
    };
    if !permitted {
        return false;
    }
    match mode {
        ChordMode::Pane => matches!(
            key.code,
            KeyCode::Char('e')
                | KeyCode::Char('n')
                | KeyCode::Char('r')
                | KeyCode::Char('l')
                | KeyCode::Char('d')
                | KeyCode::Char('u')
                | KeyCode::Char('x')
                | KeyCode::Char('z')
                | KeyCode::Char('k')
                | KeyCode::Char('L')
                | KeyCode::Char('i')
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
        ),
        ChordMode::Tab => matches!(
            key.code,
            KeyCode::Char('e')
                | KeyCode::Char('n')
                | KeyCode::Char('x')
                | KeyCode::Left
                | KeyCode::Right
        ),
        ChordMode::None => false,
    }
}
pub(in crate::tui) fn is_chord_navigation(mode: ChordMode, key: KeyEvent) -> bool {
    if !key.modifiers.is_empty() {
        return false;
    }
    match mode {
        ChordMode::Pane => matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
        ),
        ChordMode::Tab => matches!(key.code, KeyCode::Left | KeyCode::Right),
        ChordMode::None => false,
    }
}
pub(in crate::tui) fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL
}
pub(in crate::tui) fn encode_key(
    key: KeyEvent,
    screen: &vt100::Screen,
    kitty_keyboard_active: bool,
) -> Option<Vec<u8>> {
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
        // Use kitty CSI-u when the child opted in; otherwise LF is the newline fallback.
        KeyCode::Enter if modifiers == 2 => {
            if kitty_keyboard_active {
                b"\x1b[13;2u".to_vec()
            } else {
                b"\n".to_vec()
            }
        }
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
pub(in crate::tui) fn encode_paste(text: &str, bracketed_paste: bool) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags};

    use crate::tui::ChordMode;

    use super::{encode_key, encode_paste, is_chord_command, is_chord_navigation};

    #[test]
    fn disambiguate_escape_codes_uses_kitty_flag_one() {
        assert_eq!(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.bits(),
            1
        );
    }

    #[test]
    fn directional_split_keys_are_commands_but_not_chord_navigation() {
        for key in ['r', 'l', 'd', 'u'] {
            assert!(is_chord_command(
                ChordMode::Pane,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
            ));
            assert!(!is_chord_navigation(
                ChordMode::Pane,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
            ));
        }
    }

    #[test]
    fn an_uppercase_chord_letter_is_reachable_despite_its_shift_modifier() {
        // Shift is how `L` is typed, not a modifier the user chose to add. An earlier
        // version rejected every modified key here, which made the session lock
        // impossible to trigger from a real keyboard while every unit test passed.
        assert!(is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT)
        ));
        assert!(is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE)
        ));
        // A genuinely extra modifier is still not a chord command.
        assert!(!is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::CONTROL)
        ));
        assert!(!is_chord_command(
            ChordMode::Pane,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn up_respects_application_cursor_mode() {
        let normal = vt100::Parser::new(1, 1, 0);
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                normal.screen(),
                false,
            ),
            Some(b"\x1b[A".to_vec())
        );

        let mut application = vt100::Parser::new(1, 1, 0);
        application.process(b"\x1b[?1h");
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                application.screen(),
                false,
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
                encode_key(event, screen, false).as_deref(),
                expected.map(str::as_bytes),
                "{event:?}"
            );
        }
    }

    #[test]
    fn shift_enter_uses_kitty_csi_u_when_active_and_lf_when_inactive() {
        let parser = vt100::Parser::new(1, 1, 0);
        let plain = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            parser.screen(),
            false,
        );
        let plain_active = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            parser.screen(),
            true,
        );
        let shifted_inactive = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            parser.screen(),
            false,
        );
        let shifted_active = encode_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            parser.screen(),
            true,
        );

        assert_eq!(plain, Some(b"\r".to_vec()));
        assert_eq!(plain_active, Some(b"\r".to_vec()));
        assert_eq!(shifted_inactive, Some(b"\n".to_vec()));
        assert_eq!(shifted_active, Some(b"\x1b[13;2u".to_vec()));
        assert_ne!(shifted_inactive, plain);
        assert_ne!(shifted_active, plain);
    }
}
