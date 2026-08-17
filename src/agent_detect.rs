//! Pure helpers for detecting supported coding agents in a hosted PTY tree.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// How long a pane may sit at its shell prompt with a pushed status still
/// standing before that status is dropped.
///
/// No hook fires when an agent is killed, so a `working` or `needs you` push
/// would otherwise stand forever on a pane whose agent is long gone. Long
/// enough that a turn briefly between child processes never trips it; short
/// enough that a killed agent stops asking for attention it will never use.
pub const PUSHED_STATUS_GRACE: Duration = Duration::from_secs(20);

/// Return a process's current working directory when it is an existing absolute directory.
pub fn cwd_for_pid(pid: u32) -> Option<PathBuf> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    system
        .process(pid)
        .and_then(|process| process.cwd())
        .and_then(existing_absolute_directory)
}

fn existing_absolute_directory(path: &Path) -> Option<PathBuf> {
    (path.is_absolute() && path.is_dir()).then(|| path.to_path_buf())
}

/// A supported coding agent kind.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AgentKind {
    Claude,
    Codex,
    Cursor,
    Pi,
    OpenCode,
    /// A personal-assistant daemon, not a coding agent started in a pane.
    ///
    /// Both of these run under a runtime — Hermes under `python`, OpenClaw
    /// under `node` — so neither is ever the basename in `ps`, and neither is
    /// a child of the pane you are looking at. What identifies them is the
    /// module or program path in their command line.
    Hermes,
    OpenClaw,
}

impl AgentKind {
    /// Match supported agent launchers from process metadata.
    pub fn from_process(exe_basename: &str, name: &str, cmdline: &[String]) -> Option<Self> {
        if matches_launcher(exe_basename, name, cmdline, "claude") {
            Some(Self::Claude)
        } else if matches_launcher(exe_basename, name, cmdline, "cursor-agent")
            || cmdline
                .iter()
                .any(|argument| argument.contains("cursor-agent"))
        {
            Some(Self::Cursor)
        } else if matches_launcher(exe_basename, name, cmdline, "codex") {
            Some(Self::Codex)
        } else if matches_launcher(exe_basename, name, cmdline, "pi")
            || cmdline
                .iter()
                .any(|argument| argument.contains("pi-coding-agent"))
            || (exe_basename == "node"
                && cmdline.iter().any(|argument| {
                    argument.ends_with("/bin/pi") || argument.contains("@earendil-works/pi")
                }))
        {
            Some(Self::Pi)
        } else if matches_launcher(exe_basename, name, cmdline, "opencode")
            || cmdline
                .iter()
                .any(|argument| argument.contains("/opencode") || argument.ends_with("opencode"))
        {
            Some(Self::OpenCode)
        } else if matches_launcher(exe_basename, name, cmdline, "hermes")
            // The shape the gateway actually has on a machine running one:
            // `.../venv/bin/python -m hermes_cli.main gateway run`. Matched as
            // the argument *after* `-m`, not as a word anywhere in the command
            // line — `grep -rn hermes_cli .` mentions the module and is not
            // running it, and on a developer's machine that difference is the
            // whole difference.
            || cmdline
                .windows(2)
                .any(|pair| pair[0] == "-m" && is_module_path(&pair[1], "hermes_cli"))
        {
            Some(Self::Hermes)
        } else if matches_launcher(exe_basename, name, cmdline, "openclaw")
            || matches_launcher(exe_basename, name, cmdline, "clawd")
            || cmdline.iter().any(|argument| {
                is_program_path(argument, "openclaw") || is_program_path(argument, "clawd")
            })
        {
            Some(Self::OpenClaw)
        } else {
            None
        }
    }

    /// Stable wire value for this agent kind.
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
            Self::Hermes => "hermes",
            Self::OpenClaw => "openclaw",
        }
    }

    /// Parse a wire value produced by [`Self::wire_value`]. `None` for anything
    /// else — a producer naming an agent this build does not know is refused
    /// rather than coerced, so a typo never files status under the wrong kind.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            "pi" => Some(Self::Pi),
            "opencode" => Some(Self::OpenCode),
            "hermes" => Some(Self::Hermes),
            "openclaw" => Some(Self::OpenClaw),
            _ => None,
        }
    }

    /// Human-readable label for this kind.
    ///
    /// Not what the agents overlay prints: that renders the lowercase kind through
    /// `tui::render::agents::overlay_kind_label`, so a row reads `claude`, not
    /// `Claude Code`. Assertions that look for this string on a rendered screen are
    /// testing something the overlay does not draw.
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor Agent",
            Self::Pi => "Pi",
            Self::OpenCode => "OpenCode",
            Self::Hermes => "Hermes",
            Self::OpenClaw => "OpenClaw",
        }
    }

    /// What running this agent's chat command actually gets you, and what it is.
    ///
    /// One exhaustive match, so adding an agent cannot forget to answer the
    /// question — and the answer is checked against the real CLI rather than
    /// assumed, because the difference between joining a conversation and
    /// starting a new one is the difference between the feature working and the
    /// feature lying.
    pub const fn chat(self) -> AgentChat {
        match self {
            // A daemon plus a client: `openclaw chat` sends the turn through
            // the running gateway, and `--local` is the flag that opts out of
            // it. A real attach.
            Self::OpenClaw => AgentChat {
                access: ChatAccess::Attach,
                command: &["openclaw", "chat"],
            },
            // Not an attach, despite looking like one. Hermes is also a daemon
            // plus a client, but `hermes chat` runs the agent in the calling
            // process and there is no flag that joins the conversation the
            // gateway is having now. `--continue` resumes a *stored* session,
            // which is a different promise. Same agent, same memories, new
            // conversation — and the UI says so.
            Self::Hermes => AgentChat {
                access: ChatAccess::NewSession,
                command: &["hermes", "chat"],
            },
            // The coding agents. Starting the binary starts a conversation; it
            // cannot join one already running in someone else's terminal.
            Self::Claude => AgentChat {
                access: ChatAccess::NewSession,
                command: &["claude"],
            },
            Self::Codex => AgentChat {
                access: ChatAccess::NewSession,
                command: &["codex"],
            },
            Self::Cursor => AgentChat {
                access: ChatAccess::NewSession,
                command: &["cursor-agent"],
            },
            Self::Pi => AgentChat {
                access: ChatAccess::NewSession,
                command: &["pi"],
            },
            Self::OpenCode => AgentChat {
                access: ChatAccess::NewSession,
                command: &["opencode"],
            },
        }
    }
}

/// How to reach an agent from the inbox, and what reaching it means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentChat {
    pub access: ChatAccess,
    /// The argv to run in a terminal on the agent's own machine. Never a shell
    /// string: it is handed to the machine that hosts the pane, which decides
    /// whether it may run it, and that decision has to be about the same words
    /// that end up being executed.
    pub command: &'static [&'static str],
}

/// What running an agent's chat command gets you.
///
/// The distinction exists because getting it wrong is the one genuinely bad
/// outcome: opening a brand-new conversation while implying you joined the
/// running one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatAccess {
    /// A real client that joins the conversation already in progress.
    Attach,
    /// Starting the binary begins a *fresh* conversation with the same agent.
    /// It cannot join the running one.
    NewSession,
    /// Visible in the inbox, no way in.
    None,
}

