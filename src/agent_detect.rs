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
        self.system
            .processes()
            .values()
            .map(|process| ProcessSnapshot {
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
            })
            .collect()
    }
}

/// Collect one global snapshot through an injected sampler.
pub fn sample_global_snapshot(sampler: &mut dyn ProcessSampler) -> Vec<ProcessSnapshot> {
    sampler.snapshot()
}

/// The supported agent selected from a hosted pane's process tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    pub cwd: String,
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
    /// A `Working` push carries its interval start forward from the previous
    /// `Working` push, so the per-tool-call stream a hooked agent emits shows
    /// one continuous interval rather than restarting the clock every few
    /// seconds.
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
        let working_since_unix_ms = match state {
            AgentState::Working => previous
                .filter(|(state, ..)| *state == AgentState::Working)
                .map(|(_, since, _)| since)
                .unwrap_or(unix_ms_now),
            _ => 0,
        };
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

        // Finishing drops the interval; the next turn starts a fresh one.
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
            0
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
