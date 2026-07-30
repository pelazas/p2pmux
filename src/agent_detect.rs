//! Pure helpers for detecting supported coding agents in a hosted PTY tree.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const DONE_GRACE: Duration = Duration::from_secs(15);
/// Output this recent starts a working interval. Entering must be fast so the overlay reacts
/// as soon as an agent does anything.
const WORKING_ENTER: Duration = Duration::from_secs(2);
/// Silence this long ends a working interval. Leaving must be slow because an agent that is
/// still working goes quiet for many seconds at a time — waiting on a model response, or
/// running a tool that streams nothing. Treating a short pause as "finished" is what made the
/// completion notification fire mid-task.
pub const DEFAULT_QUIET_BEFORE_DONE: Duration = Duration::from_secs(20);
/// After an agent signals completion, ignore output this recent for the purpose of starting a
/// new working interval. Agents ring the bell and then repaint their prompt, and that trailing
/// redraw would otherwise open a fresh interval that has to time out all over again.
const COMPLETION_SETTLE: Duration = Duration::from_secs(3);
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

/// Local tuning for when an agent counts as finished.
///
/// This module stays free of configuration IO: the values are handed to it during startup by
/// whoever read the config file, so the detection logic remains pure and testable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationTuning {
    pub quiet_before_done: Duration,
    /// Only end a working interval on an explicit completion signal. Removes every false
    /// positive at the cost of showing an agent as working indefinitely when it never rings.
    pub require_bell: bool,
}

impl Default for NotificationTuning {
    fn default() -> Self {
        Self {
            quiet_before_done: DEFAULT_QUIET_BEFORE_DONE,
            require_bell: false,
        }
    }
}

static NOTIFICATION_TUNING: OnceLock<NotificationTuning> = OnceLock::new();

/// Install this process's tuning. Called once during startup; later calls are ignored.
pub fn set_notification_tuning(tuning: NotificationTuning) {
    let _ = NOTIFICATION_TUNING.set(tuning);
}

fn notification_tuning() -> NotificationTuning {
    NOTIFICATION_TUNING.get().copied().unwrap_or_default()
}

/// A supported coding agent kind.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AgentKind {
    Claude,
    Codex,
    Cursor,
    Pi,
    OpenCode,
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
            _ => None,
        }
    }

    /// Human-readable label for the overlay.
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor Agent",
            Self::Pi => "Pi",
            Self::OpenCode => "OpenCode",
        }
    }
}

fn matches_launcher(exe_basename: &str, name: &str, cmdline: &[String], launcher: &str) -> bool {
    exe_basename == launcher
        || name == launcher
        || cmdline.first().is_some_and(|argv0| argv0 == launcher)
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

/// `sysinfo`-backed process sampler used by the macOS host runtime.
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
/// `Idle`, `Working`, and `Done` are inferable from PTY output timing.
/// `Pending` and `Error` are not, and never will be: silence looks identical
/// whether an agent is thinking or waiting on a permission prompt. They only
/// ever arrive from a producer inside the pane — see
/// [`PaneAgentTracker::record_pushed_status`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
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

/// A just-finished agent retained for the done grace period.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoneAgent {
    pub kind: AgentKind,
    pub cwd: String,
    pub entered_done_at: Instant,
}

/// A status reported by a producer running inside the pane — an agent hook,
/// not an inference. Authoritative while present: the agent said so.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushedAgent {
    pub kind: AgentKind,
    pub cwd: String,
    pub state: AgentState,
    /// When this status arrived. The expiry policy reads it.
    pub at: Instant,
    /// Start of the current working interval, carried across pushes that stay
    /// `Working` so a per-tool-call stream of updates does not reset the clock.
    pub working_since_unix_ms: u64,
}

/// Per-pane agent state maintained between global sampler snapshots.
#[derive(Clone, Debug)]
pub struct PaneAgentTracker {
    pub last_output_at: Option<Instant>,
    pub active_agent: Option<DetectedAgent>,
    pub done_agent: Option<DoneAgent>,
    pub working_since: Option<Instant>,
    pub working_since_unix_ms: u64,
    /// Latest producer-pushed status, when one is running in this pane.
    pub pushed: Option<PushedAgent>,
    /// When this pane was first seen sitting at its shell prompt while a pushed
    /// status was still standing. `None` cancels the grace clock.
    push_suspect_since: Option<Instant>,
    tuning: NotificationTuning,
    settle_until: Option<Instant>,
}

