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

/// Run the producer for one hook invocation.
///
/// Always `Ok`. Every failure path is a silent no-op by design — see the module
/// documentation.
pub fn run(kind: AgentKind, status_arg: Option<&str>) -> Result<(), Box<dyn Error>> {
    let Some((pane_id, socket)) = pane_and_socket() else {
        return Ok(());
    };
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
    if let Some(update) = update {
        let _ = send(pane_id, &socket, kind, update);
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
