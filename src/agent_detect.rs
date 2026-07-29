//! Pure helpers for detecting supported coding agents in a hosted PTY tree.

use std::{
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

/// Classify a PTY session child process tree from one global process snapshot.
///
/// The deepest matching descendant wins, then the newest available start time,
/// then the highest PID. The PTY session child itself is included in the tree.
pub fn classify_pane_tree(
    session_child_pid: u32,
    processes: &[ProcessSnapshot],
) -> Option<DetectedAgent> {
    let mut candidates = Vec::new();

    for process in processes {
        let Some(kind) =
            AgentKind::from_process(&process.exe_basename, &process.name, &process.cmdline)
        else {
            continue;
        };
        let Some(depth) = descendant_depth(session_child_pid, process.pid, processes) else {
            continue;
        };
        candidates.push(Candidate {
            process,
            depth,
            kind,
        });
    }

    candidates
        .into_iter()
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

fn descendant_depth(root_pid: u32, pid: u32, processes: &[ProcessSnapshot]) -> Option<usize> {
    let mut current_pid = pid;
    let mut depth = 0;

    while current_pid != root_pid {
        let process = processes
            .iter()
            .find(|process| process.pid == current_pid)?;
        current_pid = process.parent_pid?;
        depth += 1;
        if depth > processes.len() {
            return None;
        }
    }

    Some(depth)
}

/// Coarse activity state shown in the agents overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Idle,
    Working,
    Done,
}

/// A just-finished agent retained for the done grace period.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoneAgent {
    pub kind: AgentKind,
    pub cwd: String,
    pub entered_done_at: Instant,
}

/// Per-pane agent state maintained between global sampler snapshots.
#[derive(Clone, Debug)]
pub struct PaneAgentTracker {
    pub last_output_at: Option<Instant>,
    pub active_agent: Option<DetectedAgent>,
    pub done_agent: Option<DoneAgent>,
    pub working_since: Option<Instant>,
    pub working_since_unix_ms: u64,
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
            tuning,
            settle_until: None,
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
