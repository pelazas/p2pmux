//! The agent-hook producer: a hook payload on stdin, one pushed status out.
//!
//! This runs as its own short-lived process (`p2pmux notify claude`), spawned
//! by the agent's hook runner rather than by the mux. It reads the hook JSON,
//! decides a status, writes one line to the pane's node socket, and exits.
//!
//! Two properties matter more than anything else here, because this code runs
//! on the agent's critical path — `UserPromptSubmit` blocks the user's prompt,
//! and the tool hooks fire on every single tool call:
//!
//! - **It never errors into the agent.** Every failure — no session, no
//!   socket, unparseable payload, a node that vanished — is a silent success.
//!   A hook that fails is a hook the user disables.
//! - **It never blocks.** One connect and one small write, both with timeouts.
//!
//! Note what is *not* sent: the user's prompt and the tool being run. Those
//! reach this process and are used only to decide the status.
//!
//! One line of the assistant's message *is* sent, and travels exactly as far as
//! the Unix socket this writes to. The node that owns the pane keeps it for its
//! own inbox and strips it from the roster it publishes — see
//! `SharedLocalPane::agent_roster_entry`. A p2pmux session is shared with every
//! member, and the agent's conversation is still not the mux's to publish; it is
//! only the local user's to read, on their own machine, about their own pane.

use std::{
    error::Error,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use serde_json::Value;

use crate::{
    agent_detect::{AgentKind, AgentState},
    agent_status::AgentStatusRecord,
    local_ipc::ClientMessage,
    pty_host::{PANE_ID_ENV, SOCKET_ENV},
};

/// Cap on the hook payload read from stdin. A degenerate multi-gigabyte stream
/// must bound memory rather than buffer whole; a truncated read simply fails to
/// parse and no-ops, which is the safe degradation.
const MAX_STDIN_BYTES: u64 = 8 * 1024 * 1024;

/// Bound on the socket write. The node accepts on its next loop iteration and
/// the message is one short line, so this never trips in practice — it exists
/// so a wedged node can never hold an agent's prompt open.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Claude's placeholder notification text. These carry no information about
/// what is actually being asked, and firing "needs you" for them trains the
/// user to ignore the state that matters most.
const GENERIC_PENDING: [&str; 2] = ["Claude needs attention", "Claude Code needs your attention"];

/// Longest activity message a hook will forward.
///
/// The node caps it again on arrival; this one keeps the socket write small on
/// the agent's critical path.
const MAX_MESSAGE_CHARS: usize = 160;

/// What a hook payload resolved to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookUpdate {
    pub state: AgentState,
    pub cwd: String,
    /// One line of what the agent is doing, for the local overlay only.
    pub message: String,
}

/// The first line of an agent's message, bounded.
///
/// Only the first line: these are prose replies that run to paragraphs, and the
/// overlay has one line to show. Only the first [`MAX_MESSAGE_CHARS`]: the rest
/// would be truncated by the renderer anyway, and there is no reason to put it
/// on a socket to find that out.
fn summarize(message: &str) -> String {
    let line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((end, _)) => format!("{}…", line[..end].trim_end()),
        None => line.to_owned(),
    }
}

/// Map a Claude hook event name to a status.
///
/// `Error` is deliberately unreachable here: Claude's hook vocabulary has no
/// turn-level failure signal. `PostToolUse` carries a per-tool `is_error`, but
/// a failed tool call mid-turn is ordinary agent behaviour — it recovers and
/// continues — so mapping it to `Error` would paint healthy turns red.
fn state_from_event(event: &str) -> Option<AgentState> {
    match event {
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SubagentStop" => {
            Some(AgentState::Working)
        }
        "Notification" => Some(AgentState::Pending),
        "Stop" => Some(AgentState::Done),
        "SessionEnd" => Some(AgentState::Idle),
        _ => None,
    }
}

/// Whether the last non-blank line of `message` ends in a question mark.
///
/// A question mark anywhere else does not count: "Want anything else? All tests
/// pass." is a turn that finished on a statement.
fn ends_with_a_question(message: &str) -> bool {
    message
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.trim_end().ends_with('?'))
}