impl ChatAccess {
    /// What the UI says it is about to do, before it does it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Attach => "joining the conversation already running",
            Self::NewSession => "starting a new conversation — not the one already running",
            Self::None => "no way to open a chat with this agent",
        }
    }

    /// The same fact in the width an inbox row has for it.
    ///
    /// On the row rather than only in the answer afterwards, because "which of
    /// the three is this" is worth knowing *before* pressing enter — which is
    /// the whole reason the distinction exists.
    pub const fn on_a_row(self) -> &'static str {
        match self {
            Self::Attach => "enter joins its conversation",
            Self::NewSession => "enter starts a new conversation",
            Self::None => "no way in from here",
        }
    }
}

fn matches_launcher(exe_basename: &str, name: &str, cmdline: &[String], launcher: &str) -> bool {
    exe_basename == launcher
        || name == launcher
        || cmdline.first().is_some_and(|argv0| argv0 == launcher)
}

/// Whether an argument is a Python module path rooted at `module`.
///
/// `hermes_cli.main`, not `--config=hermes_cli` and not a file called
/// `notes-about-hermes_cli.md`. The whole argument has to be the module or a
/// submodule of it, because a substring search over a command line finds every
/// process that merely mentions one.
fn is_module_path(argument: &str, module: &str) -> bool {
    argument == module
        || argument
            .strip_prefix(module)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Whether an argument is a path to, or the bare name of, the program `name`.
///
/// The same rule one level up: `/usr/lib/node_modules/openclaw/dist/index.js`
/// and `openclaw` count; `openclaw.md` does not, and neither does a word that
/// merely contains the name.
fn is_program_path(argument: &str, name: &str) -> bool {
    argument
        .split('/')
        .any(|component| component == name || component.strip_prefix(name) == Some(".js"))
}

/// The hidden subcommand every p2pmux node runs under. See `cli.rs`.
const NODE_SUBCOMMAND: &str = "__node";

/// Whether a process is a p2pmux node — the thing that hosts panes.
///
/// Both halves are required. The subcommand alone would match this repository's
/// own `cargo test` and every `grep __node` a developer runs in it; the program
/// name alone would match the client sitting in front of the user, which hosts
/// nothing and is not what "the pane's owner" means.
fn is_node_process(process: &ProcessSnapshot) -> bool {
    (is_program_path(&process.exe_basename, "p2pmux") || process.name == "p2pmux")
        && process
            .cmdline
            .iter()
            .any(|argument| argument == NODE_SUBCOMMAND)
}

/// Whether `pid` is a p2pmux node that is still running.
///
/// Asked of the operating system rather than of the node's socket, because a
/// socket cannot tell the two apart: it answers "connection refused" both when
/// its listener has gone and when the listener is merely not accepting fast
/// enough to keep its backlog under the limit. `SessionStore::list_live` needs
/// the difference before it deletes anything.
///
/// The command line is checked, not just the pid's existence, because pids are
/// reused -- fast, on a machine building software -- and treating whatever now
/// holds a dead node's pid as that node would keep a stale record forever.
/// When `pid` started, as the operating system counts it, or `None` if there is
/// no such process.
///
/// The start time is what makes a pid safe to hold on to. Pids are reused, so
/// "is 793097 still running" is a question that can start answering yes about a
/// completely different program; "is 793097 still the process that started at
/// this instant" cannot.
pub fn process_start_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    refreshed(&mut system, pid).map(sysinfo::Process::start_time)
}

pub fn node_process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    refreshed(&mut system, pid).is_some_and(|process| is_node_process(&snapshot_of(process)))
}

/// The socket every p2pmux node on this machine is listening on.
///
/// Read from the command lines rather than from any session store, because the
/// store is the one thing that does not answer this question: the socket
/// directory is shared by every p2pmux this user runs, whatever `HOME` each was
/// started with, while the records are not. A test harness, a probe script, or
/// anything else running in a sandbox `HOME` therefore finds sockets it has no
/// record of -- and used to delete the live ones among them.
///
/// A node is launched as `p2pmux __node --bootstrap <id>.bootstrap`, beside the
/// `<id>.sock` it binds, so its command line names the socket it owns for as
/// long as it lives.
pub fn node_socket_paths() -> HashSet<PathBuf> {
    let mut sampler = SysinfoSampler::default();
    node_sockets_in(&sample_global_snapshot(&mut sampler))
}

fn node_sockets_in(processes: &[ProcessSnapshot]) -> HashSet<PathBuf> {
    processes
        .iter()
        .filter(|process| is_node_process(process))
        .filter_map(|process| socket_of_node(&process.cmdline))
        .collect()
}

fn socket_of_node(cmdline: &[String]) -> Option<PathBuf> {
    let bootstrap = cmdline
        .iter()
        .skip_while(|argument| *argument != "--bootstrap")
        .nth(1)?;
    Some(PathBuf::from(bootstrap).with_extension("sock"))
}

/// One process from a sampler snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub exe_basename: String,
    pub name: String,
    pub cmdline: Vec<String>,
    pub start_time: Option<u64>,
    pub cwd: Option<String>,
}

/// Replaceable adapter for collecting one global process snapshot.
pub trait ProcessSampler {
    fn snapshot(&mut self) -> Vec<ProcessSnapshot>;
}

/// `sysinfo`-backed process sampler used by the host runtime.
pub struct SysinfoSampler {
    system: System,
}

impl Default for SysinfoSampler {
    fn default() -> Self {
        Self {
            system: System::new(),
        }
    }
}

impl ProcessSampler for SysinfoSampler {
    fn snapshot(&mut self) -> Vec<ProcessSnapshot> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always),
        );
        self.system.processes().values().map(snapshot_of).collect()
    }
}

/// One `sysinfo` process in this module's own terms.
///
/// `cwd` is whatever the refresh that produced `process` asked for: a caller
/// that did not ask gets `None`, which is the same thing a process that will
/// not say gets.
fn snapshot_of(process: &sysinfo::Process) -> ProcessSnapshot {
    ProcessSnapshot {
        pid: process.pid().as_u32(),
        parent_pid: process.parent().map(|pid| pid.as_u32()),
        exe_basename: process
            .exe()
            .and_then(|path| path.file_name())
            .unwrap_or(process.name())
            .to_string_lossy()
            .into_owned(),
        name: process.name().to_string_lossy().into_owned(),
        cmdline: process
            .cmd()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect(),
        start_time: Some(process.start_time()),
        cwd: process
            .cwd()
            .map(|path| path.to_string_lossy().into_owned()),
    }
}

/// Collect one global snapshot through an injected sampler.
pub fn sample_global_snapshot(sampler: &mut dyn ProcessSampler) -> Vec<ProcessSnapshot> {
    sampler.snapshot()
}

/// How far up the process tree a hook looks for the agent that spawned it.
///
/// A hook runner is a child or a grandchild of the agent — `claude` runs
/// `/bin/sh -c 'p2pmux notify …'`, which is two — and no supported agent puts
/// more than a couple of processes in between. The bound is what stops a walk
/// from `init` costing a lap of the process table on the agent's critical path.
const MAX_ANCESTOR_HOPS: usize = 8;

