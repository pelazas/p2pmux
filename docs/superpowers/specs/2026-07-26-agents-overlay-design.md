# Agents overlay: roster of live coding agents

## Goal

Give every member in a shared session a toggleable overlay that lists **coding agents only** (not every shell/process), with enough context to jump into the right pane.

Fields per row:

- agent kind (fixed display label from allowlist)
- working directory / repo folder (best-effort; may be empty)
- pane host display name (from layout roster)
- controller (`free` or display name) — same semantics as pane chrome
- coarse state: `idle` | `working` | `done`

Interaction:

- `Ctrl+A` **toggles** the Agents overlay open/closed
- **Pass-through:** if overlay is closed and the user presses `Ctrl+A` twice within 400ms, forward a single Ctrl+A to the focused PTY and do **not** open the overlay (readline beginning-of-line escape hatch). A single `Ctrl+A` after the window elapses opens the overlay.
- While open: modal and **non-expiring** (not subject to the 2s sticky-chord idle timeout). ↑/↓ or `j`/`k` move selection; Enter focuses that pane’s tab+leaf and closes the overlay; Esc closes without jumping.
- While open: **no keys are forwarded to any PTY** except that Esc/Enter/arrows/jk are handled by the overlay.
- Overlay is a floating panel (`Clear` + bordered list over the current layout), not a real mux tab
- **Mouse: cut from v1** (keyboard only)

## Non-goals

- Agent orchestration, ACLs, or sandboxing
- Listing arbitrary processes / shells / editors
- Perfect per-vendor turn state via proprietary APIs
- Always-on sidebar or dashboard home screen
- Changing lease / control semantics
- Windows/Linux (macOS only)
- Mouse hit-testing for overlay rows (v1)
- Automatic `git push` / PR creation as part of coding tasks (handoff step only)

## Product framing

Agents remain **processes inside panes**. The overlay is a **lens** over panes whose process tree matches a known agent allowlist. Aligns with “shared session fabric, not a dashboard of agents.”

cwd paths are shared with all session members (same trust model as the shared shell). Mention briefly in README.

## Detection (host-local)

Each pane is a login shell PTY. Agents run as descendants. On the **pane host only**:

1. Resolve the PTY session child PID from `portable_pty::Child::process_id()` via `PtyHost`.
2. Sample processes on a timer with a **replaceable sampler adapter**.
3. **v1 sampler choice:** `sysinfo` (safe public API from this crate’s perspective). Collect **once globally per interval** (~1s), then classify each hosted pane’s tree from that snapshot. Never call the sampler on the 16ms TUI render path — run sampling on a dedicated interval tick / blocking task and apply results asynchronously.
4. Match descendants against the **exact v1 allowlist** below.
5. If multiple allowlisted agents appear under one pane, pick deterministically:
   1. greatest descendant depth from the PTY session root
   2. then newest process start time (if sampler provides it; else skip this key)
   3. then highest PID
6. Working directory: cwd of the matched agent process when `sysinfo` provides it; else empty string.
7. If no allowlisted agent is present → not listed (unless in `done` grace — see state model).

### v1 allowlist (exact)

Matching uses the process **basename** only (last path component of exe/argv0), compared **case-sensitively** as returned by the sampler.

| Basename equals | `agent_kind` wire value | Display label |
|-----------------|-------------------------|---------------|
| `claude` | `claude` | Claude Code |
| `codex` | `codex` | Codex |
| `cursor-agent` | `cursor` | Cursor Agent |
| `pi` | `pi` | Pi |

No other names match. Bare `cursor` (the editor) does **not** match. Adding a tool = one table row + tests.

Required snapshot fields per process: `pid`, `parent_pid`, `basename` (from exe or argv0), optional `start_time`, optional `cwd`.

## State model

Host tracks per pane:

- `last_output_at: Option<Instant>` — updated in `SharedLocalPane::drain()` when PTY bytes arrive (this timestamp does not exist today; add it)
- `active_agent: Option<DetectedAgent>` — current match
- `done_agent: Option<(kind, cwd, entered_done_at)>` — set when a previously active agent disappears

| State | Meaning |
|-------|---------|
| `working` | Active allowlisted agent **and** `last_output_at` within the last 2s |
| `idle` | Active allowlisted agent **and** quiet |
| `done` | No active agent, but `done_agent` within 15s grace |
| (absent) | No active agent and grace expired / never had an agent |