/// Map an opencode plugin event name to a status.
///
/// opencode's vocabulary is an event bus rather than a hook list, so the plugin
/// subscribes to the four that say something about who the turn is waiting on.
/// `session.error` is the one Claude has no equivalent for: opencode reports a
/// turn that failed, and a failed turn is not a finished one.
fn state_from_opencode_event(event: &str) -> Option<AgentState> {
    match event {
        "message.updated" | "tool.execute.before" | "tool.execute.after" => {
            Some(AgentState::Working)
        }
        "permission.asked" | "permission.updated" => Some(AgentState::Pending),
        "session.idle" => Some(AgentState::Done),
        "session.error" => Some(AgentState::Error),
        "session.deleted" => Some(AgentState::Idle),
        _ => None,
    }
}

/// Decide what an opencode plugin payload means. `None` is a deliberate no-op.
///
/// The payload is the plugin's own JSON — `{"event":…,"message":…,"cwd":…}` —
/// rather than Claude's hook envelope, but everything downstream of the state
/// is identical, including the rule that a "needs you" with nothing to say is
/// dropped: opencode raises `permission.asked` for every tool call it is
/// configured to ask about, and a badge that lights without saying what for is
/// a badge people learn to ignore.
pub fn derive_opencode(raw: &str, status_arg: Option<&str>) -> Option<HookUpdate> {
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let state = match status_arg {
        Some(argument) => AgentState::from_wire(argument)?,
        None => state_from_opencode_event(payload.get("event").and_then(Value::as_str)?)?,
    };

    finish(state, message, &payload)
}

/// The half of a derivation that no agent's vocabulary changes: drop an empty
/// "needs you", read the working directory, and bound the message.
fn finish(state: AgentState, message: &str, payload: &Value) -> Option<HookUpdate> {
    if state == AgentState::Pending && message.trim().is_empty() {
        return None;
    }

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    Some(HookUpdate {
        state,
        cwd,
        message: summarize(message),
    })
}

/// Decide what a Claude hook payload means. `None` is a deliberate no-op.
///
/// `status_arg` comes from the hook registration (`notify.sh running`) and wins
/// over the payload's own event name, so one script serves every hook.
pub fn derive_claude(raw: &str, status_arg: Option<&str>) -> Option<HookUpdate> {
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
        })
        .unwrap_or_default();

    let mut state = match status_arg {
        // Strict: an unknown `--status` is refused, never folded to idle. A
        // typo in a hook registration would otherwise blank every row it
        // touched, and the user would have no idea why.
        Some(argument) => AgentState::from_wire(argument)?,
        None => state_from_event(payload.get("hook_event_name").and_then(Value::as_str)?)?,
    };

    // A turn that ends by asking something is blocked on a human, not
    // finished. Claude surfaces tool-permission questions as `Notification`,
    // but a plain prose question just fires `Stop` — which would show green
    // and read as safe to ignore.
    if state == AgentState::Done && ends_with_a_question(message) {
        state = AgentState::Pending;
    }

    // Claude's own placeholder text says nothing about what is being asked, so
    // it is dropped here on top of the empty-message rule `finish` applies to
    // every agent.
    if state == AgentState::Pending && GENERIC_PENDING.contains(&message.trim()) {
        return None;
    }

    finish(state, message, &payload)
}

/// The pane this process is running in, and the node socket to report to.
/// `None` outside a p2pmux pane, which is what makes the hook safe to leave
/// enabled everywhere.
fn pane_and_socket() -> Option<(u64, PathBuf)> {
    let pane_id = std::env::var(PANE_ID_ENV).ok()?.parse().ok()?;
    let socket = std::env::var_os(SOCKET_ENV)?;
    Some((pane_id, PathBuf::from(socket)))
}

/// Read the hook payload from stdin, bounded.
fn read_payload() -> String {
    let mut raw = String::new();
    let _ = std::io::stdin()
        .lock()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut raw);
    raw
}

/// Send one pushed status to the node that owns this pane.
fn send(pane_id: u64, socket: &PathBuf, kind: AgentKind, update: HookUpdate) -> Option<()> {
    let message = ClientMessage::AgentStatus {
        pane_id,
        kind: kind.wire_value().to_owned(),
        status: update.state.wire_value().to_owned(),
        cwd: update.cwd,
        message: update.message,
    };
    let mut line = serde_json::to_vec(&message).ok()?;
    line.push(b'\n');

    let stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok()?;
    (&stream).write_all(&line).ok()
}

