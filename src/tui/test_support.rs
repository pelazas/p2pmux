//! Fixtures shared by the tests of several `tui` submodules.

use crate::tui::PaneMouseProtocol;

pub(in crate::tui) fn mouse_protocol(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> PaneMouseProtocol {
    PaneMouseProtocol { mode, encoding }
}

pub(in crate::tui) fn sgr_protocol(mode: vt100::MouseProtocolMode) -> PaneMouseProtocol {
    mouse_protocol(mode, vt100::MouseProtocolEncoding::Sgr)
}
