use p2pmux::{
    cli::Cli,
    failover::Election,
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
    let _: Option<Election> = None;
    // Bumped by the coordinator epoch, which every welcome and every ledger entry now
    // carries: a peer on 9 cannot tell which coordinator sealed what it is being sent.
    assert_eq!(PROTOCOL_VERSION, 12);
}