/// Report a status for an agent that is not running in a p2pmux pane.
///
/// There is no pane to name and no node socket to write to — the mux did not
/// start this agent and may not be running at all — so the status goes to the
/// machine-local record the process scan reads, keyed by the agent's own pid.
/// See [`crate::agent_status`] for why that is a file and not a socket.
///
/// The agent is found by walking up from this process: a hook runner is a child
/// of the agent that spawned it. Nothing is written when no agent is found —
/// the row this would name does not exist either.
fn report_outside_a_pane(kind: AgentKind, update: HookUpdate) -> Option<()> {
    let (pid, _, start_time) = crate::agent_detect::agent_ancestor(std::process::id(), kind)?;
    let directory = crate::agent_status::default_dir()?;

    // An agent that has stopped hands its row back to the process scan, which
    // is what an absent record means. Nothing else expires one: a loose agent
    // *is* its process, so the row lives exactly as long as the agent does.
    if update.state == AgentState::Idle {
        let _ = std::fs::remove_file(directory.join(format!("{pid}.json")));
        return Some(());
    }

    let previous = crate::agent_status::read(directory, pid, Some(start_time));
    let record = merged_record(previous, kind, update, start_time, unix_ms_now());
    crate::agent_status::write(directory, pid, &record).ok()
}