/// The agent process a hook is running underneath, if there is one.
///
/// This is how a hook outside a p2pmux pane learns *whose* status it is
/// reporting: there is no pane id to name, so the row is keyed by the agent's
/// own process, which is what [`AgentScan::loose_agents`] keys its rows by too.
///
/// Deliberately walked one process at a time rather than by taking a whole
/// snapshot: this runs on `UserPromptSubmit`, which blocks the user's prompt,
/// and on every single tool call. Eight targeted refreshes cost microseconds;
/// one global refresh reads every process on the machine.
///
/// `preferred` is the kind the hook says it belongs to, and wins over a nearer
/// ancestor of another kind: an agent run from inside another agent's terminal
/// must file its status under its own process, not its host's. A match of any
/// kind is still better than none, so one is kept as a fallback.
pub fn agent_ancestor(from_pid: u32, preferred: AgentKind) -> Option<(u32, AgentKind, u64)> {
    let mut system = System::new();
    let mut pid = Pid::from_u32(from_pid);
    let mut fallback = None;
    for _ in 0..MAX_ANCESTOR_HOPS {
        let parent = refreshed(&mut system, pid)?.parent()?;
        let process = refreshed(&mut system, parent)?;
        let exe_basename = process
            .exe()
            .and_then(|path| path.file_name())
            .unwrap_or(process.name())
            .to_string_lossy()
            .into_owned();
        let name = process.name().to_string_lossy().into_owned();
        let cmdline = process
            .cmd()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if let Some(kind) = AgentKind::from_process(&exe_basename, &name, &cmdline) {
            let found = (parent.as_u32(), kind, process.start_time());
            if kind == preferred {
                return Some(found);
            }
            fallback = fallback.or(Some(found));
        }
        pid = parent;
    }
    fallback
}

fn refreshed(system: &mut System, pid: Pid) -> Option<&sysinfo::Process> {
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    system.process(pid)
}

/// The supported agent selected from a hosted pane's process tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    pub cwd: String,
}

/// An agent running on this machine but not in any p2pmux pane.
///
/// A bot under systemd, or something started in a stray tmux. Detectable, and —
/// until the capability table existed — not reachable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LooseAgent {
    pub kind: AgentKind,
    /// Identity for a row that has no pane to be identified by.
    pub pid: u32,
    pub cwd: String,
    /// When the process started, as the process table reports it. Only ever
    /// used to tell this agent from an older one that had the same pid — see
    /// [`crate::agent_status::read`].
    pub start_time: Option<u64>,
    /// The p2pmux node this agent is running under, or `0` for one that is
    /// running under no node at all.
    ///
    /// Non-zero means the agent is in a p2pmux pane — just not in the session
    /// asking the question. Panes of *this* session never get here; they are
    /// excluded by `pane_roots` before a loose agent is built. So a non-zero
    /// value is always another session on this machine, and saying "running
    /// outside p2pmux" about it is simply untrue.
    ///
    /// The pid rather than the session name, because a process table is what
    /// this module reads and a session's name lives in the session store. The
    /// caller that has both turns one into the other.
    pub node_pid: u32,
    /// The name of the session [`Self::node_pid`] is hosting, when the caller
    /// has looked it up. Empty for an agent under no node, and for one whose
    /// session left no record behind.
    pub session: String,
    /// What its hooks last said, or [`AgentState::Unknown`] if nothing has.
    ///
    /// Filled in from [`crate::agent_status`] after the scan, not by it: which
    /// agent is running is a question about processes, what it is doing is a
    /// question only the agent can answer.
    pub state: AgentState,
    /// The agent's own words. Local to this machine, exactly as on the pane
    /// path — the roster published to peers has no field for it.
    pub message: String,
    pub working_since_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct Candidate<'a> {
    process: &'a ProcessSnapshot,
    depth: usize,
    kind: AgentKind,
}

/// One snapshot, prepared for classification.
///
/// The expensive parts of classifying a pane — running every agent matcher over
/// every process, and finding a process by pid — do not depend on which pane is
/// being classified, so they happen once per snapshot here rather than once per
/// pane. With one global scan feeding a session's worth of panes, that is the
/// difference between O(panes × processes) matcher calls per second and O(panes
/// × agents).
pub struct AgentScan<'a> {
    by_pid: HashMap<u32, &'a ProcessSnapshot>,
    /// Every pid in the snapshot that is some process's parent.
    parents: HashSet<u32>,
    /// Every process in the snapshot that looks like a supported agent. Tiny
    /// next to the snapshot itself: a machine has a handful of these, not
    /// hundreds.
    agents: Vec<(&'a ProcessSnapshot, AgentKind)>,
}

impl<'a> AgentScan<'a> {
    pub fn new(processes: &'a [ProcessSnapshot]) -> Self {
        let by_pid = processes
            .iter()
            .map(|process| (process.pid, process))
            .collect();
        let parents = processes
            .iter()
            .filter_map(|process| process.parent_pid)
            .collect();
        let agents = processes
            .iter()
            .filter_map(|process| {
                AgentKind::from_process(&process.exe_basename, &process.name, &process.cmdline)
                    .map(|kind| (process, kind))
            })
            .collect();
        Self {
            by_pid,
            parents,
            agents,
        }
    }

    /// Whether a pane's session child still has any child process — whether the
    /// pane is running something rather than sitting at its shell prompt.
    ///
    /// Deliberately not an agent-aware check. This is the evidence that a
    /// producer has gone, and it must hold even for an agent launched through a
    /// wrapper no matcher in this module recognizes: whatever the pane was
    /// running, it is not running it any more.
    pub fn has_children(&self, session_child_pid: u32) -> bool {
        self.parents.contains(&session_child_pid)
    }

    /// Classify one PTY session child's process tree.
    ///
    /// The deepest matching descendant wins, then the newest available start
    /// time, then the highest PID. The PTY session child itself is included in
    /// the tree.
    pub fn classify(&self, session_child_pid: u32) -> Option<DetectedAgent> {
        self.agents
            .iter()
            .filter_map(|&(process, kind)| {
                self.descendant_depth(session_child_pid, process.pid)
                    .map(|depth| Candidate {
                        process,
                        depth,
                        kind,
                    })
            })
            .max_by(|left, right| {
                left.depth
                    .cmp(&right.depth)
                    .then_with(
                        || match (left.process.start_time, right.process.start_time) {
                            (Some(left), Some(right)) => left.cmp(&right),
                            _ => std::cmp::Ordering::Equal,
                        },
                    )
                    .then_with(|| left.process.pid.cmp(&right.process.pid))
            })
            .map(|candidate| DetectedAgent {
                kind: candidate.kind,
                cwd: candidate.process.cwd.clone().unwrap_or_default(),
            })
    }

    /// Agents on this machine that are not inside any of these panes.
    ///
    /// The inbox was built on the assumption that an agent is something you
    /// started in a p2pmux pane, so an assistant running under systemd — which
    /// is how both Hermes and OpenClaw are meant to run — was invisible to it.
    /// This is the other half: every agent the scan found whose process is not
    /// descended from a pane.
    ///
    /// A daemon's own workers are the same agent, so a process that descends
    /// from another loose agent of its kind is folded into it and only the
    /// outermost gets a row. Two agents that are not each other's ancestors are
    /// two agents, however alike they look: a person with `claude` open in
    /// three terminals has three of these, and collapsing them to one row would
    /// mean two agents whose `needs you` nobody is ever shown.
    ///
    /// State is not answered here. The scan says which agents are running; a
    /// hook says what they are doing, and the caller reads those from
    /// [`crate::agent_status`].
    pub fn loose_agents(&self, pane_roots: &[u32]) -> Vec<LooseAgent> {
        let outside = self
            .agents
            .iter()
            .filter(|(process, _)| {
                !pane_roots
                    .iter()
                    .any(|&root| self.descendant_depth(root, process.pid).is_some())
            })
            .collect::<Vec<_>>();
        let kind_by_pid = outside
            .iter()
            .map(|(process, kind)| (process.pid, *kind))
            .collect::<HashMap<_, _>>();
        let mut found = outside
            .iter()
            .filter(|(process, kind)| {
                !self
                    .ancestors_of(process.pid)
                    .any(|pid| kind_by_pid.get(&pid) == Some(kind))
            })
            .map(|(process, kind)| LooseAgent {
                kind: *kind,
                pid: process.pid,
                cwd: process.cwd.clone().unwrap_or_default(),
                start_time: process.start_time,
                node_pid: self.enclosing_node(process.pid).unwrap_or_default(),
                session: String::new(),
                state: AgentState::Unknown,
                message: String::new(),
                working_since_unix_ms: 0,
            })
            .collect::<Vec<_>>();
        // Ordered so the rows do not shuffle between two scans that found the
        // same agents: the process table hands them over in whatever order it
        // likes, and the inbox compares this list to decide it changed.
        found.sort_by_key(|agent| (agent.kind, agent.pid));
        found
    }

