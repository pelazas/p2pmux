//! xterm mouse reporting: what a pane's child asked for, and how to encode it.

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

use crate::tui::ScreenCell;

/// Low two bits of an xterm button code, by button.
const MOUSE_BUTTON_LEFT: u8 = 0;
const MOUSE_BUTTON_MIDDLE: u8 = 1;
const MOUSE_BUTTON_RIGHT: u8 = 2;
/// Legacy encodings report every release with the button bits set.
const MOUSE_BUTTON_RELEASE: u8 = 3;
const MOUSE_MOTION_FLAG: u8 = 32;
const MOUSE_WHEEL_FLAG: u8 = 64;
const MOUSE_SHIFT_FLAG: u8 = 4;
const MOUSE_ALT_FLAG: u8 = 8;
const MOUSE_CONTROL_FLAG: u8 = 16;
/// Legacy button/coordinate bytes are biased so they land in printable ASCII.
const MOUSE_LEGACY_BIAS: u16 = 32;
/// The largest coordinate a single biased byte can carry.
const MOUSE_LEGACY_MAX: u16 = 223;
/// The xterm mouse reporting a pane's child has asked for, if any.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneMouseProtocol {
    pub(in crate::tui) mode: vt100::MouseProtocolMode,
    pub(in crate::tui) encoding: vt100::MouseProtocolEncoding,
}
impl PaneMouseProtocol {
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        Self {
            mode: screen.mouse_protocol_mode(),
            encoding: screen.mouse_protocol_encoding(),
        }
    }

    /// Whether the child wants mouse events at all; when false the mux keeps them.
    pub fn reports_mouse(self) -> bool {
        self.mode != vt100::MouseProtocolMode::None
    }
}
fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => MOUSE_BUTTON_LEFT,
        MouseButton::Middle => MOUSE_BUTTON_MIDDLE,
        MouseButton::Right => MOUSE_BUTTON_RIGHT,
    }
}
fn mouse_modifier_flags(modifiers: KeyModifiers) -> u8 {
    let mut flags = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        flags |= MOUSE_SHIFT_FLAG;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        flags |= MOUSE_ALT_FLAG;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        flags |= MOUSE_CONTROL_FLAG;
    }
    flags
}
/// Encodes a pane-local mouse event the way the child asked to receive it.
///
/// Returns `None` when the child's [`vt100::MouseProtocolMode`] does not subscribe to
/// this event, so callers can fall back to mux-local handling.
pub(in crate::tui) fn encode_mouse(
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    cell: ScreenCell,
    protocol: PaneMouseProtocol,
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode as Mode;

    if !protocol.reports_mouse() {
        return None;
    }
    let (button, release) = match kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), false),
        MouseEventKind::Up(button) => {
            // X10 mode reports presses only.
            if protocol.mode == Mode::Press {
                return None;
            }
            (mouse_button_code(button), true)
        }
        MouseEventKind::Drag(button) => {
            if !matches!(protocol.mode, Mode::ButtonMotion | Mode::AnyMotion) {
                return None;
            }
            (mouse_button_code(button) | MOUSE_MOTION_FLAG, false)
        }
        MouseEventKind::Moved => {
            if protocol.mode != Mode::AnyMotion {
                return None;
            }
            (MOUSE_BUTTON_RELEASE | MOUSE_MOTION_FLAG, false)
        }
        // Wheel reports are presses without a matching release in every mode.
        MouseEventKind::ScrollUp => (MOUSE_WHEEL_FLAG, false),
        MouseEventKind::ScrollDown => (MOUSE_WHEEL_FLAG | 1, false),
        MouseEventKind::ScrollLeft => (MOUSE_WHEEL_FLAG | 2, false),
        MouseEventKind::ScrollRight => (MOUSE_WHEEL_FLAG | 3, false),
    };
    let code = button | mouse_modifier_flags(modifiers);
    let column = cell.col.saturating_add(1);
    let row = cell.row.saturating_add(1);

    Some(match protocol.encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let final_byte = if release { 'm' } else { 'M' };
            format!("\x1b[<{code};{column};{row}{final_byte}").into_bytes()
        }
        encoding => {
            // Legacy encodings cannot name the released button, only that one was.
            let code = if release {
                code | MOUSE_BUTTON_RELEASE
            } else {
                code
            };
            let mut bytes = b"\x1b[M".to_vec();
            for value in [u16::from(code), column, row] {
                let biased = value.checked_add(MOUSE_LEGACY_BIAS)?;
                if encoding == vt100::MouseProtocolEncoding::Utf8 {
                    let mut buffer = [0u8; 4];
                    bytes.extend_from_slice(
                        char::from_u32(u32::from(biased))?
                            .encode_utf8(&mut buffer)
                            .as_bytes(),
                    );
                } else {
                    // A cell past the single-byte ceiling has no representation at all.
                    if biased > MOUSE_LEGACY_MAX {
                        return None;
                    }
                    bytes.push(biased as u8);
                }
            }
            bytes
        }
    })
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

    use crate::tui::{
        ScreenCell,
        test_support::{mouse_protocol, sgr_protocol},
    };

    use super::{PaneMouseProtocol, encode_mouse};

    #[test]
    fn encode_mouse_skips_panes_without_mouse_reporting() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                PaneMouseProtocol::default(),
            ),
            None
        );
    }

    #[test]
    fn encode_mouse_reports_sgr_press_with_one_based_cells() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 4, col: 11 },
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            Some(b"\x1b[<0;12;5M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_marks_sgr_release_with_a_lowercase_final_byte() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Right),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            Some(b"\x1b[<2;1;1m".to_vec())
        );
    }

    #[test]
    fn encode_mouse_folds_modifiers_into_the_button_code() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                ScreenCell { row: 0, col: 0 },
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            Some(b"\x1b[<24;1;1M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_drops_release_in_press_only_mode() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                sgr_protocol(vt100::MouseProtocolMode::Press),
            ),
            None
        );
    }

    #[test]
    fn encode_mouse_reports_drag_only_once_the_child_asks_for_motion() {
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let cell = ScreenCell { row: 0, col: 0 };
        assert_eq!(
            encode_mouse(
                drag,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::PressRelease),
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                drag,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::ButtonMotion),
            ),
            Some(b"\x1b[<32;1;1M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_reports_bare_motion_only_in_any_motion_mode() {
        let cell = ScreenCell { row: 0, col: 0 };
        assert_eq!(
            encode_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::ButtonMotion),
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::AnyMotion),
            ),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_reports_wheel_even_in_press_only_mode() {
        let cell = ScreenCell { row: 2, col: 2 };
        assert_eq!(
            encode_mouse(
                MouseEventKind::ScrollUp,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::Press),
            ),
            Some(b"\x1b[<64;3;3M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::ScrollDown,
                KeyModifiers::NONE,
                cell,
                sgr_protocol(vt100::MouseProtocolMode::Press),
            ),
            Some(b"\x1b[<65;3;3M".to_vec())
        );
    }

    #[test]
    fn encode_mouse_biases_legacy_reports_into_printable_bytes() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 4, col: 11 },
                mouse_protocol(
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Default,
                ),
            ),
            Some(vec![0x1b, b'[', b'M', 32, 32 + 12, 32 + 5])
        );
    }

    #[test]
    fn encode_mouse_reports_legacy_release_without_naming_the_button() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Right),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 0 },
                mouse_protocol(
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Default,
                ),
            ),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33])
        );
    }

    #[test]
    fn encode_mouse_drops_legacy_cells_past_the_single_byte_ceiling() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                ScreenCell { row: 0, col: 300 },
                mouse_protocol(
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Default,
                ),
            ),
            None
        );
    }

    #[test]
    fn encode_mouse_widens_utf8_cells_past_the_single_byte_ceiling() {
        let bytes = encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            ScreenCell { row: 0, col: 300 },
            mouse_protocol(
                vt100::MouseProtocolMode::PressRelease,
                vt100::MouseProtocolEncoding::Utf8,
            ),
        )
        .expect("utf8 encoding carries wide coordinates");
        let mut expected = b"\x1b[M".to_vec();
        expected.push(32);
        expected.extend_from_slice(char::from_u32(333).unwrap().to_string().as_bytes());
        expected.push(33);
        assert_eq!(bytes, expected);
    }
}
