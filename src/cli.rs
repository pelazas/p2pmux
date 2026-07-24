//! Command-line parsing and live session dispatch.

use std::{
    error::Error,
    future::Future,
    io::{self, Write},
    pin::Pin,
    str::FromStr,
};

use clap::{Parser, Subcommand};
use portable_pty::PtySize;

use crate::{
    lease::LeaseManager,
    screen::HostScreen,
    session::{DEFAULT_PANE_ID, HostPaneChannels, HostSession, SessionError, join_pane},
    ticket::JoinTicket,
    transport::{Transport, TransportError},
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
        let accept_host = host.clone();
        let serve_host = host.clone();
        tokio::spawn(async move {
            accept_until_closed(
                move || {
                    let host = accept_host.clone();
                    Box::pin(async move { host.accept_incoming().await })
                        as Pin<Box<dyn Future<Output = Result<_, SessionError>> + Send>>
                },
                move |incoming| {
                    let pane = HostPaneChannels {
                        pane_id: DEFAULT_PANE_ID.to_vec(),
                        host_peer_id: host_peer_id.clone(),
                        screen_rx: screen_rx.clone(),
                        lease_rx: lease_rx.clone(),
                        control_tx: control_tx.clone(),
                    };
                    let host = serve_host.clone();
                    tokio::spawn(async move {
                        let _ = host.serve_peer(incoming, pane).await;
                    });
                },
            )
            .await;
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

async fn accept_until_closed<T, Accept, Handle>(mut accept: Accept, mut handle: Handle)
where
    Accept: FnMut() -> Pin<Box<dyn Future<Output = Result<T, SessionError>> + Send>>,
    Handle: FnMut(T),
{
    loop {
        match accept().await {
            Ok(incoming) => handle(incoming),
            Err(error) if endpoint_is_closed(&error) => return,
            Err(error) if !idle_accept_timed_out(&error) => {
                eprintln!("p2pmux accept failed; continuing to listen: {error}");
            }
            Err(_) => {}
        }
    }
}

fn endpoint_is_closed(error: &SessionError) -> bool {
    matches!(error, SessionError::Transport(TransportError::Closed))
}

fn idle_accept_timed_out(error: &SessionError) -> bool {
    matches!(
        error,
        SessionError::Transport(TransportError::TimedOut("incoming accept"))
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex},
    };

    use crate::{session::SessionError, transport::TransportError};

    use super::accept_until_closed;

    #[tokio::test]
    async fn accept_loop_keeps_accepting_after_an_idle_timeout() {
        let results = Arc::new(Mutex::new(VecDeque::from([
            Err(SessionError::Transport(TransportError::TimedOut(
                "incoming accept",
            ))),
            Ok("joined"),
            Err(SessionError::Transport(TransportError::Closed)),
        ])));
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let next = results.clone();
        let handled = accepted.clone();

        accept_until_closed(
            move || {
                let next = next.clone();
                Box::pin(async move {
                    next.lock()
                        .expect("results lock")
                        .pop_front()
                        .expect("loop should stop when endpoint closes")
                })
                    as Pin<Box<dyn Future<Output = Result<&'static str, SessionError>> + Send>>
            },
            move |incoming| handled.lock().expect("accepted lock").push(incoming),
        )
        .await;

        assert_eq!(*accepted.lock().expect("accepted lock"), vec!["joined"]);
    }
}