    /// The p2pmux node `pid` runs under, if it runs under one.
    ///
    /// A pane's shell is a child of the node hosting it, so every agent started
    /// in a pane has one of these above it. That is what separates an agent in
    /// *another* p2pmux session on this machine from one started in a bare
    /// terminal — two things the inbox used to call by the same wrong name,
    /// because it only ever asked about the panes of the session in front of
    /// you and read "not one of mine" as "not p2pmux at all".
    ///
    /// The nearest node wins, which matters the day somebody attaches one
    /// session from inside another's pane: the agent belongs to the session
    /// whose pane it is actually in.
    pub fn enclosing_node(&self, pid: u32) -> Option<u32> {
        self.ancestors_of(pid).find(|ancestor| {
            self.by_pid
                .get(ancestor)
                .is_some_and(|p| is_node_process(p))
        })
    }

    /// Whether the snapshot this scan was built from saw `pid` at all.
    ///
    /// Not "is it an agent" — any process. It answers the only question the
    /// status sweep has: is the process that wrote this record still there.
    pub fn knows_pid(&self, pid: u32) -> bool {
        self.by_pid.contains_key(&pid)
    }

    /// Every pid above `pid`, nearest first, bounded by the snapshot's size so
    /// a cycle in a stale process table cannot spin.
    fn ancestors_of(&self, pid: u32) -> impl Iterator<Item = u32> + '_ {
        let mut current = Some(pid);
        let mut steps = 0;
        std::iter::from_fn(move || {
            let parent = self.by_pid.get(&current?)?.parent_pid?;
            steps += 1;
            if steps > self.by_pid.len() {
                return None;
            }
            current = Some(parent);
            Some(parent)
        })
    }

    fn descendant_depth(&self, root_pid: u32, pid: u32) -> Option<usize> {
        let mut current_pid = pid;
        let mut depth = 0;

        while current_pid != root_pid {
            current_pid = self.by_pid.get(&current_pid)?.parent_pid?;
            depth += 1;
            if depth > self.by_pid.len() {
                return None;
            }
        }

        Some(depth)
    }
}

/// Classify one pane against a whole snapshot. Convenience for a single lookup;
/// classifying several panes should build one [`AgentScan`] and reuse it.
pub fn classify_pane_tree(
    session_child_pid: u32,
    processes: &[ProcessSnapshot],
) -> Option<DetectedAgent> {
    AgentScan::new(processes).classify(session_child_pid)
}

/// Coarse activity state shown in the agents overlay.
///
/// Every one of these arrives from a producer inside the pane — an agent hook,
/// never an inference. This module used to derive `Working`/`Done` from how long
/// a pane had been quiet, and that could not work: silence looks identical
/// whether an agent is thinking, waiting on a permission prompt, or finished.
/// The guess fired completions mid-task and could never once report the state a
/// human most needs, so it is gone.
///
/// What is left when nothing has reported is [`Self::Unknown`] — the process
/// scan can see that an agent is running in a pane without knowing a thing about
/// what it is doing. Saying so is honest; calling it `Idle` was not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    /// An agent is running here and no producer has reported its state. Either
    /// its hooks are not wired up, or none has fired yet.
    Unknown,
    Idle,
    Working,
    Done,
    Pending,
    Error,
}

impl AgentState {
    /// Stable wire value for a pushed status.
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Pending => "pending",
            Self::Error => "error",
        }
    }

    /// Parse a pushed status. Strict: an unrecognized token is `None`, never a
    /// lenient fallback to `Idle`. Leniency here would let a producer's typo
    /// silently erase the row it meant to update, which is the worst possible
    /// failure for a status a human is waiting on.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "working" | "running" => Some(Self::Working),
            "done" => Some(Self::Done),
            "pending" => Some(Self::Pending),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// A status reported by a producer running inside the pane — an agent hook,
/// not an inference. Authoritative while present: the agent said so.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushedAgent {
    pub kind: AgentKind,
    pub cwd: String,
    pub state: AgentState,
    /// What the agent is doing, in its own words.
    ///
    /// Local only. This is the one field carrying anything the agent actually
    /// said, and a p2pmux session is shared: it reaches this machine's own
    /// overlay and stops there, stripped before the roster goes to peers. See
    /// `SharedLocalPane::agent_roster_entry`.
    pub message: String,
    /// When this status arrived. The expiry policy reads it.
    pub at: Instant,
    /// Start of the current working interval, carried across pushes that stay
    /// `Working` so a per-tool-call stream of updates does not reset the clock.
    pub working_since_unix_ms: u64,
}

/// One agent worth showing a row for, and everything the row needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedAgent {
    pub agent: DetectedAgent,
    pub state: AgentState,
    /// The agent's own words. Empty unless a producer sent them; never leaves
    /// this machine.
    pub message: String,
    /// Unix milliseconds when the current working interval started, or `0`.
    pub working_since_unix_ms: u64,
}

/// Per-pane agent state.
///
/// Two sources, and only one of them speaks about state. The process scan says
/// *which* agent is running in a pane; a producer's hook says *what it is
/// doing*. Nothing here tries to derive the second from PTY output any more —
/// see [`AgentState`] for why that never worked.
#[derive(Clone, Debug, Default)]
pub struct PaneAgentTracker {
    /// What the process scan last saw running in this pane.
    pub active_agent: Option<DetectedAgent>,
    /// Latest producer-pushed status, when one is running in this pane.
    pub pushed: Option<PushedAgent>,
    /// When this pane was first seen sitting at its shell prompt while a pushed
    /// status was still standing. `None` cancels the grace clock.
    push_suspect_since: Option<Instant>,
}

impl PaneAgentTracker {
    /// Feed the pane's liveness into the pushed-status grace clock.
    ///
    /// `has_children` is whether the pane is running anything at all. Nothing
    /// fires a hook when an agent is killed — no `Stop`, no `SessionEnd` — so a
    /// `working` or `needs you` push has no other way to end. A pane that has
    /// fallen back to its shell prompt is the evidence that the producer is
    /// gone.
    ///
    /// A first sighting only starts the clock. A turn is legitimately between
    /// child processes for moments at a time, and a `pending` agent can sit
    /// waiting for an answer indefinitely, so nothing is dropped until the pane
    /// has *stayed* at its prompt for [`PUSHED_STATUS_GRACE`] — and repeat
    /// sightings do not reset it, because staying there is exactly the point.
    pub fn observe_pane_liveness(&mut self, has_children: bool, now: Instant) {
        if self.pushed.is_none() || has_children {
            self.push_suspect_since = None;
            return;
        }
        let since = *self.push_suspect_since.get_or_insert(now);
        if now.duration_since(since) >= PUSHED_STATUS_GRACE {
            self.pushed = None;
            self.push_suspect_since = None;
        }
    }

