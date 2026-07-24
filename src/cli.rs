//! Command-line parsing and live session dispatch.

use std::{
    error::Error,
    io::{self, Write},
    str::FromStr,
};

use clap::{Parser, Subcommand};
use tokio::task::JoinSet;

use crate::{
    session::{HostSession, join_pane},
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
    let mut handshakes = JoinSet::new();
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
            incoming = host.accept_incoming() => match incoming {
                Ok(incoming) => {
                    let host = host.clone();
                    handshakes.spawn(async move {
                        if host.handle_incoming(incoming).await.is_err() {
                            eprintln!("join handshake failed");
                        }
                    });
                }
                Err(_) => eprintln!("incoming handshake accept failed"),
            },
        }
    }
    handshakes.abort_all();
    while handshakes.join_next().await.is_some() {}
    host.close().await;
    Ok(())
}
