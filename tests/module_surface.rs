use p2pmux::{
    cli::Cli,
    protocol::{Envelope, PROTOCOL_VERSION},
    pty_host::PtyHost,
    session::HostSession,
    ticket::JoinTicket,
    transport::Transport,
    tui::Tui,
};

#[test]
fn exposes_the_scaffold_module_boundaries() {
    let _: Option<Cli> = None;
    let _ = Tui;
    let _: Option<PtyHost> = None;
    let _: Option<Transport> = None;
    let _: Option<JoinTicket> = None;
    let _: Option<HostSession> = None;
    let _: Option<Envelope> = None;
    assert_eq!(PROTOCOL_VERSION, 6);
}
