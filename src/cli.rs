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
    hosted_rendezvous::{HostedRendezvous, JoinCode},
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
    version,
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
    /// Join a shared session with a join code or a reusable shared-session ticket.
    Join {
        /// The short code from the host's Ctrl+S panel, or the full ticket.
        ticket: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Pair this machine with another one you own, once and permanently.
    ///
    /// With no code, prints one for the other machine to type. With a code,
    /// pairs with the machine that printed it. After either, bare `p2pmux`
    /// rejoins on both with no code typed again.
    Pair {
        /// The pairing code printed by `p2pmux pair` on your other machine.
        code: Option<String>,
        /// Answer the accepts-work question without being asked. Only recorded;
        /// nothing acts on it yet.
        #[arg(long = "accept-work")]
        accept_work: bool,
        /// Refuse the accepts-work question without being asked.
        #[arg(long = "no-accept-work", conflicts_with = "accept_work")]
        no_accept_work: bool,
    },
    /// List the machines paired with this one.
    Machines,
    /// Forget a paired machine.
    Unpair {
        /// The machine's name, as listed by `p2pmux machines`.
        name: String,
    },
    /// List the live sessions on this machine.
    Ls,
    /// Print the full reusable join ticket for a session hosted on this machine.
    Ticket {
        /// The memorable session name, as listed by `p2pmux ls`. Omit it when one session
        /// is hosted here.
        session: Option<String>,
    },
    /// Print the short join code for a session hosted on this machine.
    Code {
        /// The memorable session name, as listed by `p2pmux ls`. Omit it when one session
        /// is hosted here.
        session: Option<String>,
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
    /// Report agent status from an agent hook. Run inside a p2pmux pane; a
    /// no-op anywhere else, so it is safe to leave registered everywhere.
    Notify {
        #[command(subcommand)]
        agent: NotifyAgent,
    },
    /// Wire an agent's hooks up so its real state reaches the inbox.
    ///
    /// With no agent named, wires up every agent p2pmux knows how to. That is
    /// the form the inbox's own nudge tells people to run, and a user who has
    /// just been told their agents are unreported should not then have to pick
    /// which of them to fix.
    Setup {
        #[command(subcommand)]
        agent: Option<SetupAgent>,
    },
    /// Report whether each agent's hooks are wired up.
    Doctor,
    #[command(name = "__node", hide = true)]
    Node {
        #[arg(long)]
        bootstrap: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum SetupAgent {
    /// Claude Code, via marker-owned entries in ~/.claude/settings.json.
    Claude {
        /// Remove the hooks p2pmux installed, leaving your own alone.
        #[arg(long)]
        uninstall: bool,
        /// Say what would change without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// OpenCode, via a plugin at ~/.config/opencode/plugin/p2pmux.js.
    #[command(name = "opencode")]
    OpenCode {
        /// Delete the plugin p2pmux installed.
        #[arg(long)]
        uninstall: bool,
        /// Say what would change without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Set { key: String, value: String },
    Get { key: String },
    Init,
}

#[derive(Debug, Subcommand)]
enum NotifyAgent {
    /// Read a Claude Code hook payload on stdin.
    Claude {
        /// The status this hook reports, when the registration knows it
        /// (`running`, `pending`, `done`, `idle`, `error`). Without it the
        /// payload's own `hook_event_name` decides.
        #[arg(long)]
        status: Option<String>,
    },
    /// Read an OpenCode plugin payload on stdin.
    #[command(name = "opencode")]
    OpenCode {
        /// The status this event reports (`running`, `pending`, `done`,
        /// `idle`, `error`). Without it the payload's own `event` decides.
        #[arg(long)]
        status: Option<String>,
    },
}

const TRUST_WARNING: &str = "TRUST WARNING: This is a fully trusted shared-shell session. Anyone with the join ticket can see every pane and may obtain interactive control of available terminals (run commands, see output, touch files reachable to that user account).

Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege user account and avoid production credentials in shared panes.

Processes and credential files stay on the pane host's machine (not uploaded to peers). That does not stop a controller from using or displaying them via the shared shell.";

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

/// Parse process arguments.
pub fn parse() -> Cli {
    Cli::parse()
}

/// Run the commands that must not pay for an async runtime, returning `None`
/// for everything else.
///
/// `notify` is spawned by an agent hook, which fires on every tool call. It
/// writes one line to a unix socket and exits; standing up a multi-threaded
/// Tokio runtime — a thread per core, created and torn down — to do that would
/// cost more than the work, on the agent's own critical path.
pub fn run_without_runtime(cli: &Cli) -> Option<Result<(), Box<dyn Error>>> {
    match &cli.command {
        Some(Command::Notify { agent }) => Some(match agent {
            NotifyAgent::Claude { status } => {
                crate::agent_notify::run(crate::agent_detect::AgentKind::Claude, status.as_deref())
            }
            NotifyAgent::OpenCode { status } => crate::agent_notify::run(
                crate::agent_detect::AgentKind::OpenCode,
                status.as_deref(),
            ),
        }),
        // Reading the finder records is a directory scan; it needs no runtime either.
        Some(Command::Ls) => Some(print_sessions()),
        // Editing a settings file and reporting on it are both plain filesystem
        // work, and a user running them wants an answer, not a thread pool.
        Some(Command::Setup { agent }) => Some(match agent {
            Some(SetupAgent::Claude { uninstall, dry_run }) => {
                crate::agent_setup::setup_claude(*uninstall, *dry_run)
            }
            Some(SetupAgent::OpenCode { uninstall, dry_run }) => {
                crate::agent_setup::setup_opencode(*uninstall, *dry_run)
            }
            // Every agent with a hook surface. Naming one is the exception —
            // the sentence the inbox shows a first-time user is the bare
            // command, and it has to leave that machine fully wired.
            None => crate::agent_setup::setup_all(false, false),
        }),
        Some(Command::Doctor) => Some(crate::agent_setup::doctor()),
        _ => None,
    }
}

/// The live sessions on this machine, as `ticket`, `code`, `attach`, `kill` and `rename` name them.
///
/// Those five commands all take a session name and every one of them documented `p2pmux ls` as
/// where to read it, so this is the command that makes the rest addressable. It prints nothing
/// but a header when there are none, rather than failing: "no sessions" is a legitimate answer
/// to a listing, unlike to `p2pmux code`.
fn print_sessions() -> Result<(), Box<dyn Error>> {
    let sessions = crate::session_store::SessionStore::for_current_user()?.list_live()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let width = sessions
        .iter()
        .map(|session| session.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "{:<width$}  {:<11}  {:<11}  UP",
        "NAME", "ROLE", "CODE"
    )?;
    for session in &sessions {
        let role = match session.role {
            crate::session_store::SessionRole::Coordinator => "coordinator",
            crate::session_store::SessionRole::Member => "member",
        };
        // A member never mints a code, and a coordinator that started while the rendezvous was
        // unreachable has only a ticket. Both are ordinary states, not errors to report here.
        let code = session.join_code.as_deref().unwrap_or("-");
        writeln!(
            stdout,
            "{:<width$}  {role:<11}  {code:<11}  {}",
            session.name,
            format_uptime(now.saturating_sub(session.created_at))
        )?;
    }
    Ok(())
}

/// Elapsed time a person can read at a glance, rather than the raw epoch the picker shows.
fn format_uptime(seconds: u64) -> String {
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h{:02}m", minutes % 60);
    }
    format!("{}d{:02}h", hours / 24, hours % 24)
}

pub async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    if let Some(result) = run_without_runtime(&cli) {
        return result;
    }
    if cli.resume {
        return resume_picker(true);
    }
    match cli.command {
        None => open_home().await,
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
        // Already handled by the `run_without_runtime` call above; reaching
        // here would mean the two dispatches disagreed.
        Some(Command::Notify { .. })
        | Some(Command::Ls)
        | Some(Command::Setup { .. })
        | Some(Command::Doctor) => Ok(()),
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
            let ticket = host.ticket().to_string();
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "Join with: p2pmux join {ticket}")?;
                writeln!(
                    stdout,
                    "The ticket stays available behind Ctrl+S after you continue."
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
                    ticket,
                    None,
                    handle,
                )
                .map_err(|error| error.to_string())?;
                runtime.set_session_id(session_id);
                runtime.run().map_err(|error| error.to_string())
            })
            .await?;
            dispatcher_task.abort();
            let _ = dispatcher_task.await;
            result.map_err(io::Error::other)?;
            Ok(())
        }
        Some(Command::Join { ticket, name }) => {
            {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{TRUST_WARNING}\n")?;
                stdout.flush()?;
            }
            let ticket = resolve_join_ticket(&ticket).await?;
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
        Some(Command::Pair {
            code,
            accept_work,
            no_accept_work,
        }) => {
            let answer = accepts_work_answer(accept_work, no_accept_work)?;
            match code {
                Some(code) => pair_with_code(&code, answer).await,
                None => offer_pairing(answer).await,
            }
        }
        Some(Command::Machines) => print_machines(),
        Some(Command::Unpair { name }) => {
            let mut pairing = crate::pairing::load()?;
            if !pairing.forget(&name) {
                return Err(CliError("no paired machine with that name").into());
            }
            crate::pairing::save(&pairing)?;
            println!("unpaired: {name}");
            Ok(())
        }
        Some(Command::Ticket { session }) => print_join_ticket(session),
        Some(Command::Code { session }) => print_join_code(session),
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

/// The accepts-work answer, asked once during pairing rather than left to a
/// separate configuration step nobody would find.
///
/// Default-deny, and the wording matters: it means *accepts work from me*,
/// never *from anyone in the session*. Otherwise a join code you hand to a
/// colleague becomes remote code execution on your desktop. Nothing acts on the
/// flag yet — it is only recorded, and it is the consent primitive that will
/// later make starting a terminal on another machine legal without widening the
/// trust model.
fn accepts_work_answer(accept: bool, refuse: bool) -> Result<bool, Box<dyn Error>> {
    if accept {
        return Ok(true);
    }
    if refuse || !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("Let your other machines start work here? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `p2pmux pair` with no code: print one for the other machine to type.
///
/// Pairing needs a session to be about, so this makes one if none is live here.
/// The code is the existing short-code-to-ticket mechanism, unchanged — pairing
/// is mostly persistence plus auto-join on start.
async fn offer_pairing(accepts_work: bool) -> Result<(), Box<dyn Error>> {
    {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{TRUST_WARNING}\n")?;
        stdout.flush()?;
    }
    let store = crate::session_store::SessionStore::for_current_user()?;
    let descriptor = match newest_live(&store.list_live()?) {
        Some(descriptor) => descriptor,
        None => start_solo_session()?,
    };
    // The node publishes the code a moment after it starts, so a session
    // created two lines ago has not necessarily got one yet.
    let descriptor = wait_for_invite(&store, &descriptor.id).await?;
    let ticket = descriptor.ticket.clone().ok_or(CliError(
        "this machine is not hosting the session; pair from the machine that is",
    ))?;
    let mut pairing = crate::pairing::load()?;
    pairing.ticket = Some(ticket.clone());
    pairing.accepts_work = accepts_work;
    crate::pairing::save(&pairing)?;

    let mut stdout = io::stdout().lock();
    match descriptor.join_code.as_deref() {
        Some(code) => {
            writeln!(stdout, "pairing code: {code}")?;
            writeln!(
                stdout,
                "\nOn your other machine, run:\n  p2pmux pair {code}"
            )?;
        }
        // A rendezvous outage costs the short code, not the pairing: the ticket
        // is the real address and works without the service.
        None => {
            writeln!(
                stdout,
                "the rendezvous service was unreachable, so there is no short code."
            )?;
            writeln!(
                stdout,
                "\nOn your other machine, run:\n  p2pmux pair {ticket}"
            )?;
        }
    }
    writeln!(
        stdout,
        "\naccepts work from your machines: {}",
        if accepts_work { "yes" } else { "no" }
    )?;
    Ok(())
}

/// `p2pmux pair <code>`: join the machine that printed it, permanently.
async fn pair_with_code(code: &str, accepts_work: bool) -> Result<(), Box<dyn Error>> {
    {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{TRUST_WARNING}\n")?;
        stdout.flush()?;
    }
    let ticket = resolve_join_ticket(code).await?;
    let ticket_text = ticket.to_string();
    // Recorded before the join rather than after it. A join that connects and
    // then drops has still paired the machines, and losing the ticket to a
    // network blip would mean typing the code again for a pairing that
    // succeeded.
    let mut pairing = crate::pairing::load()?;
    pairing.ticket = Some(ticket_text.clone());
    pairing.accepts_work = accepts_work;
    crate::pairing::save(&pairing)?;

    let descriptor = rejoin_paired_session(&ticket_text).await?;
    let peers = wait_for_peers(&descriptor).await;
    let mut pairing = crate::pairing::load()?;
    for peer in &peers {
        pairing.remember(peer, None);
    }
    crate::pairing::save(&pairing)?;

    let mut stdout = io::stdout().lock();
    if peers.is_empty() {
        // The session answered — the ticket resolved and the node started — but
        // no member has been seen yet. Say what is true rather than claiming a
        // machine by a name nobody sent.
        writeln!(
            stdout,
            "paired, but the other machine has not answered yet."
        )?;
        writeln!(stdout, "Run `p2pmux machines` once it is awake.")?;
    } else {
        for peer in peers {
            writeln!(stdout, "paired: {peer}")?;
        }
    }
    writeln!(stdout, "\nFrom now on, bare `p2pmux` rejoins with no code.")?;
    Ok(())
}

/// Wait for the node to publish the session's invite material.
async fn wait_for_invite(
    store: &crate::session_store::SessionStore,
    id: &str,
) -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    const INVITE_TIMEOUT: Duration = Duration::from_secs(20);
    let deadline = Instant::now() + INVITE_TIMEOUT;
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(descriptor) = store
            .list_live()?
            .into_iter()
            .find(|descriptor| descriptor.id == id)
        {
            if descriptor.ticket.is_some() && descriptor.join_code.is_some() {
                return Ok(descriptor);
            }
            last = Some(descriptor);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A ticket with no code still pairs, so a rendezvous outage falls through
    // here rather than failing.
    last.ok_or_else(|| CliError("the session did not start").into())
}

/// The other machines in the session this node just joined.
///
/// Bounded and best-effort: the pairing is already recorded, so an empty answer
/// costs a name in the printout and nothing else.
async fn wait_for_peers(descriptor: &crate::session_store::SessionDescriptor) -> Vec<String> {
    const PEER_TIMEOUT: Duration = Duration::from_secs(15);
    let deadline = Instant::now() + PEER_TIMEOUT;
    while Instant::now() < deadline {
        let peers = crate::pairing::load_or_empty().machines;
        if !peers.is_empty() {
            return peers.into_iter().map(|machine| machine.name).collect();
        }
        // A node that died takes the pairing's chance of learning a name with
        // it, so stop waiting for an answer that is not coming.
        if !crate::session_store::SessionStore::for_current_user()
            .and_then(|store| store.list_live())
            .is_ok_and(|live| live.iter().any(|session| session.id == descriptor.id))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Vec::new()
}

/// `p2pmux machines`: the fleet, and whether each part of it is answering.
///
/// Reachability comes from the live session records, which the node keeps up to
/// date out of process. A machine paired but not in any of them is one you own
/// that is not answering — off, asleep, or without a node running — and it
/// keeps its row rather than vanishing. Saying `asleep` is the whole point.
/// This machine is one of them, so the list is never empty. Printing "no
/// machines" on the machine being asked was answering a different question than
/// the one the command asks; the pairing nudge is still worth saying, but as a
/// line under the fleet rather than instead of it.
fn print_machines() -> Result<(), Box<dyn Error>> {
    let rows = machine_rows()?;
    println!(
        "{:<12} {:<8} {:<14} RUNNING",
        "NAME", "STATUS", "ACCEPTS WORK"
    );
    for row in &rows {
        println!("{}", crate::tui::machine_line(row));
    }
    if rows.len() < 2 {
        println!();
        println!("No other machines paired yet. Run `p2pmux pair` to add one.");
    }
    Ok(())
}

/// Every machine this one knows about, session members first.
///
/// Shared with the `m` key on Home, so the two can never drift into describing
/// the same fleet differently.
fn machine_rows() -> Result<Vec<crate::tui::MachineRow>, Box<dyn Error>> {
    let pairing = crate::pairing::load()?;
    let live = crate::session_store::SessionStore::for_current_user()?.list_live()?;
    let here = crate::config::load()?.unwrap_or_else(hostname_label);
    let accepts_work = |name: &str| {
        pairing
            .machines
            .iter()
            .find(|machine| machine.name == name)
            .and_then(|machine| machine.accepts_work)
    };

    let mut rows = vec![crate::tui::MachineRow {
        agents: live
            .iter()
            .flat_map(|session| session.peers.iter())
            .filter(|peer| peer.this_machine)
            .map(|peer| peer.agents)
            .sum(),
        name: here.clone(),
        reachable: true,
        // The one row whose answer is genuinely known here: it was given on
        // this machine, about this machine.
        accepts_work: Some(pairing.accepts_work),
        this_machine: true,
    }];
    for peer in live
        .iter()
        .flat_map(|session| session.peers.iter())
        .filter(|peer| !peer.this_machine)
    {
        if rows.iter().any(|row| row.name == peer.name) {
            continue;
        }
        rows.push(crate::tui::MachineRow {
            name: peer.name.clone(),
            reachable: true,
            accepts_work: accepts_work(&peer.name),
            agents: peer.agents,
            this_machine: false,
        });
    }
    for machine in &pairing.machines {
        if rows.iter().any(|row| row.name == machine.name) {
            continue;
        }
        rows.push(crate::tui::MachineRow {
            name: machine.name.clone(),
            reachable: false,
            accepts_work: machine.accepts_work,
            agents: 0,
            this_machine: false,
        });
    }
    Ok(rows)
}

/// What bare `p2pmux` does: open the inbox.
///
/// Not a session picker. The question the command answers is "which of my
/// agents needs me", and making someone choose a session first is asking them
/// to answer a question they did not have in order to be shown the one they
/// did. `--resume` still reaches the picker for anyone who wants it.
///
/// Three cases, in order, and all of them end on Home:
///
/// 1. A session is already live here — attach it.
/// 2. This machine is paired — rejoin the session the pairing recorded, with
///    no code typed. That is the whole point of pairing once.
/// 3. Nothing yet — start a solo session, so a first run has a real terminal
///    behind `n` rather than an empty screen and an error.
async fn open_home() -> Result<(), Box<dyn Error>> {
    let store = crate::session_store::SessionStore::for_current_user()?;
    if let Some(descriptor) = newest_live(&store.list_live()?) {
        return crate::client::run_on(&descriptor, crate::client::StartScreen::Home);
    }
    let pairing = crate::pairing::load_or_empty();
    if let Some(ticket) = pairing.ticket.as_deref() {
        match rejoin_paired_session(ticket).await {
            Ok(descriptor) => {
                return crate::client::run_on(&descriptor, crate::client::StartScreen::Home);
            }
            // A paired machine that is asleep, or a session whose coordinator
            // is gone, must not leave the user with nothing. Say so and open a
            // session here — the inbox still has this machine's agents on it.
            Err(error) => {
                let mut stderr = io::stderr().lock();
                writeln!(stderr, "could not rejoin the paired session: {error}")?;
                writeln!(stderr, "starting a session on this machine instead")?;
            }
        }
    }
    let descriptor = start_solo_session()?;
    crate::client::run_on(&descriptor, crate::client::StartScreen::Home)
}

/// The most recently created live session, which is the one a bare command
/// means. Deterministic rather than "the first one the store listed".
fn newest_live(
    sessions: &[crate::session_store::SessionDescriptor],
) -> Option<crate::session_store::SessionDescriptor> {
    sessions
        .iter()
        .max_by_key(|session| session.created_at)
        .cloned()
}

async fn rejoin_paired_session(
    ticket: &str,
) -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    let ticket = resolve_join_ticket(ticket).await?;
    let display_name = display_name_or_hostname()?;
    let (cols, rows) = terminal_size_or_default();
    launch_background_node(
        crate::node::NodeBootstrapKind::Join {
            ticket: ticket.to_string(),
            display_name,
            cols,
            rows,
        },
        crate::session_store::generate_name()?,
        crate::session_store::SessionRole::Member,
    )
}

/// A session with only this machine in it.
///
/// No trust warning: nothing is shared until the user hands out a code, and
/// `create` and `pair` — the two commands that do that — both print it.
fn start_solo_session() -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    let display_name = display_name_or_hostname()?;
    let (cols, rows) = terminal_size_or_default();
    launch_background_node(
        crate::node::NodeBootstrapKind::Create {
            display_name,
            cols,
            rows,
        },
        crate::session_store::generate_name()?,
        crate::session_store::SessionRole::Coordinator,
    )
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
    let mut child = command.spawn()?;
    // A `join` cannot report anything until it has dialled the coordinator, and iroh
    // gives that dial about thirty seconds before it gives up. Waiting five and then
    // printing a local-startup message meant the real cause -- "transport error: Iroh
    // connect timed out", which the node does write, twenty-five seconds later -- was
    // never the thing the user read. The cap is now past that dial, and the loop leaves
    // early the moment the node resolves either way, so a session that starts normally
    // is not slowed by it.
    let deadline = Instant::now() + Duration::from_secs(60);
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
        // A node that has exited is never going to become ready, and waiting out the
        // rest of the cap for it would turn a fast failure into a slow one. It writes
        // its reason before exiting, so re-read once after reaping to avoid losing a
        // message that landed between the check above and the exit.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err(match take_node_error() {
                Some(message) => NodeStartupError(message).into(),
                None => Box::<dyn Error>::from(io::Error::other(
                    "the background node exited before the session started",
                )),
            });
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

/// The display name to use without ever asking.
///
/// Bare `p2pmux` is meant to be install-to-value in under a minute, and a name
/// prompt is a question before the first answer. The machine's own hostname is
/// both a good default and the right one: the inbox shows this column as the
/// *machine* an agent is on, so `desktop` and `droplet` are exactly what a user
/// would have typed anyway. A name typed later with `p2pmux config set name`
/// still wins, and so does one already saved.
fn display_name_or_hostname() -> Result<String, Box<dyn Error>> {
    if let Some(name) = crate::config::load()? {
        return Ok(name);
    }
    let hostname = hostname_label();
    // Saved rather than used once, so the machine keeps the same name across
    // restarts and peers do not watch it rename itself.
    Ok(crate::config::save(&hostname)?)
}

/// The terminal's size, or a default when there is no terminal to ask.
///
/// `pair` starts a session and never draws anything, so it has to work from a
/// script and over a plain ssh command with no PTY. The first client to attach
/// resizes the panes to its own window anyway, which makes this only the size
/// the first shell is born at.
fn terminal_size_or_default() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// The machine's short hostname, cleaned up enough to be a display name.
///
/// `laptop.local` is `laptop`: the mDNS suffix is noise in a column that has
/// ten characters to spend. Falls back to a fixed word rather than failing —
/// a nameless machine should still get an inbox.
fn hostname_label() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();
    let trimmed = raw.trim().split('.').next().unwrap_or("").trim();
    let cleaned = trimmed
        .chars()
        .filter(|character| !character.is_control())
        .take(32)
        .collect::<String>();
    if cleaned.is_empty() {
        String::from("this-machine")
    } else {
        cleaned
    }
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
    // This prompt is the first thing a new install shows. Ending the whole command on a
    // stray Enter -- the single likeliest thing to happen here -- means retyping `p2pmux
    // create` and reading the trust warning again to recover from a keystroke, so ask
    // again instead. Bounded, so a terminal feeding an endless stream of blank lines
    // cannot spin here forever.
    const PROMPT_ATTEMPTS: usize = 3;
    for attempt in 1..=PROMPT_ATTEMPTS {
        {
            let mut stdout = io::stdout().lock();
            write!(stdout, "Choose a display name (visible to session peers): ")?;
            stdout.flush()?;
        }
        let mut name = String::new();
        // Zero bytes is end of input, not a short name: the pipe is closed and asking
        // again would read zero bytes forever.
        if io::stdin().read_line(&mut name)? == 0 {
            return Err(CliError("no display name given").into());
        }
        match crate::config::save(&name) {
            Ok(saved) => return Ok(saved),
            Err(error) if attempt < PROMPT_ATTEMPTS => {
                let mut stderr = io::stderr().lock();
                writeln!(stderr, "{error}")?;
                stderr.flush()?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("the loop returns on its last attempt either way")
}

/// Turn whatever the user pasted into a dialable ticket.
///
/// A ticket is self-contained and resolves offline; a short code has to be exchanged for one
/// at the rendezvous service. The two are told apart by shape, and the check is ordered
/// ticket-first so a working invite never depends on a network round trip it does not need.
async fn resolve_join_ticket(input: &str) -> Result<JoinTicket, Box<dyn Error>> {
    if looks_like_ticket(input) {
        return Ok(JoinTicket::from_str(input).map_err(|_| CliError("invalid ticket format"))?);
    }
    // Points at the host rather than describing the two shapes: whoever hits this has been
    // sent something malformed, and the fix is a fresh invite, not a lesson in what a code
    // looks like.
    let code = JoinCode::parse(input).map_err(|_| {
        CliError("that is not a join code; ask the host for the line their Ctrl+S panel shows")
    })?;
    let ticket = HostedRendezvous::new()?.resolve(&code).await?;
    Ok(JoinTicket::from_str(&ticket).map_err(|_| CliError("invalid ticket format"))?)
}

/// Print the portable join ticket for a session hosted on this machine.
///
/// Read back off the session record the coordinator's node wrote, and printed to stdout alone
/// so `p2pmux ticket | pbcopy` yields something directly pasteable.
fn print_join_ticket(session: Option<String>) -> Result<(), Box<dyn Error>> {
    let descriptor = hosted_session(session)?;
    let ticket = descriptor.ticket.ok_or(CliError(
        "that session was joined, not created here, so this machine holds no ticket for it",
    ))?;
    print_invite(&ticket)
}

/// Print the short join code for a session hosted on this machine.
///
/// The code needs the rendezvous service to have accepted it, so unlike the ticket this can
/// legitimately not exist for a live session — say which of the two it is rather than making
/// the caller guess.
fn print_join_code(session: Option<String>) -> Result<(), Box<dyn Error>> {
    let descriptor = hosted_session(session)?;
    if descriptor.ticket.is_none() {
        return Err(CliError(
            "that session was joined, not created here, so this machine holds no code for it",
        )
        .into());
    }
    let code = descriptor.join_code.ok_or(CliError(
        "that session has no code; the rendezvous service was unreachable when it started, so share the ticket instead",
    ))?;
    print_invite(&code)
}

/// stdout carries the invite alone; the warning goes to stderr so a pipe stays clean.
fn print_invite(invite: &str) -> Result<(), Box<dyn Error>> {
    {
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "{TICKET_WARNING}\n")?;
        stderr.flush()?;
    }
    println!("{invite}");
    Ok(())
}

fn hosted_session(
    session: Option<String>,
) -> Result<crate::session_store::SessionDescriptor, Box<dyn Error>> {
    let store = crate::session_store::SessionStore::for_current_user()?;
    let sessions = store.list_live()?;
    Ok(match session {
        Some(name) => sessions
            .into_iter()
            .find(|session| session.name == name)
            .ok_or(CliError("no live session by that name on this machine"))?,
        None => sole_hosted_session(sessions)?,
    })
}

/// The one session hosted here, when leaving the name off is unambiguous.
///
/// Only coordinators hold a ticket, so sessions joined from elsewhere are not candidates and
/// never make the choice ambiguous.
fn sole_hosted_session(
    sessions: Vec<crate::session_store::SessionDescriptor>,
) -> Result<crate::session_store::SessionDescriptor, CliError> {
    let mut hosted: Vec<_> = sessions
        .into_iter()
        .filter(|session| session.ticket.is_some())
        .collect();
    match hosted.len() {
        0 => Err(CliError(
            "no session was created on this machine; run p2pmux create first",
        )),
        1 => Ok(hosted.remove(0)),
        _ => Err(CliError(
            "several sessions are hosted on this machine; pass the session name, as listed by p2pmux ls",
        )),
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
