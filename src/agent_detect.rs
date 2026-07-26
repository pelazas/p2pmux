//! Pure helpers for detecting supported coding agents in a hosted PTY tree.

use std::time::{Duration, Instant};

use sysinfo::{ProcessesToUpdate, System};

const DONE_GRACE: Duration = Duration::from_secs(15);
const WORKING_WINDOW: Duration = Duration::from_secs(2);

/// A supported coding agent kind.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AgentKind {
    Claude,
    Codex,
    Cursor,
    Pi,
}

impl AgentKind {
    /// Match an exact, case-sensitive executable basename.
    pub fn from_basename(basename: &str) -> Option<Self> {
        match basename {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor-agent" => Some(Self::Cursor),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    /// Stable wire value for this agent kind.
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Pi => "pi",
        }
    }

    /// Human-readable label for the overlay.
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor Agent",
            Self::Pi => "Pi",
        }
    }
}

/// One process from a sampler snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub basename: String,
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
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.system
            .processes()
            .values()
            .map(|process| ProcessSnapshot {
                pid: process.pid().as_u32(),
                parent_pid: process.parent().map(|pid| pid.as_u32()),
                basename: process
                    .exe()
                    .and_then(|path| path.file_name())
                    .unwrap_or(process.name())
                    .to_string_lossy()
                    .into_owned(),
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
        let Some(kind) = AgentKind::from_basename(&process.basename) else {
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
#[derive(Clone, Debug, Default)]
pub struct PaneAgentTracker {
    pub last_output_at: Option<Instant>,
    pub active_agent: Option<DetectedAgent>,
    pub done_agent: Option<DoneAgent>,
}

impl PaneAgentTracker {
    /// Record PTY output for the working/idle state calculation.
    pub fn record_output(&mut self, now: Instant) {
        self.last_output_at = Some(now);
    }

    /// Apply this pane's latest process-tree classification.
    pub fn update(&mut self, detected: Option<DetectedAgent>, now: Instant) {
        match detected {
            Some(agent) => {
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
                if self
                    .done_agent
                    .as_ref()
                    .is_some_and(|done| now.duration_since(done.entered_done_at) > DONE_GRACE)
                {
                    self.done_agent = None;
                }
            }
        }
    }

    /// Return the current agent and coarse state, if the pane should be listed.
    pub fn listed_agent(&self, now: Instant) -> Option<(DetectedAgent, AgentState)> {
        if let Some(agent) = &self.active_agent {
            let state = if self
                .last_output_at
                .is_some_and(|last_output| now.duration_since(last_output) <= WORKING_WINDOW)
            {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(
        pid: u32,
        parent_pid: Option<u32>,
        basename: &str,
        start_time: Option<u64>,
    ) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            parent_pid,
            basename: basename.into(),
            start_time,
            cwd: None,
        }
    }

    #[test]
    fn allowlist_matches_only_exact_basenames() {
        assert_eq!(AgentKind::from_basename("claude"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::from_basename("codex"), Some(AgentKind::Codex));
        assert_eq!(
            AgentKind::from_basename("cursor-agent"),
            Some(AgentKind::Cursor)
        );
        assert_eq!(AgentKind::from_basename("pi"), Some(AgentKind::Pi));
        assert_eq!(AgentKind::from_basename("cursor"), None);
        assert_eq!(AgentKind::from_basename("Codex"), None);
        assert_eq!(AgentKind::Claude.display_label(), "Claude Code");
        assert_eq!(AgentKind::Cursor.wire_value(), "cursor");
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
        tracker.update(Some(agent.clone()), now);
        assert_eq!(
            tracker.listed_agent(now),
            Some((agent.clone(), AgentState::Idle))
        );

        tracker.record_output(now);
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(2)),
            Some((agent.clone(), AgentState::Working))
        );
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(3)),
            Some((agent.clone(), AgentState::Idle))
        );

        tracker.update(None, now + Duration::from_secs(4));
        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(19)),
            Some((agent.clone(), AgentState::Done))
        );
        tracker.update(None, now + Duration::from_secs(20));
        assert_eq!(tracker.listed_agent(now + Duration::from_secs(20)), None);
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
        );
        tracker.update(None, now + Duration::from_secs(1));
        tracker.update(
            Some(DetectedAgent {
                kind: AgentKind::Pi,
                cwd: "/new".into(),
            }),
            now + Duration::from_secs(2),
        );

        assert_eq!(
            tracker.listed_agent(now + Duration::from_secs(2)),
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
