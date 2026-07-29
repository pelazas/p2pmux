//! Command-line parsing and live session dispatch.

use std::{
    error::Error,
    io::{self, IsTerminal, Write},
    process::{Command as ProcessCommand, Stdio},
    str::FromStr,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};

use crate::{
    rendezvous::{LocalRendezvous, RendezvousError},
    session::{
        HostSession, LayoutControlEvent, SharedLayoutHost, join_layout_with_display_name,
        layout_snapshot_from_state,
    },
    ticket::{JoinTicket, looks_like_ticket},
    transport::Transport,
};

/// The temporary p2pmux command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "p2pmux",
    about = "Peer-to-peer multiplayer terminal multiplexer"
)]
pub struct Cli {
    #[arg(long)]
    resume: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start one local interactive shell (Spike 1).
    Local,
    /// Create a shared session with a reusable join ticket.
    Create {
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "session-name")]
        session_name: Option<String>,
    },
    /// Join a remote fixed-grid shared pane using a reusable shared-session ticket.
    Join {
        ticket: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Print the full reusable join ticket for a live local session.
    Ticket {
        /// The join code shown in the session footer. Omit it when one session is live.
        code: Option<String>,
    },
    /// Attach a live local session by memorable name.
    Attach { name: String },
    /// Gracefully stop a live local session.
    Kill {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Rename a live local session finder record.
    Rename { old: String, new: String },
    /// Read or write local configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    #[command(name = "__node", hide = true)]
    Node {
        #[arg(long)]
        bootstrap: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Set { key: String, value: String },
    Get { key: String },
    Init,
}

const TRUST_WARNING: &str = "TRUST WARNING: This is a fully trusted shared-shell session. Anyone with the join ticket can see every pane and may obtain interactive control of available terminals (run commands, see output, touch files reachable to that macOS user).

Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege Mac account and avoid production credentials in shared panes.

Processes and credential files stay on the pane host's Mac (not uploaded to peers). That does not stop a controller from using or displaying them via the shared shell.";

/// The short form of `TRUST_WARNING`, for a command whose stdout is meant to be piped.
const TICKET_WARNING: &str = "TRUST WARNING: this ticket grants full shared-shell access to the session for as long as it lives, to anyone who holds it. Share it only with people you trust with that access.";

#[derive(Debug)]
struct CliError(&'static str);

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for CliError {}

/// Why the background node refused to start, forwarded verbatim from the node itself.
#[derive(Debug)]
struct NodeStartupError(String);

impl std::fmt::Display for NodeStartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for NodeStartupError {}

/// Parse process arguments and run a command.
pub async fn parse_and_run() -> Result<(), Box<dyn Error>> {
    run(Cli::parse()).await
}

/// Hand this Mac's completion tuning to the detector, once per process.
///
/// Panes are hosted by the detached node, which reaches this same dispatch, so every process
/// that can own a PTY passes through here. A config that cannot be read is not worth failing a
/// session over — the built-in defaults are the ones most users want anyway.
fn apply_notification_tuning() {
    let notifications = crate::config::load_config()
        .map(|config| config.ui.notifications)
        .unwrap_or_default();
    crate::agent_detect::set_notification_tuning(crate::agent_detect::NotificationTuning {
        quiet_before_done: Duration::from_secs(notifications.quiet_seconds),
        require_bell: notifications.require_bell,
    });
}

async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    apply_notification_tuning();
    if cli.resume {
        return resume_picker(true);
    }
    match cli.command {
        None => resume_picker(false),
        Some(Command::Node { bootstrap }) => {
            // This process has no terminal and its stderr is /dev/null, so a startup
            // failure would otherwise be invisible and the launcher could only report
            // that the socket never appeared. Leave the reason where it can find it.
            let result = match crate::node::read_bootstrap(&bootstrap) {
                Ok(parsed) => crate::node::run_background(parsed).await,
                Err(error) => Err(error.into()),
            };
            if let Err(error) = &result {
                let _ = std::fs::write(bootstrap.with_extension("error"), error.to_string());
            }
            result
        }
        Some(Command::Local) => crate::tui::run_local(),
        Some(Command::Config { command }) => match command {
            ConfigCommand::Init => {
                crate::config::init()?;
                Ok(())
            }
            ConfigCommand::Set { key, value } if key == "name" => {
                crate::config::save(&value)?;
                Ok(())
            }
            ConfigCommand::Get { key } if key == "name" => {
                if let Some(name) = crate::config::load()? {
                    println!("{name}");
                }
                Ok(())
            }
            _ => Err(CliError("config key must be name").into()),
        },
        Some(Command::Create { name, session_name }) => {
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{TRUST_WARNING}\n")?;
                stdout.flush()?;
            }
            let display_name = resolve_display_name(name)?;
            let (cols, rows) = crossterm::terminal::size()?;
            let descriptor = launch_background_node(
                crate::node::NodeBootstrapKind::Create {
                    display_name: display_name.clone(),
                    cols,
                    rows,
                },
                session_name
                    .map(Ok)
                    .unwrap_or_else(crate::session_store::generate_name)?,
                crate::session_store::SessionRole::Coordinator,
            )?;
            if std::env::var_os("P2PMUX_LEGACY_FOREGROUND").is_none() {
                return crate::client::run(&descriptor);
            }
            let (cols, rows) = crossterm::terminal::size()?;
            let (shell_rows, shell_cols) = crate::tui::initial_root_pane_grid(cols, rows);
            let host = SharedLayoutHost::with_display_name(
                HostSession::create().await?,
                display_name,
                shell_rows,
                shell_cols,
            )?;
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
                    title: None,
                    locked: false,
                    exited: false,
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
        Some(Command::Join { ticket, name }) => {
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{TRUST_WARNING}\n")?;
                stdout.flush()?;
            }
            let ticket = resolve_join_ticket(&ticket)?;
            let display_name = resolve_display_name(name)?;
            let (cols, rows) = crossterm::terminal::size()?;
            let descriptor = launch_background_node(
                crate::node::NodeBootstrapKind::Join {
                    ticket: ticket.to_string(),
                    display_name: display_name.clone(),
                    cols,
                    rows,
                },
                crate::session_store::generate_name()?,
                crate::session_store::SessionRole::Member,
            )?;
            if std::env::var_os("P2PMUX_LEGACY_FOREGROUND").is_none() {
                return crate::client::run(&descriptor);
            }
            let transport = Transport::bind().await?;
            let mut member =
                join_layout_with_display_name(transport, ticket.clone(), display_name).await?;
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
        Some(Command::Ticket { code }) => print_join_ticket(code),
        Some(Command::Attach { name }) => crate::client::run(&find_live(&name)?),
        Some(Command::Kill { name, yes }) => {
            let descriptor = find_live(&name)?;
            if descriptor.role == crate::session_store::SessionRole::Coordinator && !yes {
                if !io::stdin().is_terminal() {
                    return Err(
                        CliError("coordinator kill requires --yes outside a terminal").into(),
                    );
                }
                print!(
                    "This stops the coordinator session for all peers. Kill {}? [y/N] ",
                    descriptor.name
                );
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    return Ok(());
                }
            }
            crate::client::shutdown(&descriptor)?;
            crate::session_store::SessionStore::for_current_user()?.remove(&descriptor.id)?;
            println!("Stopping {}", descriptor.name);
            Ok(())
        }
        Some(Command::Rename { old, new }) => {
            let store = crate::session_store::SessionStore::for_current_user()?;
            let descriptor = find_live(&old)?;
            let renamed = store.rename(&descriptor.id, &new)?;
            println!("Renamed {} to {}", old, renamed.name);
            Ok(())
        }
    }
}

fn find_live(name: &str) -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    crate::session_store::SessionStore::for_current_user()?
        .list_live()?
        .into_iter()
        .find(|descriptor| descriptor.name == name || descriptor.id == name)
        .ok_or_else(|| CliError("no live session with that name").into())
}