impl Default for PaneAgentTracker {
    fn default() -> Self {
        Self::with_tuning(notification_tuning())
    }
}

impl PaneAgentTracker {
    pub fn with_tuning(tuning: NotificationTuning) -> Self {
        Self {
            last_output_at: None,
            active_agent: None,
            done_agent: None,
            working_since: None,
            working_since_unix_ms: 0,
            pushed: None,
            push_suspect_since: None,
            tuning,
            settle_until: None,
        }
    }

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
    /// A push outranks every inference this module makes, because the producer
    /// is the agent: it knows it is blocked on a permission prompt, and no
    /// amount of output timing can tell that apart from waiting on a model.
    ///
    /// A `Working` push carries its interval start forward from the previous
    /// `Working` push, so the per-tool-call stream a hooked agent emits shows
    /// one continuous interval rather than restarting the clock every few
    /// seconds.
    pub fn record_pushed_status(
        &mut self,
        kind: AgentKind,
        cwd: String,
        state: AgentState,
        now: Instant,
        unix_ms_now: u64,
    ) {
        let continuing = self
            .pushed
            .as_ref()
            .filter(|pushed| pushed.state == AgentState::Working && pushed.kind == kind)
            .map(|pushed| pushed.working_since_unix_ms);
        let working_since_unix_ms = match state {
            AgentState::Working => continuing.unwrap_or(unix_ms_now),
            _ => 0,
        };
        self.pushed = Some(PushedAgent {
            kind,
            cwd,
            state,
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

    /// Working-interval start for whichever source currently owns the state.
    pub fn reported_working_since_unix_ms(&self) -> u64 {
        match self.owning_push() {
            Some(pushed) => pushed.working_since_unix_ms,
            None => self.working_since_unix_ms,
        }
    }

    /// Record PTY output for the working/idle state calculation.
    pub fn record_output(&mut self, now: Instant, unix_ms_now: u64) {
        self.last_output_at = Some(now);
        self.reconcile_working_state(now, unix_ms_now);
    }

    /// Record an explicit completion signal from the agent (a terminal bell).
    ///
    /// This ends the working interval immediately rather than waiting out the quiet window,
    /// which is the whole point of preferring it: the timing heuristic can only ever guess when
    /// an agent stopped, while the bell says so.
    pub fn record_completion_signal(&mut self, now: Instant) {
        if self.active_agent.is_none() {
            return;
        }
        self.last_output_at = None;
        self.settle_until = Some(now + COMPLETION_SETTLE);
        self.clear_working_state();
    }

    /// Apply this pane's latest process-tree classification.
    pub fn update(&mut self, detected: Option<DetectedAgent>, now: Instant, unix_ms_now: u64) {
        match detected {
            Some(agent) => {
                if self
                    .active_agent
                    .as_ref()
                    .is_some_and(|active| active != &agent)
                {
                    self.last_output_at = None;
                    self.settle_until = None;
                    self.clear_working_state();
                }
                self.active_agent = Some(agent);
                self.done_agent = None;
            }
            None => {
                if let Some(agent) = self.active_agent.take() {
                    self.done_agent = Some(DoneAgent {
                        kind: agent.kind,
                        cwd: agent.cwd,
                        entered_done_at: now,
                    });
                }
                self.last_output_at = None;
                self.clear_working_state();
                if self
                    .done_agent
                    .as_ref()
                    .is_some_and(|done| now.duration_since(done.entered_done_at) > DONE_GRACE)
                {
                    self.done_agent = None;
                }
            }
        }
        self.reconcile_working_state(now, unix_ms_now);
    }

    /// Return the current agent and coarse state, if the pane should be listed.
    pub fn listed_agent(
        &mut self,
        now: Instant,
        unix_ms_now: u64,
    ) -> Option<(DetectedAgent, AgentState)> {
        self.reconcile_working_state(now, unix_ms_now);
        // A producer inside the pane outranks detection, and stands alone: a
        // hooked agent this build's process matchers do not recognize still
        // gets a row, which is the whole point of accepting pushes.
        if let Some(pushed) = self.owning_push() {
            return Some((
                DetectedAgent {
                    kind: pushed.kind,
                    cwd: pushed.cwd.clone(),
                },
                pushed.state,
            ));
        }
        if let Some(agent) = &self.active_agent {
            let state = if self.working_since.is_some() {
                AgentState::Working
            } else {
                AgentState::Idle
            };
            return Some((agent.clone(), state));
        }

        self.done_agent
            .as_ref()
            .filter(|done| now.duration_since(done.entered_done_at) <= DONE_GRACE)
            .map(|done| {
                (
                    DetectedAgent {
                        kind: done.kind,
                        cwd: done.cwd.clone(),
                    },
                    AgentState::Done,
                )
            })
    }

    /// Working state uses hysteresis: it is entered on any recent output but only left after a
    /// much longer silence. One symmetric threshold cannot serve both, because "how fast we
    /// notice work" and "how sure we are it stopped" want opposite values.
    fn reconcile_working_state(&mut self, now: Instant, unix_ms_now: u64) {
        if self.active_agent.is_none() {
            self.clear_working_state();
            return;
        }
        let Some(last_output) = self.last_output_at else {
            self.clear_working_state();
            return;
        };
        let quiet_for = now.duration_since(last_output);
        if self.working_since.is_some() {
            if !self.tuning.require_bell && quiet_for >= self.tuning.quiet_before_done {
                self.clear_working_state();
            }
        } else if quiet_for <= WORKING_ENTER && !self.settling(now) {
            self.working_since = Some(now);
            self.working_since_unix_ms = unix_ms_now;
        }
    }

    fn settling(&mut self, now: Instant) -> bool {
        match self.settle_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.settle_until = None;
                false
            }
            None => false,
        }
    }

    fn clear_working_state(&mut self) {
        self.working_since = None;
        self.working_since_unix_ms = 0;
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
            now,
            1_000,
        );

        // Killing an agent mid-question fires no hook at all, so nothing else
        // would ever retire this row.
        tracker.observe_pane_liveness(false, now);
        assert_eq!(
            tracker.listed_agent(now, 1_000).map(|(_, state)| state),
            Some(AgentState::Pending),
            "one sighting only starts the clock"
        );

        // Repeat sightings must not push expiry out — the pane *staying* at its
        // prompt is the evidence.
        tracker.observe_pane_liveness(false, now + PUSHED_STATUS_GRACE - Duration::from_secs(1));
        assert!(tracker.pushed.is_some());

        tracker.observe_pane_liveness(false, now + PUSHED_STATUS_GRACE);
        assert!(tracker.pushed.is_none(), "a dead producer stops asking");
        assert_eq!(tracker.listed_agent(now + PUSHED_STATUS_GRACE, 1_000), None);
    }

