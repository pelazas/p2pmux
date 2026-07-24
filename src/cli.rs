//! Command-line parsing and scaffold-only command dispatch.

use std::{
    error::Error,
    io::{self, Write},
};

use clap::{Parser, Subcommand};

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
    /// Create a shared session (scaffold only).
    Create,
    /// Join a shared session using a reusable ticket (scaffold only).
    Join { ticket: String },
}

const TRUST_WARNING: &str = "TRUST WARNING: This is a fully trusted shared-shell session. Anyone with the join ticket can see every pane and may obtain interactive control of available terminals (run commands, see output, touch files reachable to that macOS user).

Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege Mac account and avoid production credentials in shared panes.

Processes and credential files stay on the pane host's Mac (not uploaded to peers). That does not stop a controller from using or displaying them via the shared shell.";

/// Parse process arguments and run a scaffold-only command.
pub fn parse_and_run() -> Result<(), Box<dyn Error>> {
    run(Cli::parse())
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Local => crate::tui::run_local(),
        Command::Create => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{TRUST_WARNING}\n")?;
            writeln!(stdout, "create is not implemented in the scaffold.")?;
            Ok(())
        }
        Command::Join { ticket } => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{TRUST_WARNING}\n")?;
            let _ = ticket;
            writeln!(stdout, "join is not implemented in the scaffold.")?;
            Ok(())
        }
    }
}
