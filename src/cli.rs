//! Command-line parsing and live session dispatch.

use std::{
    error::Error,
    io::{self, IsTerminal, Write},
    path::PathBuf,
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
        /// Answer the accepts-work question without being asked. The first of
        /// two gates; `p2pmux work allow` here opens the second.
        #[arg(long = "accept-work")]
        accept_work: bool,
        /// Refuse the accepts-work question without being asked.
        #[arg(long = "no-accept-work", conflicts_with = "accept_work")]
        no_accept_work: bool,
    },
    /// List the machines paired with this one.
    Machines,
    /// Keep this machine in its fleet, so it is there when you start a session
    /// somewhere else.
    ///
    /// With no subcommand, runs in the foreground — which is what the installed
    /// service does. `install` is how it comes back after a reboot.
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// Forget a paired machine.
    Unpair {
        /// The machine's name, as listed by `p2pmux machines`.
        name: String,
    },
    /// List the live sessions on this machine.
    ///
    /// `ls` is kept as an alias: it is what every release so far has answered
    /// to, and it is written down in scripts and muscle memory this rename has
    /// no business breaking.
    #[command(alias = "ls")]
    List,
    /// Print the full reusable join ticket for a session hosted on this machine.
    Ticket {
        /// The memorable session name, as listed by `p2pmux list`. Omit it when one session
        /// is hosted here.
        session: Option<String>,
    },
    /// Print the short join code for a session hosted on this machine.
    Code {
        /// The memorable session name, as listed by `p2pmux list`. Omit it when one session
        /// is hosted here.
        session: Option<String>,
    },
    /// Go back to a session already running on this machine.
    ///
    /// Bare `p2pmux` starts somewhere new; this is how you return to somewhere
    /// old. With no name it takes the session started most recently here, which
    /// is what "put me back" means when only one is running.
    Attach {
        /// The memorable session name, as listed by `p2pmux list`. Omit it to
        /// take the most recently started one.
        name: Option<String>,
    },
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
    /// Put a machine you own in your fleet without anybody sitting at it.
    ///
    /// `p2pmux pair` is a code one human types on one machine within ten
    /// minutes, which is right for two laptops and unusable from a provisioning
    /// script. With no arguments this prints a token to paste into one; on the
    /// new machine, `p2pmux enroll <token>` joins the fleet unattended.
    Enroll {
        /// The token printed by `p2pmux enroll` on a machine already in the
        /// fleet. Omit it to print one here instead.
        token: Option<String>,
        /// The name this machine goes by in the fleet. Defaults to its
        /// hostname, which on a droplet is rarely what you want to read.
        #[arg(long)]
        name: Option<String>,
        /// Withdraw the standing invitation. Machines already enrolled stay;
        /// `p2pmux unpair` is how one leaves.
        #[arg(long)]
        revoke: bool,
        /// Let your other machines start a login shell here. Unattended boxes
        /// usually want this, and it is the same thing `p2pmux work allow` does.
        #[arg(long = "accept-work")]
        accept_work: bool,
    },
    /// Say what your other machines may start on this one.
    ///
    /// Two gates gate a remote terminal, and both are closed until you open
    /// them: this machine has to accept work at all, and the exact command has
    /// to be on its allowlist. With no arguments this prints where both stand.
    Work {
        #[command(subcommand)]
        action: Option<WorkCommand>,
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
enum DaemonCommand {
    /// Start the fleet agent at boot and keep it running.
    Install,
    /// Stop it and remove the service.
    Uninstall,
    /// Say whether it is installed, and where.
    Status,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Set { key: String, value: String },
    Get { key: String },
    Init,
}

#[derive(Debug, Subcommand)]
enum WorkCommand {
    /// Let your other machines start this command here.
    ///
    /// Written and matched in full, so `claude` and
    /// `claude --dangerously-skip-permissions` are separate decisions. With no
    /// command it allows a login shell, which is everything this user account
    /// can do — say `p2pmux work allow` only if you mean that.
    ///
    /// Granting anything is itself the consent to accept work, so this turns
    /// that on too rather than leaving you with an allowlist nothing reads.
    Allow {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Stop letting them start it. With no command, a login shell.
    Deny {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Ask on this machine before each remote pane, even an allowed one.
    Confirm {
        /// Stop asking, and let allowed commands through unattended.
        #[arg(long)]
        off: bool,
    },
    /// Refuse every remote pane, keeping the allowlist for when you turn it
    /// back on.
    Off,
    /// Accept remote panes again, subject to the allowlist.
    On,
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
        Some(Command::List) => Some(print_sessions()),
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
/// Those five commands all take a session name and every one of them documented `p2pmux list` as
/// where to read it, so this is the command that makes the rest addressable. It prints nothing
/// but a header when there are none, rather than failing: "no sessions" is a legitimate answer
/// to a listing, unlike to `p2pmux code`.
fn print_sessions() -> Result<(), Box<dyn Error>> {
    let sessions = crate::session_store::SessionStore::for_current_user()?.list_live()?;
    // A header with nothing under it is the one answer this command can give
    // that a person cannot act on: it looks the same as a listing that failed.
    // Every other empty list in p2pmux says what it means and what starts one,
    // and so does this.
    if sessions.is_empty() {
        println!("No sessions running on this machine. `p2pmux` starts one.");
        return Ok(());
    }
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
            // This process has no terminal, so a failure would otherwise be invisible and
            // the launcher could only report that the socket never appeared. Leave the
            // reason where it can find it -- and where the client can, long after the
            // launcher stopped waiting: a node that dies an hour into a session used to
            // leave nothing at all behind, which made "p2pmux node ended" the whole of
            // what anybody could know about it.
            let result = match crate::node::read_bootstrap(&bootstrap) {
                Ok(parsed) => crate::node::run_background(parsed).await,
                Err(error) => Err(error.into()),
            };
            match &result {
                Err(error) => {
                    let _ = std::fs::write(bootstrap.with_extension("error"), error.to_string());
                }
                // Nothing went wrong, so the log is a temporary file nobody will
                // ever read. A node that panicked runs neither arm and leaves its
                // log where the client will look.
                Ok(()) => {
                    let _ = std::fs::remove_file(bootstrap.with_extension("log"));
                }
            }
            result
        }
        // Already handled by the `run_without_runtime` call above; reaching
        // here would mean the two dispatches disagreed.
        Some(Command::Notify { .. })
        | Some(Command::List)
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
        Some(Command::Work { action }) => run_work(action),
        Some(Command::Enroll {
            token,
            name,
            revoke,
            accept_work,
        }) => match (token, revoke) {
            (_, true) => {
                let mut pairing = crate::pairing::load()?;
                if crate::pairing::revoke_enrolment(&mut pairing) {
                    crate::pairing::save(&pairing)?;
                    println!("enrolment token revoked; machines already in the fleet stay");
                } else {
                    println!("no enrolment token to revoke");
                }
                Ok(())
            }
            (None, false) => print_enrolment_token(),
            (Some(token), false) => enroll_with_token(&token, name.as_deref(), accept_work).await,
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
        Some(Command::Daemon { command }) => match command {
            None => crate::daemon::run().await,
            Some(DaemonCommand::Install) => {
                let path = crate::daemon::install()?;
                println!("fleet agent installed: {}", path.display());
                // What it will actually do depends on whether there is a fleet
                // to keep this machine in. Saying "rejoins its home session at
                // boot" to a machine that has never been paired is a promise
                // about a session that does not exist.
                if crate::pairing::load_or_empty().can_rejoin() {
                    println!(
                        "This machine now rejoins its home session at boot, so your other\n\
                         machines can find it — and invite it into sessions you start later."
                    );
                } else {
                    println!(
                        "This machine is not in a fleet yet, so the agent is running and\n\
                         waiting. Pair it — `p2pmux pair` here, `p2pmux pair <code>` on\n\
                         another machine you own — and it starts keeping this one in that\n\
                         fleet, at boot and after a crash, without being installed again."
                    );
                }
                Ok(())
            }
            Some(DaemonCommand::Uninstall) => {
                match crate::daemon::uninstall()? {
                    Some(path) => println!("fleet agent removed: {}", path.display()),
                    None => println!("no fleet agent was installed"),
                }
                Ok(())
            }
            Some(DaemonCommand::Status) => {
                let path = crate::daemon::unit_path()?;
                if crate::daemon::installed() {
                    println!("fleet agent installed: {}", path.display());
                } else {
                    println!("no fleet agent installed");
                    println!("Run `p2pmux daemon install` to keep this machine in its fleet.");
                }
                Ok(())
            }
        },
        Some(Command::Unpair { name }) => {
            let mut pairing = crate::pairing::load()?;
            if !pairing.forget(&name) {
                return Err(CliError("no paired machine with that name").into());
            }
            crate::pairing::save(&pairing)?;
            println!("unpaired: {name}");
            // The last one takes the fleet's session with it, so say so here
            // rather than leaving the change to be discovered as a bare
            // `p2pmux` that no longer waits for anybody.
            if pairing.machines.is_empty() {
                println!("nothing is paired with this machine now; `p2pmux` starts a session here");
            }
            Ok(())
        }
        Some(Command::Ticket { session }) => print_join_ticket(session),
        Some(Command::Code { session }) => print_join_code(session),
        Some(Command::Attach { name }) => crate::client::run(&match name {
            Some(name) => find_live(&name)?,
            None => {
                newest_live(&crate::session_store::SessionStore::for_current_user()?.list_live()?)
                    .ok_or(CliError(
                    "no session is running on this machine; `p2pmux` starts one",
                ))?
            }
        }),
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
/// colleague becomes remote code execution on your desktop. Saying yes here is
/// consent to be asked; what may actually be started is the allowlist in the
/// pairing file, which is empty until somebody writes in it.
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

/// Offer to keep this machine in its fleet across reboots.
///
/// Asked here because this is the moment the user decided this box is part of
/// a fleet, and a machine that is only in it until the next restart is not one.
/// Declining is remembered — see [`crate::pairing::Pairing::daemon_declined`] —
/// so a person who said no once is not asked again by every later pairing.
fn offer_fleet_daemon() -> Result<(), Box<dyn Error>> {
    if crate::daemon::installed() {
        return Ok(());
    }
    let mut pairing = crate::pairing::load()?;
    if pairing.daemon_declined || !io::stdin().is_terminal() {
        return Ok(());
    }
    println!();
    println!(
        "Keep this machine in its fleet after a reboot? Without this it is only\n\
         reachable while a p2pmux is running here by hand."
    );
    print!("Install the fleet agent? [Y/n] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "n" | "N" | "no") {
        pairing.daemon_declined = true;
        crate::pairing::save(&pairing)?;
        println!("Not installed. `p2pmux daemon install` does it later.");
        return Ok(());
    }
    match crate::daemon::install() {
        Ok(path) => println!("fleet agent installed: {}", path.display()),
        // Worth a line, not worth failing the pairing: the machines are paired
        // either way, and this is about what happens after the next reboot.
        Err(error) => println!("could not install the fleet agent: {error}"),
    }
    Ok(())
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
    // This machine's own session, offered to a machine that may never come for
    // it. What that costs when nobody does is `Pairing::rejoin_ticket`'s
    // subject, and it can only tell an offer from an invitation if the offer
    // says so here.
    pairing.offered_here = true;
    // This command returns as soon as it has printed a code, so the node is
    // what will be watching when the other machine arrives. The window is how
    // it knows that arrival was invited: without one, every peer of the session
    // looks the same as the machine you meant to pair with, which is how a
    // guest used to end up in the fleet.
    pairing.open_pairing_window(crate::pairing::now_unix());
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
    drop(stdout);
    offer_fleet_daemon()?;
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
    // Somebody else's session, and the machine hosting it may be asleep rather
    // than absent — so this ticket keeps its thirty seconds even before any
    // machine has answered on it. An offer this machine made earlier does not
    // get to speak for a pairing it has nothing to do with.
    pairing.offered_here = false;
    crate::pairing::save(&pairing)?;

    let descriptor = rejoin_paired_session(&ticket_text).await?;
    let peers = wait_for_peers(&descriptor).await;
    let mut pairing = crate::pairing::load()?;
    for peer in &peers {
        // Recorded by machine id, which that machine proved, rather than by
        // the name it chose for itself. A machine renamed later is still this
        // machine; two machines that pick the same name are still two; and the
        // same machine in a different session, with a different node behind it,
        // is still the one you paired.
        pairing.remember(
            &peer.name,
            (!peer.machine_id.is_empty()).then(|| peer.machine_id.clone()),
            None,
        );
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
            writeln!(stdout, "paired: {}", peer.name)?;
        }
    }
    writeln!(stdout, "\nFrom now on, bare `p2pmux` rejoins with no code.")?;
    drop(stdout);
    // Asked after the pairing is reported, not before: the machines are paired
    // either way, and this question is about what happens after a reboot.
    offer_fleet_daemon()?;
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
async fn wait_for_peers(
    descriptor: &crate::session_store::SessionDescriptor,
) -> Vec<crate::session_store::SessionPeer> {
    const PEER_TIMEOUT: Duration = Duration::from_secs(15);
    let deadline = Instant::now() + PEER_TIMEOUT;
    while Instant::now() < deadline {
        // Read from the live session rather than from the fleet record. The
        // fleet record is what this is about to write, and a machine only
        // reaches it by being paired — so waiting for it to fill up would be
        // waiting for something this function is supposed to cause.
        let peers = crate::session_store::SessionStore::for_current_user()
            .and_then(|store| store.list_live())
            .map(|live| {
                live.into_iter()
                    .filter(|session| session.id == descriptor.id)
                    .flat_map(|session| session.peers.into_iter())
                    // Deliberately not filtered on what the peer says it is.
                    // Somebody typed this machine's code here, on purpose, a
                    // moment ago — that act is the consent, and asking the
                    // other machine to also declare itself would rule out the
                    // commonest case there is: the machine that *printed* the
                    // code, whose node started before it had been paired and so
                    // has nothing to declare yet.
                    .filter(|peer| !peer.this_machine)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !peers.is_empty() {
            return peers;
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
    let pairing = crate::pairing::load()?;
    let (fleet, guests): (Vec<_>, Vec<_>) = rows.iter().partition(|row| row.owned);
    println!(
        "{:<12} {:<8} {:<14} RUNNING",
        "NAME", "STATUS", "ACCEPTS WORK"
    );
    for row in &fleet {
        println!("{}", crate::tui::machine_line(row, None));
    }
    // Under their own heading, never in the fleet table. Someone collaborating
    // on this session from their own laptop is not compute you own, and a list
    // that prints them together is the list that would go on to offer to start
    // a terminal on a stranger's machine.
    if !guests.is_empty() {
        println!();
        println!("IN THIS SESSION, NOT YOURS");
        for row in &guests {
            println!("{}", crate::tui::machine_line(row, None));
        }
    }
    if fleet.len() < 2 {
        println!();
        // An open invitation is not the same state as no invitation, and this
        // line used to call them the same thing. The machine that accepts a
        // code is written down by the node, on its own two-second scan, and not
        // at the instant the code is typed -- so asking straight after pairing
        // reads `No other machines paired yet` about a pairing that is working.
        // That gap was reported as a pairing that never happened (#92), and the
        // right answer to it is a sentence, not a faster timer.
        if pairing.pairing_window_open(crate::pairing::now_unix()) {
            println!(
                "An invitation is open and no machine has come through it yet.\n\
                 One that accepts the code lands here a few seconds later, not the\n\
                 moment it is typed — so if you have just paired, ask again."
            );
        } else {
            println!("No other machines paired yet. Run `p2pmux pair` to add one.");
        }
    }
    // Only about this machine, because this machine's own file is the only
    // policy it can honestly report. Every other row's `ACCEPTS WORK` column
    // already says `—` for the same reason.
    println!();
    print!("{}", work_policy_summary(&pairing));
    Ok(())
}

/// `p2pmux work …` — the only supported way to open the two gates a remote
/// terminal has to pass.
///
/// They were openable before this existed only by hand-editing the pairing
/// file, which meant the feature was reachable by people who had read the
/// source. `p2pmux machines` says what a machine allows; this is how it comes
/// to allow it.
fn run_work(action: Option<WorkCommand>) -> Result<(), Box<dyn Error>> {
    let mut pairing = crate::pairing::load()?;
    let Some(action) = action else {
        print!("{}", work_policy_summary(&pairing));
        return Ok(());
    };
    match action {
        WorkCommand::Allow { command } => {
            let entry = crate::pairing::work_entry(&command);
            let added = pairing.work.allow(&command);
            // Granting a command is the consent; an allowlist behind a closed
            // `accepts_work` is a list nothing reads, and leaving the user with
            // one is how this feature looked broken.
            let opened = !std::mem::replace(&mut pairing.accepts_work, true);
            crate::pairing::save(&pairing)?;
            if added {
                println!("your machines may now start `{entry}` here");
            } else {
                println!("`{entry}` was already allowed here");
            }
            if opened {
                println!("and this machine now accepts work — `p2pmux work off` undoes that");
            }
            if entry == crate::pairing::SHELL_ENTRY {
                println!(
                    "a login shell is everything this user account can do, on purpose and by name"
                );
            }
        }
        WorkCommand::Deny { command } => {
            let entry = crate::pairing::work_entry(&command);
            if pairing.work.deny(&command) {
                crate::pairing::save(&pairing)?;
                println!("your machines may no longer start `{entry}` here");
            } else {
                println!("`{entry}` was not allowed here anyway");
            }
        }
        WorkCommand::Confirm { off } => {
            pairing.work.confirm = !off;
            crate::pairing::save(&pairing)?;
            if off {
                println!("allowed commands will start here without asking");
            } else {
                println!("every remote pane will wait for somebody here to say yes");
            }
        }
        WorkCommand::Off => {
            pairing.accepts_work = false;
            crate::pairing::save(&pairing)?;
            println!("this machine now refuses remote panes; its allowlist is kept");
        }
        WorkCommand::On => {
            pairing.accepts_work = true;
            crate::pairing::save(&pairing)?;
            println!("this machine now accepts remote panes, subject to its allowlist");
        }
    }
    println!();
    print!("{}", work_policy_summary(&crate::pairing::load()?));
    Ok(())
}

/// Print the standing invitation for this fleet, and the line to run with it.
///
/// The line rather than the token alone, because what a person does next is
/// paste it into a cloud-init file, and a token with the command left off is a
/// token that gets pasted into the wrong one.
fn print_enrolment_token() -> Result<(), Box<dyn Error>> {
    let mut pairing = crate::pairing::load()?;
    let Some(ticket) = pairing.ticket.clone() else {
        // Not "or start a session". A session is not a fleet: `ticket` is
        // written by a pairing and nothing else, so telling somebody to run
        // `p2pmux` instead sent them round a loop that could not end. The
        // unattended path really does start with a human pairing once.
        return Err(CliError(
            "this machine is not in a fleet yet, and `p2pmux enroll` hands out an \
             invitation to one that exists — run `p2pmux pair` here and \
             `p2pmux pair <code>` on another machine you own, then try again",
        )
        .into());
    };
    let minted = pairing.enrol.is_none();
    let secret = crate::pairing::enrolment_token(&mut pairing, crate::pairing::now_unix())?;
    if minted {
        crate::pairing::save(&pairing)?;
    }
    let invite = crate::pairing::EnrolInvite { ticket, secret }.encode();
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{TRUST_WARNING}\n")?;
    writeln!(
        stdout,
        "Anyone holding this token can put a machine in your fleet until you run\n\
         `p2pmux enroll --revoke`. Membership on its own starts nothing: what may\n\
         run on a machine is that machine's own `p2pmux work allow`.\n"
    )?;
    writeln!(stdout, "On the machine you are adding, run:\n")?;
    writeln!(stdout, "  p2pmux enroll {invite} --name <name>\n")?;
    writeln!(
        stdout,
        "In cloud-init, with nobody there to type it:\n\n  \
         runcmd:\n    - [ sh, -c, \"p2pmux enroll {invite} --name build-box --accept-work\" ]"
    )?;
    Ok(())
}

/// Join the fleet a token names, with nobody sitting at this machine.
///
/// Everything is written *before* the join, exactly as `pair_with_code` does
/// it and for the same reason: a join that connects and then drops has still
/// enrolled the machine, and losing the record to a network blip on a box with
/// nobody at it means nobody finds out.
async fn enroll_with_token(
    token: &str,
    name: Option<&str>,
    accept_work: bool,
) -> Result<(), Box<dyn Error>> {
    let invite = crate::pairing::EnrolInvite::decode(token)?;
    if let Some(name) = name {
        crate::config::save(name)?;
    }
    let mut pairing = crate::pairing::load()?;
    pairing.ticket = Some(invite.ticket.clone());
    // The token names a session on the machine that minted it, which is the one
    // case where nobody is sitting here to notice it did not answer. Dialling
    // it on every boot until it does is the whole point of enrolling.
    pairing.offered_here = false;
    pairing.enrol = Some(crate::pairing::EnrolToken {
        secret: invite.secret.clone(),
        created_at: crate::pairing::now_unix(),
    });
    if accept_work {
        pairing.accepts_work = true;
        pairing.work.allow(&[]);
    }
    // The window the classic flow opens by hand. The machine that minted the
    // token is about to write this one into its fleet on sight; this is the
    // other direction, so that the two records agree without a second command.
    pairing.open_pairing_window(crate::pairing::now_unix());
    crate::pairing::save(&pairing)?;

    let descriptor = rejoin_paired_session(&invite.ticket).await?;
    let peers = wait_for_peers(&descriptor).await;
    let mut pairing = crate::pairing::load()?;
    for peer in &peers {
        pairing.remember(
            &peer.name,
            (!peer.machine_id.is_empty()).then(|| peer.machine_id.clone()),
            None,
        );
    }
    crate::pairing::save(&pairing)?;

    let mut stdout = io::stdout().lock();
    let here = crate::config::load()?.unwrap_or_else(hostname_label);
    writeln!(stdout, "enrolled as {here}")?;
    if peers.is_empty() {
        writeln!(
            stdout,
            "no other machine answered yet — it joins the fleet when one is running"
        )?;
    } else {
        for peer in &peers {
            writeln!(stdout, "  in a fleet with {}", peer.name)?;
        }
    }
    writeln!(stdout)?;
    write!(stdout, "{}", work_policy_summary(&crate::pairing::load()?))?;
    Ok(())
}

/// What this machine will let your other machines start on it, in words.
fn work_policy_summary(pairing: &crate::pairing::Pairing) -> String {
    let mut summary = String::from("On this machine, your other machines may start:\n");
    if !pairing.accepts_work {
        summary.push_str("  nothing — this machine has not agreed to accept work\n");
        summary.push_str("  `p2pmux work allow` here opens both gates, for a login shell\n");
        return summary;
    }
    if pairing.work.allow.is_empty() {
        // Naming the command matters more here than anywhere else in this
        // output: this is the state a machine is in right after being paired
        // with `--accept-work`, and it reads as the feature being broken.
        summary.push_str("  nothing yet — `p2pmux work allow` here allows a login shell,\n");
        summary.push_str("  or `p2pmux work allow claude` one named command\n");
        return summary;
    }
    for entry in &pairing.work.allow {
        if entry.trim() == crate::pairing::SHELL_ENTRY {
            summary.push_str("  a login shell — everything this user account can do\n");
        } else {
            summary.push_str(&format!("  {entry}\n"));
        }
    }
    if pairing.work.confirm {
        summary.push_str("  ...and only after someone here says yes each time\n");
    }
    summary
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
        peer_id: None,
        reachable: true,
        // The one row whose answer is genuinely known here: it was given on
        // this machine, about this machine.
        accepts_work: Some(pairing.accepts_work),
        this_machine: true,
        owned: true,
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
            peer_id: None,
            reachable: true,
            accepts_work: accepts_work(&peer.name),
            agents: peer.agents,
            this_machine: false,
            owned: pairing.owns(&peer.machine_id, &peer.name, peer.kind),
        });
    }
    for machine in &pairing.machines {
        if rows.iter().any(|row| row.name == machine.name) {
            continue;
        }
        rows.push(crate::tui::MachineRow {
            name: machine.name.clone(),
            peer_id: None,
            reachable: false,
            accepts_work: machine.accepts_work,
            agents: 0,
            this_machine: false,
            owned: true,
        });
    }
    rows.sort_by_key(|row| !row.owned);
    Ok(rows)
}

/// What bare `p2pmux` does: put this terminal in a session, always.
///
/// The command has to answer for a machine in a fleet and a machine on its own,
/// and those two want opposite things from the same six keystrokes:
///
/// * **Paired** — rejoin the session the pairing recorded. Every machine you own
///   converging on one session is the whole point of pairing once, and the line
///   `p2pmux pair` prints promises exactly this. Creating a fresh session here
///   instead would leave each machine hosting its own and the fleet never
///   meeting.
/// * **On its own** — create a session. Someone with no fleet who types the
///   shortest command wants a terminal, and this is the command that gives them
///   one.
///
/// What it may never do is end without a session. It used to attach the newest
/// live one first, whoever was in it — and a node serves one terminal at a time,
/// so the second window on a machine that already had p2pmux open got
/// `Error: already attached` and nothing else. That is the state of every
/// machine actually using this, which is why bare `p2pmux` read as broken. An
/// occupied session is now a session to pass over, not a failure to report.
///
/// `--resume` still reaches the picker, and `p2pmux attach` still goes back to a
/// session already running here.
async fn open_home() -> Result<(), Box<dyn Error>> {
    let store = crate::session_store::SessionStore::for_current_user()?;
    let pairing = crate::pairing::load_or_empty();
    if let Some(ticket) = pairing.ticket.as_deref() {
        // A node here is already in the fleet's session: use it rather than
        // standing up a second one alongside it. Only while it is free — an
        // occupied one falls through to a node of this terminal's own, which
        // lands in the same shared session anyway.
        if let Some(descriptor) = newest_live(&joined_to(&store.list_live()?, ticket))
            && let Some(result) = attach_unless_busy(&descriptor, crate::client::StartScreen::Home)
        {
            return result;
        }
    }
    // Which is a different question from whether there is a ticket at all: the
    // session offered to a machine that never came for it is still worth
    // attaching to while it is running here, and never worth dialling once it
    // is not. See `Pairing::rejoin_ticket`.
    if let Some(ticket) = pairing.rejoin_ticket(crate::pairing::now_unix()) {
        // Said before the dial, not after it. Reaching a machine that is not
        // answering costs iroh about thirty seconds, and this command spent
        // every one of them printing nothing at all: a bare `p2pmux` on a laptop
        // whose paired desktop is asleep looked exactly like a command that had
        // hung. Thirty seconds of "waiting for my other machine" is a wait; the
        // same thirty seconds in silence is what "p2pmux does nothing" was.
        {
            let mut stderr = io::stderr().lock();
            writeln!(
                stderr,
                "rejoining the session this machine is paired with; this can take \
                 up to half a minute if the other machine is asleep…"
            )?;
            stderr.flush()?;
        }
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
    // The session screen, not the inbox. A session created a moment ago has one
    // pane and no agents in it, so Home would open on an empty list — the blank
    // screen a first run is least able to interpret. Rejoining a fleet session
    // is the opposite case and keeps the inbox.
    crate::client::run_on(&descriptor, crate::client::StartScreen::Session)
}

/// Attach `descriptor`, unless another terminal is already in it.
///
/// `None` means only that: the session is taken, and the caller should look
/// elsewhere. Every other failure comes back as `Some(Err(..))` and stays the
/// caller's problem to report — swallowing those would hide a broken session
/// behind a brand-new one and make the fault impossible to see.
fn attach_unless_busy(
    descriptor: &crate::session_store::SessionDescriptor,
    start: crate::client::StartScreen,
) -> Option<Result<(), Box<dyn Error>>> {
    match crate::client::run_on(descriptor, start) {
        Err(error) if crate::client::is_already_attached(&*error) => None,
        result => Some(result),
    }
}

/// The live sessions here that are the one the pairing recorded.
///
/// Matched by ticket rather than taken as "the newest", because a machine in a
/// fleet also starts sessions of its own: attaching one of those for a bare
/// `p2pmux` would put the user somewhere their other machines are not, which is
/// the one place this command must never leave them. The coordinator that
/// minted the pairing ticket holds it as `ticket`; every machine that joined
/// holds the same string as `joined_ticket`.
fn joined_to(
    live: &[crate::session_store::SessionDescriptor],
    ticket: &str,
) -> Vec<crate::session_store::SessionDescriptor> {
    live.iter()
        .filter(|descriptor| {
            descriptor.joined_ticket.as_deref() == Some(ticket)
                || descriptor.ticket.as_deref() == Some(ticket)
        })
        .cloned()
        .collect()
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
        return Err(CliError("no live session here; `p2pmux` starts one").into());
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

/// A launch that has not been told it worked yet, and everything it would leave
/// behind if nobody told it.
///
/// Between `spawn` and "the session is up" there are five ways out, and all five
/// used to be a bare `return Err(..)`. That is not a leak of a handle: `Child`
/// has no `Drop`, so letting one fall out of scope neither stops the process nor
/// reaps it -- the standard library says as much, and says that dropping child
/// handles unwaited "is not recommended in long-running applications". The fleet
/// agent is the longest-running application this has, and it calls this on a
/// timer. On 2026-08-16 that arithmetic ran to nine abandoned nodes holding
/// 3.3GB on a 3.9GB machine, and the box's own OOM killer chose which of the
/// user's unrelated services to shoot.
///
/// So the attempt owns the node and its three files until [`Self::keep`] says
/// the session exists. Anything else -- an error, a timeout, a `?` on a path
/// nobody thought about -- unwinds through `Drop` and leaves the machine as it
/// was found.
struct LaunchAttempt {
    /// `None` before the spawn, and after [`Self::keep`] has handed the node on.
    child: Option<std::process::Child>,
    bootstrap_path: PathBuf,
    log_path: PathBuf,
    error_path: PathBuf,
    kept: bool,
}

impl LaunchAttempt {
    fn new(bootstrap_path: PathBuf, log_path: PathBuf, error_path: PathBuf) -> Self {
        Self {
            child: None,
            bootstrap_path,
            log_path,
            error_path,
            // Nothing is worth keeping until a session exists. Built this way
            // round because the failure that leaked 1014 files was a `?` between
            // writing the bootstrap and spawning anything at all.
            kept: false,
        }
    }

    /// The node is running and the session is recorded: it owns its own files
    /// from here, and this stops watching it.
    fn keep(mut self) {
        self.kept = true;
        if let Some(mut child) = self.child.take() {
            // This process never speaks to the node again -- but it is still its
            // parent, and a child nobody waits on becomes a zombie the moment it
            // exits. That is not hypothetical for a node that launches these: a
            // machine following an invitation into a session it turns out it
            // cannot reach records itself, becomes "ready", and dies seconds
            // later. One thread, which ends when the node does.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    /// The node is running. From here an abandoned attempt has a process to stop
    /// as well as files to remove.
    fn spawned(&mut self, child: std::process::Child) {
        self.child = Some(child);
    }
}

impl Drop for LaunchAttempt {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // SIGKILL rather than a polite stop. This node never became ready,
            // so there is no session to unwind, nothing to flush and nobody
            // attached -- and the caller is a supervision loop that is trying to
            // give up on it, which a graceful-stop timeout would hold open.
            let _ = child.kill();
            // Reaped here, not merely signalled, so the failure that is being
            // abandoned does not become a zombie instead of a leak.
            let _ = child.wait();
        }
        if self.kept {
            return;
        }
        // A launch that never produced a session owns nothing, so its files are
        // litter -- and litter that accumulates: one machine had 1014 orphaned
        // bootstraps in its runtime directory, one per failed attempt, because
        // the spawn failed after the file was written and nothing swept up.
        let _ = std::fs::remove_file(&self.bootstrap_path);
        let _ = std::fs::remove_file(&self.log_path);
        let _ = std::fs::remove_file(&self.error_path);
    }
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
    let mut descriptor =
        crate::session_store::SessionDescriptor::new(id.clone(), name, socket_path, 1, role);
    // Recorded before the node starts, because it is what a *later* invitation
    // is compared against: a machine has to be able to say "I am already in
    // that session" without attaching to it.
    if let crate::node::NodeBootstrapKind::Join { ticket, .. } = &kind {
        descriptor.joined_ticket = Some(ticket.clone());
    }
    let bootstrap = crate::node::NodeBootstrap {
        descriptor: descriptor.clone(),
        kind,
    };
    let bootstrap_path = descriptor.socket_path.with_extension("bootstrap");
    crate::node::write_bootstrap(&bootstrap_path, &bootstrap)?;
    // Armed on the line after the first file exists, so every `?` below this
    // point unwinds through it.
    let log_path = descriptor.socket_path.with_extension("log");
    let error_path = descriptor.socket_path.with_extension("error");
    let mut attempt =
        LaunchAttempt::new(bootstrap_path.clone(), log_path.clone(), error_path.clone());
    let mut command = ProcessCommand::new(std::env::current_exe()?);
    command
        .arg("__node")
        .arg("--bootstrap")
        .arg(&bootstrap_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // Whatever the node has to say about itself, kept for as long as the session
    // lives and deleted with it. It goes beside the socket rather than into a log
    // directory because it belongs to this session and to nothing else: the node's
    // own warnings, and -- the reason this exists -- the panic message of a node
    // that died under someone who was working in it. `/dev/null` here is what made
    // a session that ended on its own impossible to explain afterwards.
    match std::fs::File::create(&log_path) {
        Ok(log) => {
            command.stderr(Stdio::from(log));
        }
        Err(_) => {
            command.stderr(Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = std::fs::remove_file(&error_path);
    attempt.spawned(command.spawn()?);
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
            // Ready. The session exists, so the node keeps its files and this
            // stops being responsible for stopping it.
            attempt.keep();
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
        if matches!(
            attempt.child.as_mut().map(std::process::Child::try_wait),
            Some(Ok(Some(_)))
        ) {
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
pub(crate) fn display_name_or_hostname() -> Result<String, Box<dyn Error>> {
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
            "no session was created on this machine; `p2pmux` starts one",
        )),
        1 => Ok(hosted.remove(0)),
        _ => Err(CliError(
            "several sessions are hosted on this machine; pass the session name, as listed by p2pmux list",
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

    /// Bare `p2pmux` says it is dialling *before* it dials.
    ///
    /// Reaching a paired machine that is asleep costs iroh about thirty
    /// seconds. Printing afterwards — which is all this did — meant those
    /// thirty seconds were blank, and a blank terminal is indistinguishable
    /// from a hung command. Measured on a droplet whose peer was unreachable,
    /// the first byte arrived at t=31.6s.
    ///
    /// Pinned by order rather than by behaviour because the alternative is a
    /// test that waits out a real dial to a machine that is deliberately not
    /// answering, and a half-minute of CI per run is a worse trade than this.
    #[test]
    fn bare_p2pmux_announces_the_rejoin_before_it_waits_on_one() {
        let source = include_str!("cli.rs");
        let body = source
            .split_once("async fn open_home()")
            .expect("open_home")
            .1
            .split_once("\nasync fn ")
            .map_or_else(
                || source.split_once("async fn open_home()").unwrap().1,
                |split| split.0,
            );

        let announcement = body
            .find("rejoining the session this machine is paired with")
            .expect("open_home should say it is rejoining");
        let dial = body
            .find("rejoin_paired_session(ticket)")
            .expect("open_home should rejoin");
        assert!(
            announcement < dial,
            "the notice must be printed before the dial it explains, not after it"
        );
    }

    /// The node has no terminal, so whatever it writes to stderr is the only
    /// account of a session that ended by itself. Sending that to `/dev/null`
    /// is what left `p2pmux node ended` as the whole of what anyone could know.
    #[test]
    fn the_background_node_keeps_a_log_rather_than_discarding_its_stderr() {
        let source = include_str!("cli.rs");
        let launcher = source
            .split_once("pub(crate) fn launch_background_node(")
            .expect("the launcher")
            .1
            .split_once("let deadline = Instant::now()")
            .expect("the readiness wait")
            .0;

        assert!(launcher.contains("with_extension(\"log\")"));
        assert!(launcher.contains("Stdio::from(log)"));

        // And a session that ended the way it was asked to takes its log with
        // it, so this does not leave a file per session behind.
        let node_arm = source
            .split_once("Some(Command::Node { bootstrap }) => {")
            .expect("the node arm")
            .1;
        assert!(node_arm.contains("remove_file(bootstrap.with_extension(\"log\"))"));
    }

    /// The three files of a launch that produced no session are litter, and the
    /// launcher is the only thing that knows they are.
    ///
    /// One machine accumulated 1014 orphaned bootstraps this way -- one per
    /// attempt, for four hours -- because the write happened before a `?` that
    /// nobody had thought could fail.
    #[test]
    fn an_abandoned_launch_removes_the_files_it_wrote() {
        let dir = std::env::temp_dir().join(format!("p2pmux-attempt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let bootstrap = dir.join("a.bootstrap");
        let log = dir.join("a.log");
        let error = dir.join("a.error");
        for path in [&bootstrap, &log, &error] {
            std::fs::write(path, b"x").expect("fixture");
        }

        drop(super::LaunchAttempt::new(
            bootstrap.clone(),
            log.clone(),
            error.clone(),
        ));

        assert!(!bootstrap.exists(), "the bootstrap outlived its launch");
        assert!(!log.exists(), "the log outlived its launch");
        assert!(!error.exists(), "the error file outlived its launch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A launch that produced a session hands its files to the node, which owns
    /// them for as long as it runs and deletes them on the way out.
    #[test]
    fn a_kept_launch_leaves_the_session_its_files() {
        let dir = std::env::temp_dir().join(format!("p2pmux-kept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let bootstrap = dir.join("b.bootstrap");
        let log = dir.join("b.log");
        let error = dir.join("b.error");
        for path in [&bootstrap, &log, &error] {
            std::fs::write(path, b"x").expect("fixture");
        }

        super::LaunchAttempt::new(bootstrap.clone(), log.clone(), error.clone()).keep();

        assert!(log.exists(), "a live session lost the log it is writing to");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one that took a 3.9GB machine down: a node the launcher gave up on
    /// kept running, because `Child` has no `Drop` and the handle simply went
    /// out of scope. Nine of those, at up to 598MB each.
    #[cfg(unix)]
    #[test]
    fn a_launch_that_is_given_up_on_stops_the_node_it_started() {
        let dir = std::env::temp_dir().join(format!("p2pmux-kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        // A stand-in for a node that never becomes ready: it would sit there for
        // an hour, which is exactly the behaviour being defended against.
        let child = std::process::Command::new("sleep")
            .arg("3600")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the stand-in node");
        let pid = child.id();

        let mut attempt = super::LaunchAttempt::new(
            dir.join("c.bootstrap"),
            dir.join("c.log"),
            dir.join("c.error"),
        );
        attempt.spawned(child);
        drop(attempt);

        // Reaped, not merely signalled: `kill(pid, 0)` succeeds on a zombie, so
        // the question is asked of the process table via a second `wait`, which
        // the guard has already done. What is left is that the pid is not a
        // running `sleep` any more.
        let still_running = std::process::Command::new("ps")
            .arg("-o")
            .arg("state=")
            .arg("-p")
            .arg(pid.to_string())
            .output()
            .map(|output| {
                let state = String::from_utf8_lossy(&output.stdout);
                let state = state.trim();
                !state.is_empty() && !state.starts_with('Z')
            })
            .unwrap_or(false);
        assert!(
            !still_running,
            "the node outlived the launch that gave up on it (pid {pid})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