    #[test]
    fn a_live_pane_or_a_fresh_push_cancels_the_grace_clock() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
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

        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Claude,
                cwd: "/repo".into(),
            }),
            now,
            1_000,
        );
        assert!(
            !tracker.has_owning_push(),
            "a detected agent alone is inference, and inference needs fast sampling"
        );

        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Working,
            now,
            1_000,
        );
        assert!(
            tracker.has_owning_push(),
            "a hooked agent reports its own state; sampling only has to watch it exit"
        );

        // An idle push hands the pane back to inference, so the fast cadence
        // has to come back with it.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Idle,
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
    fn tracker_marks_working_idle_done_and_expires_done_grace() {
        let now = Instant::now();
        let agent = DetectedAgent {
            kind: AgentKind::Codex,
            cwd: "/repo".into(),
        };
        let mut tracker = PaneAgentTracker::default();
        tracker.update(Some(agent.clone()), now, 1_000);
        assert_eq!(
            tracker.listed_agent(now, 1_000),
            Some((agent.clone(), AgentState::Idle))
        );

        tracker.record_output(now, 1_001);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(2), 3_001),
            Some((agent.clone(), AgentState::Working))
        );
        assert_eq!(
            tracker.listed_agent(now + DEFAULT_QUIET_BEFORE_DONE, 21_001),
            Some((agent.clone(), AgentState::Idle))
        );

        tracker.update(None, now + Duration::from_secs(21), 22_001);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(35), 36_001),
            Some((agent.clone(), AgentState::Done))
        );
        tracker.update(None, now + Duration::from_secs(37), 38_001);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(37), 38_001),
            None
        );
    }

    #[test]
    fn a_pause_shorter_than_the_quiet_window_stays_working() {
        let now = Instant::now();
        let agent = DetectedAgent {
            kind: AgentKind::Claude,
            cwd: "/repo".into(),
        };
        let mut tracker = PaneAgentTracker::default();
        tracker.update(Some(agent.clone()), now, 1_000);
        tracker.record_output(now, 1_000);

        // An agent waiting on a model response or a silent tool call goes quiet for many
        // seconds without having finished. Every one of these used to report Idle, which is
        // what rang the completion sound mid-task.
        for pause_secs in [3, 5, 10, 19] {
            assert_eq!(
                tracker.listed_agent(now + Duration::from_secs(pause_secs), 1_000),
                Some((agent.clone(), AgentState::Working)),
                "a {pause_secs}s pause must not read as finished"
            );
        }

        assert_eq!(
            tracker.listed_agent(now + DEFAULT_QUIET_BEFORE_DONE, 1_000),
            Some((agent.clone(), AgentState::Idle))
        );
    }

    #[test]
    fn output_during_a_pause_extends_the_working_interval_without_restarting_it() {
        let now = Instant::now();
        let agent = DetectedAgent {
            kind: AgentKind::Codex,
            cwd: "/repo".into(),
        };
        let mut tracker = PaneAgentTracker::default();
        tracker.update(Some(agent.clone()), now, 1_000);
        tracker.record_output(now, 1_000);
        let first_episode = tracker.working_since_unix_ms;

        // Output at 15s resets the silence clock, so the interval must survive past the
        // original 20s deadline and keep the same episode start.
        tracker.record_output(now + Duration::from_secs(15), 16_000);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(30), 31_000),
            Some((agent.clone(), AgentState::Working))
        );
        assert_eq!(tracker.working_since_unix_ms, first_episode);

        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(35), 36_000),
            Some((agent, AgentState::Idle))
        );
    }

    #[test]
    fn a_completion_signal_ends_the_working_interval_immediately() {
        let now = Instant::now();
        let agent = DetectedAgent {
            kind: AgentKind::Claude,
            cwd: "/repo".into(),
        };
        let mut tracker = PaneAgentTracker::default();
        tracker.update(Some(agent.clone()), now, 1_000);
        tracker.record_output(now, 1_000);
        assert_eq!(
            tracker.listed_agent(now, 1_000),
            Some((agent.clone(), AgentState::Working))
        );

        // The bell says the agent is done, so there is no need to wait out the quiet window.
        tracker.record_completion_signal(now + Duration::from_secs(1));
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(1), 2_000),
            Some((agent.clone(), AgentState::Idle))
        );

        // Agents repaint their prompt right after ringing. That trailing output must not open
        // a new interval, or the completion would have to time out all over again.
        tracker.record_output(now + Duration::from_secs(2), 3_000);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(2), 3_000),
            Some((agent.clone(), AgentState::Idle))
        );

        // Once settled, real new work is tracked normally.
        tracker.record_output(now + Duration::from_secs(10), 11_000);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(10), 11_000),
            Some((agent, AgentState::Working))
        );
    }

    #[test]
    fn require_bell_never_finishes_on_the_silence_timer_alone() {
        let now = Instant::now();
        let agent = DetectedAgent {
            kind: AgentKind::Claude,
            cwd: "/repo".into(),
        };
        let mut tracker = PaneAgentTracker::with_tuning(NotificationTuning {
            quiet_before_done: Duration::from_secs(20),
            require_bell: true,
        });
        tracker.update(Some(agent.clone()), now, 1_000);
        tracker.record_output(now, 1_000);

        // An hour of silence is not a completion when the user asked for explicit signals only.
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(3_600), 1_000),
            Some((agent.clone(), AgentState::Working))
        );

        tracker.record_completion_signal(now + Duration::from_secs(3_601));
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(3_601), 1_000),
            Some((agent, AgentState::Idle))
        );
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
            now,
            1_000,
        );
        assert_eq!(
            tracker.listed_agent(now, 1_000),
            Some((
                DetectedAgent {
                    kind: AgentKind::Claude,
                    cwd: "/repo".into(),
                },
                AgentState::Pending,
            ))
        );

        // Detection says "working" (the pane is chattering); the producer says
        // the agent is blocked. The producer wins — that is the whole point.
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Claude,
                cwd: "/detected".into(),
            }),
            now,
            1_000,
        );
        tracker.record_output(now, 1_000);
        assert_eq!(
            tracker.listed_agent(now, 1_000).map(|(_, state)| state),
            Some(AgentState::Pending)
        );

        // An idle push means "no activity", not "blank the row": the pane goes
        // back to what detection can see rather than losing a live agent.
        tracker.record_pushed_status(
            AgentKind::Claude,
            "/repo".into(),
            AgentState::Idle,
            now,
            1_000,
        );
        assert_eq!(
            tracker.listed_agent(now, 1_000),
            Some((
                DetectedAgent {
                    kind: AgentKind::Claude,
                    cwd: "/detected".into(),
                },
                AgentState::Working,
            ))
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
            now,
            5_000,
        );
        assert_eq!(tracker.reported_working_since_unix_ms(), 5_000);
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Working,
            now + Duration::from_secs(3),
            8_000,
        );
        assert_eq!(tracker.reported_working_since_unix_ms(), 5_000);

        // Finishing drops the interval; the next turn starts a fresh one.
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Done,
            now + Duration::from_secs(4),
            9_000,
        );
        assert_eq!(tracker.reported_working_since_unix_ms(), 0);
        tracker.record_pushed_status(
            AgentKind::Codex,
            "/repo".into(),
            AgentState::Working,
            now + Duration::from_secs(5),
            10_000,
        );
        assert_eq!(tracker.reported_working_since_unix_ms(), 10_000);
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
    fn a_completion_signal_without_an_agent_is_ignored() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.record_output(now, 1_000);
        tracker.record_completion_signal(now);

        // A plain shell ringing the bell must not manufacture agent state.
        assert_eq!(tracker.listed_agent(now, 1_000), None);
        assert_eq!(tracker.last_output_at, Some(now));
    }

    #[test]
    fn replacement_clears_done_agent_immediately() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Claude,
                cwd: "/old".into(),
            }),
            now,
            1_000,
        );
        tracker.update(None, now + Duration::from_secs(1), 2_000);
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Pi,
                cwd: "/new".into(),
            }),
            now + Duration::from_secs(2),
            3_000,
        );

        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(2), 3_000),
            Some((
                DetectedAgent {
                    kind: AgentKind::Pi,
                    cwd: "/new".into(),
                },
                AgentState::Idle,
            ))
        );
    }

    #[test]
    fn working_interval_tracks_transitions_without_refreshing() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Codex,
                cwd: "/repo".into(),
            }),
            now,
            1_000,
        );

        tracker.record_output(now, 1_001);
        let started_at = tracker.working_since;
        assert_eq!(tracker.working_since_unix_ms, 1_001);

        tracker.record_output(now + Duration::from_secs(1), 2_001);
        assert_eq!(tracker.working_since, started_at);
        assert_eq!(tracker.working_since_unix_ms, 1_001);

        // The interval ends only once the pane has been quiet for the full window, measured
        // from the most recent output rather than from when the interval began.
        tracker.listed_agent(now + Duration::from_secs(4), 5_001);
        assert_eq!(tracker.working_since, started_at);

        tracker.listed_agent(
            now + Duration::from_secs(1) + DEFAULT_QUIET_BEFORE_DONE,
            22_001,
        );
        assert_eq!(tracker.working_since, None);
        assert_eq!(tracker.working_since_unix_ms, 0);
    }

    #[test]
    fn agent_replacement_resets_working_interval() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Codex,
                cwd: "/old".into(),
            }),
            now,
            1_000,
        );
        tracker.record_output(now, 1_001);
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Claude,
                cwd: "/new".into(),
            }),
            now + Duration::from_secs(1),
            2_001,
        );

        assert_eq!(tracker.working_since, None);
        assert_eq!(tracker.working_since_unix_ms, 0);
    }

    #[test]
    fn done_clears_working_interval() {
        let now = Instant::now();
        let mut tracker = PaneAgentTracker::default();
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Codex,
                cwd: "/repo".into(),
            }),
            now,
            1_000,
        );
        tracker.record_output(now, 1_001);
        tracker.update(None, now + Duration::from_secs(1), 2_001);

        assert_eq!(tracker.working_since, None);
        assert_eq!(tracker.working_since_unix_ms, 0);
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