If a **different** agent appears before grace expiry: clear `done_agent`, use the new active agent (`working`/`idle`) immediately — do not keep the old done row.

## Multiplayer / wire topology

Members send control envelopes **to the coordinator**. Members accept layout/roster **state only from the coordinator**. Hosts never peer-publish rosters sideways.

### Messages (PROTOCOL_VERSION = 4)

- `AgentRoster` (new `envelope::Body` tag **26**):
  - `host_peer_id: bytes` — must equal authenticated sender; coordinator overwrites/ignores mismatches
  - `generation: u64` — per-host monotonic; coordinator ignores generations ≤ last accepted for that host
  - `entries: repeated AgentRosterEntry`
- `AgentRosterEntry`:
  - `pane_id: u64` — same layout pane id type as `PaneDescriptor.pane_id`
  - `agent_kind: string` — one of `claude|codex|cursor|pi`
  - `cwd: string` — may be empty; max length enforced
  - `state: enum` — `idle|working|done`

Caps (normative): max **32** entries per update; `agent_kind` ≤ 32 bytes; `cwd` ≤ 512 bytes; reject unknown state/kind; reject **duplicate** `pane_id` in one update.

### Coordinator duties

1. Validate sender is an admitted member.
2. Reject any entry whose `pane_id` is not currently hosted by that sender in the authoritative layout.
3. Accept only if `generation` > last stored for that host (else ignore).
4. Store **full replacement** for that host (including empty `entries` to clear).
5. Prune stored entries when: pane deleted, pane host changes, member departs, or layout reconcile removes the pane.
6. Relay accepted rosters to all members on an **independent coalesced roster watch channel** — **not** `publish_state` (that slot coalesces layout commits and must not be shared).
7. Bootstrap: after the existing initial `SessionSnapshot`, deliver a coordinator-built **full roster snapshot** (concatenate cached per-host rosters, or one synthetic `AgentRoster` per host in order). Late joiners must see current agents without waiting for heartbeats. Heartbeat (~5s) remains a liveness/repair aid, not bootstrap.

### Client merge

- Key entries by `pane_id` globally after coordinator validation (one host owns a pane).
- Replace one host’s contribution as a unit when its `AgentRoster` arrives.
- Drop host contribution on member leave.
- Overlay joins entries with layout (host name) + lease (control) + display-name chrome.
- Sanitize peer strings to single-line for render (strip controls/newlines).

### Regression tests required

- Roster updates must not drop undelivered `LayoutCommit`s (separate mailbox path).
- Member-hosted agent appears for a second member via coordinator relay.
- Late joiner receives roster after snapshot.
- Forged entry for another host’s pane rejected.
- Empty roster clears that host’s rows.

## UI behavior under live updates

- Deterministic row order: sort by tab order, then pane ordinal within tab, then `pane_id`.
- Selection keyed by `pane_id` (not list index); if selected pane disappears, clamp to nearest remaining or clear selection.
- Jump to agent on another tab: switch tab + focus pane (covered by test).
- Empty state: panel shows `No agents running`.
- Narrow terminals: truncate cwd with leading ellipsis; keep kind + state visible.

## Keyboard conflicts

`Ctrl+A` is mux-owned with double-tap pass-through as specified. Document in README.

## Testing

- Pure unit tests for allowlist matching (basename fixtures).
- Pure unit tests for state transitions including done grace and agent replacement.
- Pure unit tests for deterministic multi-agent pick rule.
- TUI: toggle open/close; double-tap forwards Ctrl+A; Esc closes; Enter jumps including other tab; overlay modal (no PTY forward); empty state.
- Protocol encode/decode + caps + version **4** updates to all version/tag tests.
- Session: coordinator relay, bootstrap, forge reject, layout/roster mailbox isolation.

## Success criteria

1. Only allowlisted agent panes appear (plus short `done` grace rows).
2. Rows show kind, cwd (when known), host, control, state.
3. `Ctrl+A` toggle + double-tap pass-through + Esc/Enter behave as specified.
4. Remote members see agents via coordinator-relayed roster; late joiners bootstrap correctly.
5. Roster traffic never coalesces away layout commits.
6. `cargo test` passes; this crate remains `forbid(unsafe_code)`.