fn resume_picker(always_picker: bool) -> Result<(), Box<dyn Error>> {
    let sessions = crate::session_store::SessionStore::for_current_user()?.list_live()?;
    if sessions.is_empty() {
        if always_picker {
            return Err(CliError("no live p2pmux sessions").into());
        }
        return Err(CliError("no live session; run p2pmux create").into());
    }
    let selected = pick_session(&sessions)?;
    crate::client::run(&selected)
}

fn pick_session(
    sessions: &[crate::session_store::SessionDescriptor],
) -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    if !io::stdin().is_terminal() {
        return Ok(sessions[0].clone());
    }
    let mut selected = 0usize;
    let mut filter = String::new();
    crossterm::terminal::enable_raw_mode()?;
    let result = loop {
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        println!("p2pmux sessions  (type to filter, ↑/↓, Enter)");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let shown = sessions
            .iter()
            .filter(|session| session.name.contains(&filter))
            .collect::<Vec<_>>();
        for (index, session) in shown.iter().enumerate() {
            let marker = if index == selected { '>' } else { ' ' };
            println!(
                "{marker} {:<18} coordinator: {:<10} tabs: 1 panes: 1 hosts: 1 created: {} running: {}m",
                session.name,
                match session.role {
                    crate::session_store::SessionRole::Coordinator => "you",
                    crate::session_store::SessionRole::Member => "remote",
                },
                session.created_at,
                (now.saturating_sub(session.created_at)) / 60
            );
        }
        match crossterm::event::read()? {
            crossterm::event::Event::Key(key)
                if key.kind == crossterm::event::KeyEventKind::Press =>
            {
                match key.code {
                    crossterm::event::KeyCode::Enter if !shown.is_empty() => {
                        break Ok((*shown[selected.min(shown.len() - 1)]).clone());
                    }
                    crossterm::event::KeyCode::Up => selected = selected.saturating_sub(1),
                    crossterm::event::KeyCode::Down => {
                        selected = selected
                            .saturating_add(1)
                            .min(shown.len().saturating_sub(1))
                    }
                    crossterm::event::KeyCode::Backspace => {
                        filter.pop();
                        selected = 0;
                    }
                    crossterm::event::KeyCode::Char(character) => {
                        filter.push(character);
                        selected = 0;
                    }
                    crossterm::event::KeyCode::Esc => {
                        break Err(CliError("resume cancelled").into());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    result
}

/// Launches an isolated session owner. It has no terminal file descriptors and its own process
/// group, so closing the initiating terminal cannot take the PTYs down with it.
pub(crate) fn launch_background_node(
    kind: crate::node::NodeBootstrapKind,
    name: String,
    role: crate::session_store::SessionRole,
) -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    let store = crate::session_store::SessionStore::for_current_user()?;
    let id = crate::session_store::generate_id()?;
    let socket_path = store.socket_path(&id)?;
    let descriptor =
        crate::session_store::SessionDescriptor::new(id.clone(), name, socket_path, 1, role);
    let bootstrap = crate::node::NodeBootstrap {
        descriptor: descriptor.clone(),
        kind,
    };
    let bootstrap_path = descriptor.socket_path.with_extension("bootstrap");
    crate::node::write_bootstrap(&bootstrap_path, &bootstrap)?;
    let mut command = ProcessCommand::new(std::env::current_exe()?);
    command
        .arg("__node")
        .arg("--bootstrap")
        .arg(&bootstrap_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let error_path = descriptor.socket_path.with_extension("error");
    let _ = std::fs::remove_file(&error_path);
    command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let take_node_error = || -> Option<String> {
        let message = std::fs::read_to_string(&error_path).ok()?;
        let message = message.trim().to_owned();
        if message.is_empty() {
            return None;
        }
        let _ = std::fs::remove_file(&error_path);
        Some(message)
    };
    while Instant::now() < deadline {
        if let Ok(found) = store.read(&id)
            && found.socket_path.exists()
        {
            return Ok(found);
        }
        // The node reports why it could not start -- a full room, a dead coordinator, a
        // bad ticket. Surfacing that beats telling the user it "did not become ready",
        // which points at a local startup problem that is not the actual cause.
        if let Some(message) = take_node_error() {
            return Err(NodeStartupError(message).into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if let Some(message) = take_node_error() {
        return Err(NodeStartupError(message).into());
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "background node did not become ready",
    )
    .into())
}

fn resolve_display_name(override_name: Option<String>) -> Result<String, Box<dyn Error>> {
    if let Some(name) = override_name {
        return Ok(crate::config::save(&name)?);
    }
    if let Some(name) = crate::config::load()? {
        return Ok(name);
    }
    if !io::stdin().is_terminal() {
        return Err(CliError("missing display name; run: p2pmux config set name <name>").into());
    }
    {
        let mut stdout = io::stdout().lock();
        write!(stdout, "Choose a display name (visible to session peers): ")?;
        stdout.flush()?;
    }
    let mut name = String::new();
    io::stdin().read_line(&mut name)?;
    Ok(crate::config::save(&name)?)
}

fn resolve_join_ticket(input: &str) -> Result<JoinTicket, CliError> {
    if looks_like_ticket(input) {
        return JoinTicket::from_str(input).map_err(|_| CliError("invalid ticket format"));
    }
    LocalRendezvous::for_current_user()
        .and_then(|store| store.resolve(input))
        .map_err(rendezvous_error)
}

/// Print the portable join ticket for a session created on this Mac.
///
/// The short code only resolves through this Mac's local cache, so it is useless to a peer on
/// another machine. The ticket is the portable form, and until a hosted rendezvous exists this
/// command is the only way to obtain one. It goes to stdout alone so `p2pmux ticket | pbcopy`
/// yields something directly pasteable.
fn print_join_ticket(code: Option<String>) -> Result<(), Box<dyn Error>> {
    let store = LocalRendezvous::for_current_user().map_err(rendezvous_error)?;
    let code = match code {
        Some(code) => code,
        None => sole_local_code(&store)?,
    };
    let ticket = store.resolve(&code).map_err(rendezvous_error)?;
    {
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "{TICKET_WARNING}\n")?;
        stderr.flush()?;
    }
    println!("{ticket}");
    Ok(())
}

/// The one code published here, when leaving it off is unambiguous.
fn sole_local_code(store: &LocalRendezvous) -> Result<String, CliError> {
    let mut codes = store.codes().map_err(rendezvous_error)?;
    match codes.len() {
        0 => Err(CliError(
            "no session was created on this Mac; run p2pmux create first",
        )),
        1 => Ok(codes.remove(0)),
        _ => Err(CliError(
            "several sessions are live on this Mac; pass the join code from the session footer",
        )),
    }
}

/// Describe a rendezvous failure without ever echoing the code that caused it.
fn rendezvous_error(error: RendezvousError) -> CliError {
    match error {
        RendezvousError::NotFound => CliError("join code was not found on this Mac"),
        RendezvousError::InvalidCode | RendezvousError::InvalidRecord => {
            CliError("invalid ticket format")
        }
        _ => CliError("local rendezvous store is unavailable"),
    }
}

fn wait_for_enter() -> io::Result<()> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn create_uses_the_background_node_and_socket_client() {
        let source = include_str!("cli.rs");
        let create_arm = source
            .split_once("Some(Command::Create { name, session_name }) => {")
            .expect("create arm")
            .1
            .split_once("Some(Command::Join { ticket, name }) => {")
            .expect("join arm")
            .0;
        assert!(create_arm.contains("launch_background_node("));
        assert!(create_arm.contains("crate::client::run(&descriptor)"));
    }
}
