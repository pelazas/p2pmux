use p2pmux::{
    cli::Cli, protocol::PROTOCOL_VERSION, pty_host::PtyHost, session::Session, ticket::JoinTicket,
    transport::Transport, tui::Tui,
};

#[test]
fn exposes_the_scaffold_module_boundaries() {
    let _: Option<Cli> = None;
    let _ = Tui;
    let _: Option<PtyHost> = None;
    let _ = Session;
    let _ = Transport;
    let _ = JoinTicket;
    assert_eq!(PROTOCOL_VERSION, 1);
}