    /// Apply a status pushed by a producer inside the pane.
    ///
    /// A push carries its interval start forward while the state is unchanged,
    /// so the per-tool-call stream a hooked agent emits shows one continuous
    /// interval rather than restarting the clock every few seconds. A state
    /// that *has* changed dates from now: that is a new episode, and the
    /// question the row answers is how long it has been in the state it is in.
    ///
    /// Every state gets a clock, not only `Working`. An inbox exists to say
    /// which agent needs you, and the first thing worth knowing about one that
    /// does is how long it has been waiting — a `needs you` row with no clock
    /// cannot be told from one that only just stopped.
    ///
    /// An empty `message` leaves the stored one standing. Not every hook event
    /// carries text — a `PreToolUse` has nothing to say — and blanking the line
    /// on each of those would leave the message visible only in the gaps
    /// between tool calls.
    pub fn record_pushed_status(
        &mut self,
        kind: AgentKind,
        cwd: String,
        state: AgentState,
        message: String,
        now: Instant,
        unix_ms_now: u64,
    ) {
        let previous = self
            .pushed
            .as_ref()
            .filter(|pushed| pushed.kind == kind)
            .map(|pushed| (pushed.state, pushed.working_since_unix_ms, &pushed.message));
        let working_since_unix_ms = previous
            .filter(|(previous_state, ..)| *previous_state == state)
            .map(|(_, since, _)| since)
            // A stored zero is a clock that never started — a `SystemTime` the
            // producer could not read. Carrying it forward would leave the row
            // with no clock for the whole episode, so start one now instead.
            .filter(|since| *since != 0)
            .unwrap_or(unix_ms_now);
        let message = if message.is_empty() {
            previous
                .map(|(.., message)| message.clone())
                .unwrap_or_default()
        } else {
            message
        };
        self.pushed = Some(PushedAgent {
            kind,
            cwd,
            state,
            message,
            at: now,
            working_since_unix_ms,
        });
        // A payload is proof the producer is alive — cancel any grace clock
        // before it can misfire.
        self.push_suspect_since = None;
    }

    /// The authoritative pushed status, if a producer currently owns this pane.
    ///
    /// An `Idle` push is deliberately not authoritative: it means "no activity"
    /// (Claude's `/clear`, a session ending), which should hand the pane back to
    /// process detection rather than blank a row whose agent is still running.
    fn owning_push(&self) -> Option<&PushedAgent> {
        self.pushed
            .as_ref()
            .filter(|pushed| pushed.state != AgentState::Idle)
    }

    /// Whether a producer currently owns this pane's state.
    pub fn has_owning_push(&self) -> bool {
        self.owning_push().is_some()
    }

    /// Apply this pane's latest process-tree classification.
    pub fn update(&mut self, detected: Option<DetectedAgent>) {
        self.active_agent = detected;
    }

