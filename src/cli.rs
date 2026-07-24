//! Command-line parsing and live session dispatch.

use std::{
    error::Error,
    io::{self, Write},
    str::FromStr,
};

use clap::{Parser, Subcommand};
use portable_pty::PtySize;

use crate::{
    lease::LeaseManager,
    screen::HostScreen,
    session::{DEFAULT_PANE_ID, HostPaneChannels, HostSession, join_pane},
    ticket::JoinTicket,
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
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{TRUST_WARNING}\n")?;
            stdout.flush()?;
            let host = HostSession::create().await?;
            writeln!(stdout, "Reusable shared-session ticket:")?;
            writeln!(stdout, "{}", host.ticket())?;
            writeln!(
                stdout,
                "Waiting for join handshakes; press Ctrl-C to end this live session."
            )?;
            if !host.address_ready() {
                writeln!(
                    stdout,
                    "WARNING: the ticket contains only currently discovered direct addresses; localhost/LAN is supported but public reachability is not yet confirmed."
                )?;
            }
            stdout.flush()?;
            run_host(host).await
        }
        Command::Join { ticket } => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{TRUST_WARNING}\n")?;
            stdout.flush()?;
            let ticket =
                JoinTicket::from_str(&ticket).map_err(|_| CliError("invalid ticket format"))?;
            let pane = join_pane(Transport::bind().await?, ticket).await?;
            drop(stdout);
            let guest_result = tokio::task::spawn_blocking(move || {
                crate::tui::run_guest(pane).map_err(|error| error.to_string())
            })
            .await?;
            guest_result.map_err(io::Error::other)?;
            Ok(())
        }
    }
}

async fn run_host(host: HostSession) -> Result<(), Box<dyn Error>> {
    let (cols, rows) = crossterm::terminal::size()?;
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let host_peer_id = host.ticket().endpoint_addr().id.as_bytes().to_vec();
    let screen = HostScreen::new(rows, cols)?;
    let (screen_tx, screen_rx) = tokio::sync::watch::channel(screen.current_frame().clone());
    let lease = LeaseManager::new(host_peer_id.clone(), std::time::Instant::now());
    let (lease_tx, lease_rx) = tokio::sync::watch::channel(lease.state().clone());
    let (control_tx, control_rx) = tokio::sync::mpsc::channel(256);
    let runtime = crate::tui::HostPaneRuntime::new(
        size,
        host_peer_id.clone(),
        screen_tx.clone(),
        lease_tx,
        control_rx,
    )?;
    let accept_task = {
        let host = host.clone();
        tokio::spawn(async move {
            loop {
                let Ok(incoming) = host.accept_incoming().await else {
                    return;
                };
                let pane = HostPaneChannels {
                    pane_id: DEFAULT_PANE_ID.to_vec(),
                    host_peer_id: host_peer_id.clone(),
                    screen_rx: screen_rx.clone(),
                    lease_rx: lease_rx.clone(),
                    control_tx: control_tx.clone(),
                };
                let host = host.clone();
                tokio::spawn(async move {
                    let _ = host.serve_peer(incoming, pane).await;
                });
            }
        })
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::tui::run_host(runtime).map_err(|error| error.to_string())
    })
    .await?;
    accept_task.abort();
    host.close().await;
    result.map_err(io::Error::other)?;
    Ok(())
}
