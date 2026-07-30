//! The two kinds of pane a session drives — one this process hosts, one a
//! peer hosts — and the control channel they share.

pub(in crate::tui) mod control;
pub(in crate::tui) mod local;
pub(in crate::tui) mod remote;