    /// Return the agent and state for this pane's row, if it should have one.
    ///
    /// A producer outranks the scan and stands alone: a hooked agent this
    /// build's process matchers do not recognize still gets a row, which is the
    /// whole point of accepting pushes. A scanned agent with nothing pushed
    /// gets a row too, reported as [`AgentState::Unknown`] — it is running, and
    /// that is genuinely all anyone here knows about it.
    pub fn listed_agent(&self) -> Option<ListedAgent> {
        if let Some(pushed) = self.owning_push() {
            return Some(ListedAgent {
                agent: DetectedAgent {
                    kind: pushed.kind,
                    cwd: pushed.cwd.clone(),
                },
                state: pushed.state,
                message: pushed.message.clone(),
                working_since_unix_ms: pushed.working_since_unix_ms,
            });
        }
        self.active_agent.as_ref().map(|agent| ListedAgent {
            agent: agent.clone(),
            state: AgentState::Unknown,
            message: String::new(),
            working_since_unix_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        process::{Command, Stdio},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn cwd_for_pid_reads_a_live_childs_working_directory() {
        let directory = std::env::temp_dir().join(format!(
            "p2pmux-cwd-for-pid-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        fs::create_dir(&directory).expect("create temporary directory");
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .current_dir(&directory)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");

        let cwd = (0..20).find_map(|_| {
            let cwd = cwd_for_pid(child.id());
            if cwd.is_some() {
                cwd
            } else {
                thread::sleep(Duration::from_millis(25));
                None
            }
        });

        assert_eq!(
            cwd.as_deref()
                .map(|cwd| fs::canonicalize(cwd).expect("canonicalize detected cwd")),
            Some(fs::canonicalize(&directory).expect("canonicalize temporary directory"))
        );

        let _ = child.kill();
        let _ = child.wait();
        fs::remove_dir(&directory).expect("remove temporary directory");
    }

    #[test]
    fn cwd_validation_rejects_missing_paths_and_non_directories() {
        let temporary_root = std::env::temp_dir().join(format!(
            "p2pmux-cwd-validation-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        fs::create_dir(&temporary_root).expect("create temporary directory");
        let file = temporary_root.join("file");
        fs::write(&file, "not a directory").expect("write temporary file");

        assert_eq!(
            existing_absolute_directory(&temporary_root),
            Some(temporary_root.clone())
        );
        assert_eq!(existing_absolute_directory(&file), None);
        assert_eq!(
            existing_absolute_directory(&temporary_root.join("missing")),
            None
        );
        assert_eq!(existing_absolute_directory(Path::new("relative")), None);

        fs::remove_dir_all(&temporary_root).expect("remove temporary directory");
    }

    fn process(
        pid: u32,
        parent_pid: Option<u32>,
        exe_basename: &str,
        start_time: Option<u64>,
    ) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            parent_pid,
            exe_basename: exe_basename.into(),
            name: exe_basename.into(),
            cmdline: Vec::new(),
            start_time,
            cwd: None,
        }
    }

    /// A person with `claude` open in three terminals has three agents, and an
    /// inbox that shows one of them is an inbox that hides two `needs you`.
    /// Only a process descended from another agent of its own kind is the same
    /// agent — that is a daemon's worker, not a second session.
    #[test]
    fn every_agent_outside_a_pane_gets_its_own_row_unless_it_is_another_ones_child() {
        let processes = vec![
            process(1, None, "launchd", Some(1)),
            // Two independent claude sessions, in two terminals.
            process(10, Some(1), "claude", Some(10)),
            process(11, Some(1), "claude", Some(11)),
            // …and one worker each of them forked, which is the same agent.
            process(12, Some(10), "claude", Some(12)),
            // A different kind, on its own.
            process(20, Some(1), "codex", Some(20)),
            // One inside a pane, which the pane's own row already covers.
            process(30, Some(99), "claude", Some(30)),
            process(99, Some(1), "zsh", Some(99)),
        ];
        let scan = AgentScan::new(&processes);

        let loose = scan.loose_agents(&[99]);

        assert_eq!(
            loose
                .iter()
                .map(|agent| (agent.kind, agent.pid))
                .collect::<Vec<_>>(),
            vec![
                (AgentKind::Claude, 10),
                (AgentKind::Claude, 11),
                (AgentKind::Codex, 20)
            ]
        );
        // Nothing here answers what any of them is doing.
        assert!(
            loose
                .iter()
                .all(|agent| agent.state == AgentState::Unknown
                    && agent.working_since_unix_ms == 0)
        );
        assert_eq!(loose[0].start_time, Some(10));
    }

    /// The bug a fresh session on a busy machine walks straight into: three
    /// detached p2pmux sessions, each with an agent in a pane, and every one of
    /// them listed as "running outside p2pmux" because the new session only
    /// ever compared against its own panes.
    ///
    /// They are still loose — this session cannot jump to a pane it does not
    /// have — but each one now carries the node it belongs to, which is what
    /// lets the row name a session instead of denying there is one.
    #[test]
    fn an_agent_in_another_sessions_pane_is_loose_but_not_outside_p2pmux() {
        let mut node = process(50, Some(1), "p2pmux", Some(50));
        node.cmdline = vec![
            String::from("/opt/homebrew/bin/p2pmux"),
            String::from("__node"),
            String::from("--bootstrap"),
            String::from("/tmp/p2pmux-503/abc.bootstrap"),
        ];
        // The client the user is looking at is a p2pmux too, and hosts nothing.
        let mut client = process(60, Some(1), "p2pmux", Some(60));
        client.cmdline = vec![String::from("p2pmux"), String::from("create")];
        let processes = vec![
            process(1, None, "launchd", Some(1)),
            node,
            process(51, Some(50), "zsh", Some(51)),
            // The agent in the other session's pane.
            process(52, Some(51), "claude", Some(52)),
            client,
            // …and one genuinely outside every p2pmux, in a bare terminal.
            process(70, Some(1), "claude", Some(70)),
            // This session's own pane, which is not loose at all.
            process(80, Some(1), "zsh", Some(80)),
            process(81, Some(80), "claude", Some(81)),
        ];
        let scan = AgentScan::new(&processes);

        let loose = scan.loose_agents(&[80]);

        assert_eq!(
            loose
                .iter()
                .map(|agent| (agent.pid, agent.node_pid))
                .collect::<Vec<_>>(),
            vec![(52, 50), (70, 0)],
            "the agent in another session's pane names that session's node; \
             the one in a bare terminal names nothing"
        );
        // Nobody has looked a name up yet, which is the caller's job.
        assert!(loose.iter().all(|agent| agent.session.is_empty()));
    }

    /// A node names the socket it owns on its command line, which is the only
    /// place to read it from when the record belongs to another `HOME`.
    #[test]
    fn every_running_nodes_socket_is_found_whatever_home_recorded_it() {
        let mut node = process(10, Some(1), "p2pmux", Some(10));
        node.cmdline = vec![
            String::from("/opt/homebrew/bin/p2pmux"),
            String::from("__node"),
            String::from("--bootstrap"),
            String::from("/tmp/p2pmux-503/abc.bootstrap"),
        ];
        // Not a node: the client in front of the user, and this repository's
        // own tooling with `__node` on its command line.
        let mut client = process(20, Some(1), "p2pmux", Some(20));
        client.cmdline = vec![String::from("p2pmux"), String::from("attach")];
        let mut grep = process(30, Some(1), "rg", Some(30));
        grep.cmdline = vec![String::from("rg"), String::from("--bootstrap")];

        let sockets = node_sockets_in(&[node, client, grep]);

        assert_eq!(
            sockets,
            HashSet::from([PathBuf::from("/tmp/p2pmux-503/abc.sock")])
        );
    }

    /// A node with no `--bootstrap` at all is not a socket to protect, and must
    /// not be read as one -- `nth(1)` off the end of a command line is `None`.
    #[test]
    fn a_node_without_a_bootstrap_argument_names_no_socket() {
        assert_eq!(socket_of_node(&[String::from("p2pmux")]), None);
        assert_eq!(
            socket_of_node(&[String::from("p2pmux"), String::from("--bootstrap")]),
            None
        );
    }

    /// What the session store asks before deleting a session's only record.
    /// Nothing here is a node: not pid 0, and not this test binary -- which is
    /// the point, since it is a process in this repository with `__node` in
    /// reach of its command line.
    #[test]
    fn nothing_that_is_not_a_node_is_reported_as_one() {
        assert!(!node_process_is_alive(0));
        assert!(!node_process_is_alive(std::process::id()));
    }

    /// The client is a `p2pmux` process too, and a `cargo test` in this
    /// repository has `__node` all over its command line. Neither hosts a pane,
    /// and mistaking either for one would attribute an agent to a session that
    /// does not exist.
    #[test]
    fn only_a_real_node_counts_as_the_session_an_agent_is_in() {
        let mut client = process(10, Some(1), "p2pmux", Some(10));
        client.cmdline = vec![
            String::from("p2pmux"),
            String::from("attach"),
            String::from("dakar"),
        ];
        let mut grep = process(20, Some(1), "rg", Some(20));
        grep.cmdline = vec![String::from("rg"), String::from("__node")];
        let processes = vec![
            process(1, None, "launchd", Some(1)),
            client,
            process(11, Some(10), "claude", Some(11)),
            grep,
            process(21, Some(20), "claude", Some(21)),
        ];
        let scan = AgentScan::new(&processes);

        assert_eq!(scan.enclosing_node(11), None);
        assert_eq!(scan.enclosing_node(21), None);
    }

    /// The list is compared against the previous scan's to decide the inbox
    /// changed, so an unstable order would republish the roster forever.
    #[test]
    fn the_same_agents_come_back_in_the_same_order() {
        let forwards = vec![
            process(1, None, "launchd", Some(1)),
            process(10, Some(1), "claude", Some(10)),
            process(11, Some(1), "codex", Some(11)),
        ];
        let backwards = forwards.iter().rev().cloned().collect::<Vec<_>>();

        assert_eq!(
            AgentScan::new(&forwards).loose_agents(&[]),
            AgentScan::new(&backwards).loose_agents(&[])
        );
    }

    #[test]
    fn the_scan_knows_which_pids_it_saw() {
        let processes = vec![process(1, None, "launchd", Some(1))];
        let scan = AgentScan::new(&processes);

        assert!(scan.knows_pid(1));
        assert!(!scan.knows_pid(2));
    }

    #[test]
    fn process_matchers_cover_direct_and_wrapped_agent_launches() {
        assert_eq!(
            AgentKind::from_process("claude", "claude", &[]),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::from_process("sh", "codex", &[]),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            AgentKind::from_process("cursor-agent", "node", &[]),
            Some(AgentKind::Cursor)
        );
        assert_eq!(
            AgentKind::from_process("pi", "pi", &[]),
            Some(AgentKind::Pi)
        );
        assert_eq!(
            AgentKind::from_process("opencode", "opencode", &[]),
            Some(AgentKind::OpenCode)
        );

        let cursor_argv = vec![
            String::from("/Applications/Cursor.app/Contents/Resources/app/bin/agent"),
            String::from("--use-system-ca"),
            String::from(
                "/Applications/Cursor.app/Contents/Resources/app/extensions/cursor-agent/dist/index.js",
            ),
        ];
        assert_eq!(
            AgentKind::from_process("node", "node", &cursor_argv),
            Some(AgentKind::Cursor)
        );
        assert_eq!(
            AgentKind::from_process("codex", "agent", &cursor_argv),
            Some(AgentKind::Cursor)
        );

        let pi_argv = vec![
            String::from("node"),
            String::from("/tmp/pi-coding-agent/dist/cli.js"),
        ];
        assert_eq!(
            AgentKind::from_process("node", "node", &pi_argv),
            Some(AgentKind::Pi)
        );
        let pi_bin_argv = vec![
            String::from("node"),
            String::from("/Users/me/.local/bin/pi"),
        ];
        assert_eq!(
            AgentKind::from_process("node", "node", &pi_bin_argv),
            Some(AgentKind::Pi)
        );
        let opencode_argv = vec![
            String::from("node"),
            String::from("/usr/local/lib/node_modules/opencode/bin/opencode"),
        ];
        assert_eq!(
            AgentKind::from_process("node", "node", &opencode_argv),
            Some(AgentKind::OpenCode)
        );

        assert_eq!(AgentKind::from_process("cursor", "cursor", &[]), None);
        assert_eq!(AgentKind::from_process("node", "node", &[]), None);
        assert_eq!(AgentKind::from_process("Codex", "Codex", &[]), None);
        assert_eq!(AgentKind::Claude.display_label(), "Claude Code");
        assert_eq!(AgentKind::Cursor.wire_value(), "cursor");
        assert_eq!(AgentKind::OpenCode.wire_value(), "opencode");
    }

    /// The two assistant daemons, matched on the shape they really have.
    ///
    /// Neither is ever the basename in `ps` — Hermes runs under `python` and
    /// OpenClaw under `node` — and neither is a child of the pane you are
    /// looking at, so nothing about the old "the launcher is the pane's own
    /// child" assumption applies.
    #[test]
    fn the_assistant_daemons_are_matched_by_what_ps_actually_shows() {
        // Copied from `ps` on the droplet running one:
        // /home/pelazas/.hermes/hermes-agent/venv/bin/python -m hermes_cli.main gateway run --replace
        let hermes_gateway = vec![
            String::from("/home/pelazas/.hermes/hermes-agent/venv/bin/python"),
            String::from("-m"),
            String::from("hermes_cli.main"),
            String::from("gateway"),
            String::from("run"),
            String::from("--replace"),
        ];
        assert_eq!(
            AgentKind::from_process("python", "python", &hermes_gateway),
            Some(AgentKind::Hermes)
        );
        assert_eq!(
            AgentKind::from_process("python3.11", "python3.11", &hermes_gateway),
            Some(AgentKind::Hermes),
            "the runtime's version in the basename must not decide anything"
        );
        assert_eq!(
            AgentKind::from_process("hermes", "hermes", &[]),
            Some(AgentKind::Hermes),
            "and the client, which is what a chat pane runs"
        );

        // OpenClaw ships `openclaw-gateway.service` as a systemd user unit and
        // runs under node.
        let openclaw_gateway = vec![
            String::from("node"),
            String::from("/usr/lib/node_modules/openclaw/dist/index.js"),
            String::from("gateway"),
        ];
        assert_eq!(
            AgentKind::from_process("node", "node", &openclaw_gateway),
            Some(AgentKind::OpenClaw)
        );
        assert_eq!(
            AgentKind::from_process("openclaw", "node", &[]),
            Some(AgentKind::OpenClaw)
        );
        assert_eq!(
            AgentKind::from_process("clawd", "clawd", &[]),
            Some(AgentKind::OpenClaw),
            "the name it shipped under before the rename"
        );
    }

    /// A substring search over a command line finds every process that merely
    /// mentions an agent, which on a developer's machine is a lot of them.
    #[test]
    fn talking_about_an_agent_is_not_running_one() {
        let editing = vec![String::from("vim"), String::from("openclaw.md")];
        assert_eq!(AgentKind::from_process("vim", "vim", &editing), None);

        let searching = vec![
            String::from("grep"),
            String::from("-rn"),
            String::from("hermes_cli"),
            String::from("."),
        ];
        assert_eq!(AgentKind::from_process("grep", "grep", &searching), None);

        // Hermes' own migration helper, which is about OpenClaw and is not it.
        let migrating = vec![
            String::from("hermes"),
            String::from("claw"),
            String::from("migrate"),
        ];
        assert_eq!(
            AgentKind::from_process("hermes", "hermes", &migrating),
            Some(AgentKind::Hermes),
            "it is Hermes, and specifically not OpenClaw"
        );

        let reading_a_note = vec![String::from("less"), String::from("notes-hermes_cli.txt")];
        assert_eq!(
            AgentKind::from_process("less", "less", &reading_a_note),
            None
        );
    }

    /// The wire values and the labels have to move together with the enum, and
    /// an unknown value still has to be refused rather than coerced.
    #[test]
    fn the_new_agents_round_trip_and_unknown_values_are_still_refused() {
        for kind in [AgentKind::Hermes, AgentKind::OpenClaw] {
            assert_eq!(AgentKind::from_wire(kind.wire_value()), Some(kind));
            assert!(!kind.display_label().is_empty());
            assert!(!kind.chat().command.is_empty());
        }
        assert_eq!(AgentKind::Hermes.display_label(), "Hermes");
        assert_eq!(AgentKind::OpenClaw.display_label(), "OpenClaw");
        assert_eq!(AgentKind::from_wire("hermes-agent"), None);
    }

    /// The finding this issue asked for, pinned so it cannot be quietly
    /// "corrected" back to the guess it started as.
    #[test]
    fn openclaw_attaches_and_hermes_starts_a_new_conversation() {
        assert_eq!(AgentKind::OpenClaw.chat().access, ChatAccess::Attach);
        assert_eq!(AgentKind::OpenClaw.chat().command, &["openclaw", "chat"]);

        assert_eq!(AgentKind::Hermes.chat().access, ChatAccess::NewSession);
        assert_eq!(AgentKind::Hermes.chat().command, &["hermes", "chat"]);
        assert!(
            AgentKind::Hermes
                .chat()
                .access
                .describe()
                .contains("not the one already running"),
            "the one genuinely bad outcome is implying otherwise"
        );
    }

    #[test]
    fn selects_deepest_then_newest_then_highest_pid() {
        let mut processes = vec![
            process(10, None, "zsh", None),
            process(20, Some(10), "claude", Some(1)),
            process(30, Some(20), "codex", Some(1)),
            process(40, Some(20), "pi", Some(2)),
            process(50, Some(20), "cursor-agent", Some(2)),
        ];
        processes[4].cwd = Some("/repo".into());

        assert_eq!(
            classify_pane_tree(10, &processes),
            Some(DetectedAgent {
                kind: AgentKind::Cursor,
                cwd: "/repo".into(),
            })
        );
    }

    #[test]
    fn one_scan_classifies_every_pane_identically_to_a_per_pane_scan() {
        // Two panes, two agents, plus filler that no matcher should ever touch
        // twice. `AgentScan` exists to move the matcher pass off the per-pane
        // path; it must not change a single verdict doing so.
        let mut processes = vec![
            process(10, None, "zsh", None),
            process(11, Some(10), "claude", Some(1)),
            process(20, None, "zsh", None),
            process(21, Some(20), "codex", Some(1)),
        ];
        processes[1].cwd = Some("/one".into());
        processes[3].cwd = Some("/two".into());
        processes.extend((100..160).map(|pid| process(pid, Some(1), "node", None)));

        let scan = AgentScan::new(&processes);
        for root in [10, 20, 999] {
            assert_eq!(
                scan.classify(root),
                classify_pane_tree(root, &processes),
                "shared scan must agree with a standalone one for root {root}"
            );
        }
        assert_eq!(
            scan.classify(10).map(|agent| agent.kind),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            scan.classify(20).map(|agent| agent.cwd),
            Some("/two".into())
        );
        assert_eq!(scan.classify(999), None);
    }

    #[test]
    fn a_pushed_status_expires_once_the_pane_is_back_at_its_prompt() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Pending,
            String::new(),
            now,
            1_000,
        );

        // Killing an agent mid-question fires no hook at all, so nothing else
        // would ever retire this row.
        tracker.observe_pane_liveness(false, now);
        assert_eq!(
            tracker.listed_agent().map(|listed| listed.state),
            Some(AgentState::Pending),
            "one sighting only starts the clock"
        );

        // Repeat sightings must not push expiry out — the pane *staying* at its
        // prompt is the evidence.
        tracker.observe_pane_liveness(false, now + PUSHED_STATUS_GRACE - Duration::from_secs(1));
        assert!(tracker.pushed.is_some());

        tracker.observe_pane_liveness(false, now + PUSHED_STATUS_GRACE);
        assert!(tracker.pushed.is_none(), "a dead producer stops asking");
        assert_eq!(tracker.listed_agent(), None);
    }

    #[test]
    fn a_live_pane_or_a_fresh_push_cancels_the_grace_clock() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now,
            1_000,
        );

        // A turn is legitimately between child processes for moments at a time.
        tracker.observe_pane_liveness(false, now);
        tracker.observe_pane_liveness(true, now + Duration::from_secs(1));
        tracker.observe_pane_liveness(false, now + Duration::from_secs(2));
        tracker.observe_pane_liveness(false, now + PUSHED_STATUS_GRACE);
        assert!(
            tracker.pushed.is_some(),
            "the clock restarted when the pane came back to life"
        );

        // A payload is proof the producer is alive, even mid-suspicion.
        tracker.observe_pane_liveness(false, now + PUSHED_STATUS_GRACE + Duration::from_secs(1));
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now + PUSHED_STATUS_GRACE + Duration::from_secs(2),
            1_000,
        );
        tracker.observe_pane_liveness(
            false,
            now + PUSHED_STATUS_GRACE + PUSHED_STATUS_GRACE + Duration::from_secs(1),
        );
        assert!(tracker.pushed.is_some());
    }

