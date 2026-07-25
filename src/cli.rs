//! Command-line parsing and live session dispatch.

use std::{
    error::Error,
    io::{self, Write},
    str::FromStr,
};

use clap::{Parser, Subcommand};

use crate::{
    rendezvous::{LocalRendezvous, RendezvousError},
    session::{
        HostSession, LayoutControlEvent, SharedLayoutHost, join_layout, layout_snapshot_from_state,
    },
    ticket::{JoinTicket, TICKET_PREFIX},
    transport::Transport,
};

/// The temporary p2pmux command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "p2pmux",
    about = "Peer-to-peer multiplayer terminal multiplexer"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start one local interactive shell (Spike 1).
    Local,
    /// Create a shared session with a reusable join ticket.
    Create,
    /// Join a remote fixed-grid shared pane using a reusable shared-session ticket.
    Join { ticket: String },
}

const TRUST_WARNING: &str = "TRUST WARNING: This is a fully trusted shared-shell session. Anyone with the join ticket can see every pane and may obtain interactive control of available terminals (run commands, see output, touch files reachable to that macOS user).

Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege Mac account and avoid production credentials in shared panes.

Processes and credential files stay on the pane host's Mac (not uploaded to peers). That does not stop a controller from using or displaying them via the shared shell.";

#[derive(Debug)]
struct CliError(&'static str);

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for CliError {}

/// Parse process arguments and run a command.
pub async fn parse_and_run() -> Result<(), Box<dyn Error>> {
    run(Cli::parse()).await
}

async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Local => crate::tui::run_local(),
        Command::Create => {
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{TRUST_WARNING}\n")?;
                stdout.flush()?;
            }
            let (cols, rows) = crossterm::terminal::size()?;
            let (shell_rows, shell_cols) = crate::tui::initial_root_pane_grid(cols, rows);
            let host = SharedLayoutHost::new(HostSession::create().await?, shell_rows, shell_cols)?;
            let host_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
            let initial = crate::tui::SharedLocalPane::spawn(
                1,
                shell_rows,
                shell_cols,
                host_peer_id.clone(),
            )?;
            let pane_server = host.pane_server();
            pane_server.register_local_pane(
                crate::protocol::PaneDescriptor {
                    pane_id: 1,
                    host_peer_id,
                    grid_rows: u32::from(shell_rows),
                    grid_cols: u32::from(shell_cols),
                },
                initial.channels(),
            )?;
            let snapshot = host.session_snapshot()?;
            let layout =
                layout_snapshot_from_state(snapshot.state.as_ref().ok_or("missing host layout")?)
                    .map_err(|error| io::Error::other(format!("invalid host layout: {error:?}")))?;
            let dispatcher = host.incoming_dispatcher(pane_server.clone())?;
            let dispatcher_task = tokio::spawn(async move { dispatcher.accept_loop().await });
            let rendezvous = LocalRendezvous::for_current_user()?.publish(host.ticket())?;
            let join_code = rendezvous.code().to_string();
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "Join with: p2pmux join {join_code}")?;
                writeln!(
                    stdout,
                    "This code stays visible in the host status bar after you continue."
                )?;
                if !host.address_ready() {
                    writeln!(
                        stdout,
                        "WARNING: the ticket contains only currently discovered direct addresses; localhost/LAN is supported but public reachability is not yet confirmed."
                    )?;
                }
                write!(stdout, "Press Enter to start the host shell…")?;
                stdout.flush()?;
                drop(stdout);
            }
            wait_for_enter()?;
            // SharedLayoutRuntime moves terminal I/O to a blocking thread, so no StdoutLock may survive.
            let handle = tokio::runtime::Handle::current();
            let session_id = host.ticket().session_id().to_vec();
            let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let mut runtime = crate::tui::SharedLayoutRuntime::host(
                    host,
                    pane_server,
                    layout,
                    initial,
                    join_code,
                    handle,
                )
                .map_err(|error| error.to_string())?;
                runtime.set_session_id(session_id);
                runtime.run().map_err(|error| error.to_string())
            })
            .await?;
            dispatcher_task.abort();
            let _ = dispatcher_task.await;
            rendezvous.remove()?;
            result.map_err(io::Error::other)?;
            Ok(())
        }
        Command::Join { ticket } => {
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{TRUST_WARNING}\n")?;
                stdout.flush()?;
            }
            let ticket = resolve_join_ticket(&ticket)?;
            let transport = Transport::bind().await?;
            let mut member = join_layout(transport, ticket.clone()).await?;
            let state = match member.events.recv().await {
                Some(LayoutControlEvent::Snapshot(snapshot)) => {
                    snapshot.state.ok_or("missing layout snapshot")?
                }
                Some(_) => return Err(io::Error::other("expected initial layout snapshot").into()),
                None => {
                    return Err(
                        io::Error::other("layout coordinator disconnected during join").into(),
                    );
                }
            };
            let pane_server = member.pane_server(ticket.session_id().to_vec())?;
            pane_server.replace_roster_from_layout(&state)?;
            let pane_acceptor = pane_server.clone();
            let pane_accept_task = tokio::spawn(async move { pane_acceptor.accept_loop().await });
            let handle = tokio::runtime::Handle::current();
            let guest_result = tokio::task::spawn_blocking(move || {
                crate::tui::SharedLayoutRuntime::member_from_state(
                    member,
                    pane_server,
                    ticket.session_id().to_vec(),
                    state,
                    handle,
                )
                .and_then(|runtime| runtime.run())
                .map_err(|error| error.to_string())
            })
            .await?;
            pane_accept_task.abort();
            let _ = pane_accept_task.await;
            guest_result.map_err(io::Error::other)?;
            Ok(())
        }
    }
}

fn resolve_join_ticket(input: &str) -> Result<JoinTicket, CliError> {
    if input.starts_with(TICKET_PREFIX) {
        return JoinTicket::from_str(input).map_err(|_| CliError("invalid ticket format"));
    }
    LocalRendezvous::for_current_user()
        .and_then(|store| store.resolve(input))
        .map_err(|error| match error {
            RendezvousError::NotFound => CliError("join code was not found on this Mac"),
            _ => CliError("invalid ticket format"),
        })
}

fn wait_for_enter() -> io::Result<()> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn create_releases_stdout_before_starting_the_host_tui() {
        let source = include_str!("cli.rs");
        let create_arm = source
            .split_once("Command::Create => {")
            .expect("create arm")
            .1
            .split_once("Command::Join { ticket } => {")
            .expect("join arm")
            .0;

        assert!(
            create_arm
                .find("drop(stdout);")
                .expect("stdout is released")
                < create_arm
                    .find("SharedLayoutRuntime::host(")
                    .expect("host TUI starts"),
            "create must release stdout before the host TUI runs on a blocking thread"
        );
        assert!(
            create_arm.contains("wait_for_enter()?"),
            "create must pause so the join code can be copied before the TUI starts"
        );
    }
}