/// This hook's status, folded onto the one before it.
///
/// The same two carries the pane path makes in `record_pushed_status`, for the
/// same reasons: a stream of per-tool-call pushes in one state is one interval
/// rather than a clock that restarts every few seconds, and a hook with nothing
/// to say leaves the last thing the agent said standing. Kept a pure function
/// because that is the only part of writing a record worth asserting on.
fn merged_record(
    previous: Option<AgentStatusRecord>,
    kind: AgentKind,
    update: HookUpdate,
    start_time: u64,
    unix_ms_now: u64,
) -> AgentStatusRecord {
    // A record filed under another kind belongs to whatever ran in this process
    // before, and carrying its clock forward would date this agent's turn from
    // the last one's.
    let previous = previous.filter(|record| record.kind == kind.wire_value());
    let working_since_unix_ms = previous
        .as_ref()
        .filter(|record| record.state == update.state.wire_value())
        .map(|record| record.working_since_unix_ms)
        .filter(|since| *since != 0)
        .unwrap_or(unix_ms_now);
    let message = if update.message.is_empty() {
        previous.map(|record| record.message).unwrap_or_default()
    } else {
        update.message
    };
    AgentStatusRecord {
        kind: kind.wire_value().to_owned(),
        state: update.state.wire_value().to_owned(),
        cwd: update.cwd,
        message,
        working_since_unix_ms,
        start_time,
    }
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Run the producer for one hook invocation.
///
/// Always `Ok`. Every failure path is a silent no-op by design — see the module
/// documentation.
pub fn run(kind: AgentKind, status_arg: Option<&str>) -> Result<(), Box<dyn Error>> {
    let raw = read_payload();
    let update = match kind {
        AgentKind::Claude => derive_claude(&raw, status_arg),
        AgentKind::OpenCode => derive_opencode(&raw, status_arg),
        // The rest have no hook vocabulary wired up yet, but `--status`
        // already makes this usable from any script that knows its own state.
        _ => status_arg
            .and_then(AgentState::from_wire)
            .map(|state| HookUpdate {
                state,
                cwd: std::env::current_dir()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                message: String::new(),
            }),
    };
    let Some(update) = update else {
        return Ok(());
    };
    match pane_and_socket() {
        Some((pane_id, socket)) => {
            let _ = send(pane_id, &socket, kind, update);
        }
        None => {
            let _ = report_outside_a_pane(kind, update);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(raw: &str, status_arg: Option<&str>) -> Option<AgentState> {
        derive_claude(raw, status_arg).map(|update| update.state)
    }

    #[test]
    fn events_map_to_states_when_no_status_is_given() {
        assert_eq!(
            state(r#"{"hook_event_name":"UserPromptSubmit"}"#, None),
            Some(AgentState::Working)
        );
        assert_eq!(
            state(r#"{"hook_event_name":"PostToolUse"}"#, None),
            Some(AgentState::Working)
        );
        assert_eq!(
            state(
                r#"{"hook_event_name":"Notification","message":"approve?"}"#,
                None
            ),
            Some(AgentState::Pending)
        );
        assert_eq!(
            state(r#"{"hook_event_name":"Stop","message":"shipped it"}"#, None),
            Some(AgentState::Done)
        );
        assert_eq!(
            state(r#"{"hook_event_name":"SessionEnd"}"#, None),
            Some(AgentState::Idle)
        );
        assert_eq!(state(r#"{"hook_event_name":"Whatever"}"#, None), None);
    }

    #[test]
    fn an_explicit_status_wins_over_the_event() {
        assert_eq!(
            state(r#"{"hook_event_name":"Stop"}"#, Some("running")),
            Some(AgentState::Working)
        );
        // A typo'd registration is refused rather than folded to idle, which
        // would silently blank the row it meant to update.
        assert_eq!(state(r#"{"hook_event_name":"Stop"}"#, Some("dnoe")), None);
    }

    #[test]
    fn a_turn_that_ends_by_asking_reads_as_blocked_not_finished() {
        // The single most misleading state the overlay can show: a green
        // "done" on a turn that is actually waiting for an answer.
        assert_eq!(
            state(
                r#"{"hook_event_name":"Stop","last_assistant_message":"Refactored auth.\n\nShould I update the tests?"}"#,
                Some("done"),
            ),
            Some(AgentState::Pending)
        );
        // A question mark that is not the last thing said is just prose.
        assert_eq!(
            state(
                r#"{"hook_event_name":"Stop","last_assistant_message":"Want anything else?\nAll tests pass."}"#,
                Some("done"),
            ),
            Some(AgentState::Done)
        );
        // Trailing blank lines do not hide the question.
        assert_eq!(
            state(
                r#"{"hook_event_name":"Stop","last_assistant_message":"Ready to push?\n\n  \n"}"#,
                Some("done"),
            ),
            Some(AgentState::Pending)
        );
    }

    #[test]
    fn placeholder_notifications_never_claim_to_need_you() {
        for message in [
            "",
            "   ",
            "Claude needs attention",
            "Claude Code needs your attention",
        ] {
            let raw = serde_json::json!({
                "hook_event_name": "Notification",
                "message": message,
            })
            .to_string();
            assert_eq!(state(&raw, None), None, "{message:?} must not raise a flag");
        }
        // A real question still does.
        let raw = r#"{"hook_event_name":"Notification","message":"Allow git push to origin?"}"#;
        assert_eq!(state(raw, None), Some(AgentState::Pending));
    }

    #[test]
    fn a_malformed_payload_is_a_no_op_not_an_error() {
        // The hook runner pipes whatever it has; garbage must never reach the
        // agent as a failure.
        assert_eq!(state("not json at all", None), None);
        assert_eq!(state("", None), None);
        assert_eq!(state("{}", None), None);
        // …but an explicit status still works without any payload at all,
        // which is what makes this usable from a plain shell script.
        assert_eq!(state("", Some("working")), Some(AgentState::Working));
    }

    #[test]
    fn cwd_comes_from_the_payload_and_falls_back_to_the_process() {
        let update = derive_claude(
            r#"{"hook_event_name":"Stop","message":"ok","cwd":"/home/u/repo"}"#,
            None,
        )
        .expect("update");
        assert_eq!(update.cwd, "/home/u/repo");

        let update =
            derive_claude(r#"{"hook_event_name":"Stop","message":"ok"}"#, None).expect("update");
        assert_eq!(
            update.cwd,
            std::env::current_dir()
                .expect("current dir")
                .to_string_lossy()
                .into_owned()
        );
    }

    fn update(state: AgentState, message: &str) -> HookUpdate {
        HookUpdate {
            state,
            cwd: String::from("/repo"),
            message: message.to_owned(),
        }
    }

    /// What an agent outside a pane leaves on disk, over a turn's worth of
    /// hooks. Every one of these carries is the difference between an elapsed
    /// clock that counts a turn and one that counts the gap between two tool
    /// calls.
    #[test]
    fn a_record_carries_the_turn_forward_across_a_turns_worth_of_hooks() {
        let first = merged_record(
            None,
            AgentKind::Claude,
            update(AgentState::Working, "reading the tests"),
            77,
            1_000,
        );
        assert_eq!(first.working_since_unix_ms, 1_000);
        assert_eq!(first.start_time, 77);

        // A `PreToolUse` five seconds later has nothing to say and does not
        // restart the clock.
        let second = merged_record(
            Some(first),
            AgentKind::Claude,
            update(AgentState::Working, ""),
            77,
            6_000,
        );
        assert_eq!(second.working_since_unix_ms, 1_000);
        assert_eq!(second.message, "reading the tests");

        // The turn ends: a clock of its own — how long ago it finished — and
        // the agent's closing words.
        let third = merged_record(
            Some(second),
            AgentKind::Claude,
            update(AgentState::Done, "all green"),
            77,
            9_000,
        );
        assert_eq!(third.working_since_unix_ms, 9_000);
        assert_eq!(third.message, "all green");
        assert_eq!(third.state, "done");

        // And the next turn starts its own clock rather than resuming the last.
        let fourth = merged_record(
            Some(third),
            AgentKind::Claude,
            update(AgentState::Working, ""),
            77,
            20_000,
        );
        assert_eq!(fourth.working_since_unix_ms, 20_000);
    }

    /// Pids are reused, and a record left by a different agent is not this
    /// agent's history.
    #[test]
    fn a_record_from_another_agent_is_not_carried_forward() {
        let theirs = merged_record(
            None,
            AgentKind::OpenCode,
            update(AgentState::Working, "their turn"),
            5,
            1_000,
        );

        let ours = merged_record(
            Some(theirs),
            AgentKind::Claude,
            update(AgentState::Working, ""),
            5,
            8_000,
        );

        assert_eq!(ours.working_since_unix_ms, 8_000);
        assert_eq!(ours.message, "");
        assert_eq!(ours.kind, "claude");
    }

    fn opencode_state(raw: &str, status_arg: Option<&str>) -> Option<AgentState> {
        derive_opencode(raw, status_arg).map(|update| update.state)
    }

    #[test]
    fn opencode_events_map_to_states_when_no_status_is_given() {
        assert_eq!(
            opencode_state(r#"{"event":"tool.execute.before"}"#, None),
            Some(AgentState::Working)
        );
        assert_eq!(
            opencode_state(r#"{"event":"session.idle"}"#, None),
            Some(AgentState::Done)
        );
        assert_eq!(
            opencode_state(r#"{"event":"session.error"}"#, None),
            Some(AgentState::Error)
        );
        assert_eq!(
            opencode_state(
                r#"{"event":"permission.updated","message":"needs permission: cargo test"}"#,
                None
            ),
            Some(AgentState::Pending)
        );
        assert_eq!(opencode_state(r#"{"event":"tui.toast.show"}"#, None), None);
    }

    /// The same rule Claude's placeholder notification gets, for the same
    /// reason: opencode raises a permission event for every ask, and a row that
    /// says an agent wants *something* is a row people stop reading.
    #[test]
    fn an_opencode_permission_it_cannot_name_raises_nothing() {
        assert_eq!(
            opencode_state(r#"{"event":"permission.updated","message":""}"#, None),
            None
        );
        assert_eq!(
            opencode_state(r#"{"event":"permission.updated"}"#, None),
            None
        );
        assert_eq!(opencode_state("{}", Some("pending")), None);
    }

    #[test]
    fn an_opencode_payload_carries_its_own_cwd_and_message() {
        let update = derive_opencode(
            r#"{"event":"permission.updated","message":"needs permission: rm -rf build","cwd":"/root/work"}"#,
            None,
        )
        .expect("update");
        assert_eq!(update.cwd, "/root/work");
        assert_eq!(update.message, "needs permission: rm -rf build");

        // The plugin passes `--status` on every call, so that path is the one
        // that actually runs in production.
        assert_eq!(
            opencode_state(
                r#"{"event":"session.idle","cwd":"/root/work"}"#,
                Some("done")
            ),
            Some(AgentState::Done)
        );
        assert_eq!(
            opencode_state(r#"{"event":"session.idle"}"#, Some("dnoe")),
            None
        );
    }
}