    #[test]
    fn scan_reports_whether_a_pane_is_running_anything() {
        let processes = vec![
            process(10, None, "zsh", None),
            process(11, Some(10), "claude", None),
            process(20, None, "zsh", None),
        ];
        let scan = AgentScan::new(&processes);
        assert!(scan.has_children(10), "pane 10 is running an agent");
        assert!(!scan.has_children(20), "pane 20 is back at its prompt");
        assert!(!scan.has_children(999));
    }

    #[test]
    fn has_owning_push_tracks_who_owns_the_state() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        assert!(!tracker.has_owning_push());

        tracker.update(Some(DetectedAgent {
            kind: AgentKind::Claude,
            cwd: "/repo".into(),
        }));
        assert!(
            !tracker.has_owning_push(),
            "the scan sees an agent, but nothing has reported what it is doing"
        );

        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now,
            1_000,
        );
        assert!(
            tracker.has_owning_push(),
            "a hooked agent reports its own state; sampling only has to watch it exit"
        );

        // An idle push hands the pane back to the scan, which knows only that
        // the process is there.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Idle,
            String::new(),
            now,
            1_000,
        );
        assert!(!tracker.has_owning_push());
    }

    #[test]
    fn ignores_agents_outside_the_pane_tree_and_cycles() {
        let processes = vec![
            process(10, None, "zsh", None),
            process(20, Some(99), "codex", None),
            process(30, Some(31), "claude", None),
            process(31, Some(30), "zsh", None),
        ];

        assert_eq!(classify_pane_tree(10, &processes), None);
    }

    #[test]
    fn a_pushed_status_outranks_detection_and_stands_alone() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();

        // No agent process matched: a hooked agent this build's matchers do not
        // recognize still gets a row.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Pending,
            String::new(),
            now,
            1_000,
        );
        let listed = tracker.listed_agent().expect("a pushed row stands alone");
        assert_eq!(listed.agent.kind, AgentKind::Claude);
        assert_eq!(listed.agent.cwd, "/repo");
        assert_eq!(listed.state, AgentState::Pending);

        // The scan found a different cwd for the same pane. The producer still
        // wins on both — it is the agent, and it knows where it is working.
        tracker.update(Some(DetectedAgent {
            kind: AgentKind::Claude,
            cwd: "/detected".into(),
        }));
        let listed = tracker.listed_agent().expect("still the pushed row");
        assert_eq!(listed.state, AgentState::Pending);
        assert_eq!(listed.agent.cwd, "/repo");

        // An idle push means "no activity", not "blank the row": the pane goes
        // back to what detection can see rather than losing a live agent.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Idle,
            String::new(),
            now,
            1_000,
        );
        let listed = tracker
            .listed_agent()
            .expect("the scanned agent keeps its row");
        assert_eq!(listed.agent.cwd, "/detected");
        assert_eq!(
            listed.state,
            AgentState::Unknown,
            "nothing is reporting on it any more, and saying `idle` would be a guess"
        );
    }

    #[test]
    fn consecutive_working_pushes_keep_one_interval() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();

        // A hooked agent pushes on every tool call. Those must read as one
        // continuous interval, or the overlay's elapsed clock resets every few
        // seconds and never shows how long the turn has really run.
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now,
            5_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            5_000
        );
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now + Duration::from_secs(3),
            8_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            5_000
        );

        // Finishing ends that interval and starts its own: "done 4s ago" is the
        // useful reading of a row that has stopped, and the next turn dates
        // from when that turn began rather than from this one.
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Done,
            String::new(),
            now + Duration::from_secs(4),
            9_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            9_000
        );
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now + Duration::from_secs(5),
            10_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            10_000
        );
    }

    /// A question mid-turn is its own episode, and the row has to say how long
    /// it has been asking. Before this the clock was zeroed for every state but
    /// `Working`, so the one row an inbox exists to surface — the one waiting on
    /// you — was the only row that could not tell you how long it had waited.
    #[test]
    fn a_pane_agent_waiting_on_you_counts_from_when_it_started_waiting() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();

        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now,
            5_000,
        );
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Pending,
            "may I run this?".into(),
            now + Duration::from_secs(2),
            7_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            7_000
        );

        // A second push in the same state does not restart the wait.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Pending,
            String::new(),
            now + Duration::from_secs(9),
            14_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            7_000
        );

        // Answering it starts the new working stretch from the answer, not from
        // the start of the turn the question interrupted.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
            String::new(),
            now + Duration::from_secs(10),
            15_000,
        );
        assert_eq!(
            tracker
                .listed_agent()
                .map_or(0, |listed| listed.working_since_unix_ms),
            15_000
        );
    }

    #[test]
    fn wire_vocabularies_round_trip_and_refuse_typos() {
        for kind in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::Pi,
            AgentKind::OpenCode,
        ] {
            assert_eq!(AgentKind::from_wire(kind.wire_value()), Some(kind));
        }
        assert_eq!(AgentKind::from_wire("Claude"), None);
        assert_eq!(AgentKind::from_wire("gemini"), None);

        for state in [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Done,
            AgentState::Pending,
            AgentState::Error,
        ] {
            assert_eq!(AgentState::from_wire(state.wire_value()), Some(state));
        }
        // Producers modelled on Claude's hook vocabulary say "running".
        assert_eq!(AgentState::from_wire("running"), Some(AgentState::Working));
        // A typo must never lenient-parse: folding it to idle would silently
        // erase the row the producer meant to update.
        assert_eq!(AgentState::from_wire("pendign"), None);
        assert_eq!(AgentState::from_wire(""), None);
    }

    #[test]
    fn global_snapshot_uses_the_injected_sampler() {
        struct FakeSampler;

        impl ProcessSampler for FakeSampler {
            fn snapshot(&mut self) -> Vec<ProcessSnapshot> {
                vec![process(1, None, "codex", None)]
            }
        }

        assert_eq!(sample_global_snapshot(&mut FakeSampler).len(), 1);
    }
}
