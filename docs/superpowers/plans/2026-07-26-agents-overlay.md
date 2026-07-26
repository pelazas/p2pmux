# Agents overlay Implementation Plan

> **For agentic workers:** Execute only in `/Users/pelazas/Desktop/p2pmux-agents-overlay` on branch `feat/agents-overlay`. One commit per completed task. Do **not** `git push` or open a PR unless the human/handoff explicitly asks — implementation ends when the suite is green and commits are local.

**Goal:** Ship a `Ctrl+A` agents-only overlay (kind, cwd, host, control, idle/working/done) backed by host-local allowlist detection and coordinator-relayed `AgentRoster` (protocol v5).

**Architecture:** Pure `agent_detect` + `sysinfo` sampler adapter (global interval sample). Hosts send `AgentRoster` to the coordinator; coordinator validates/caches/relays on a **new coalesced roster mailbox** separate from layout `publish_state`. TUI owns modal overlay toggle, double-tap Ctrl+A pass-through, selection-by-pane-id, and jump. Layout/lease semantics unchanged.

**Spec:** `docs/superpowers/specs/2026-07-26-agents-overlay-design.md` (authoritative; follow it when this plan and the code would otherwise drift).

**Current contract (post-plan polish):** protocol v5 adds `AgentRosterEntry.working_since_unix_ms` (tag 5). The shipped overlay uses multi-line cards with working duration/spinner and accepts mouse click and wheel scrolling while open. Task text below records the original implementation sequence; its v4/no-mouse scope is superseded by this current contract and the spec.

---

## Task 1: Spec + plan docs

**Files:**
- `docs/superpowers/specs/2026-07-26-agents-overlay-design.md`
- `docs/superpowers/plans/2026-07-26-agents-overlay.md` (this file)

- [ ] Commit docs only

```bash
git add docs/superpowers/specs/2026-07-26-agents-overlay-design.md docs/superpowers/plans/2026-07-26-agents-overlay.md
git commit -m "$(cat <<'EOF'
docs: specify agents overlay roster and plan

EOF
)"
```

---

## Task 2: Agent detection module (pure)

**Files:**
- Create `src/agent_detect.rs`
- Update `src/lib.rs`
- Unit tests in `agent_detect` (fixtures, no live agents)

Implement per spec:
- Exact basename allowlist table + display labels
- `classify_pane_tree(...)` with deterministic pick: depth → start_time → pid
- State helper with `last_output_at`, active agent, done grace (15s), and replacement-before-grace behavior
- Injected process snapshot type (`pid`, `parent_pid`, `basename`, optional `start_time`, optional `cwd`)

- [ ] `cargo test agent_detect --lib`
- [ ] Commit

```bash
git commit -m "$(cat <<'EOF'
feat: add allowlist agent detection helpers

EOF
)"
```

---

## Task 3: PtyHost PID + drain timestamp + sysinfo sampler

**Files:**
- `src/pty_host.rs` — expose `process_id()`
- `src/agent_detect.rs` — `SysinfoSampler` adapter + global snapshot helper
- `src/tui.rs` — `SharedLocalPane::drain` sets `last_output_at`; interval sampling off render path
- `Cargo.toml` — add `sysinfo` dependency

Rules:
- Sample once globally per ~1s interval, not per pane on the render loop
- Soft-fail → no agent
- Wire local detection into hosted panes only

- [ ] Tests with fake snapshots still primary; sampler adapter testable behind trait
- [ ] Commit

```bash
git commit -m "$(cat <<'EOF'
feat: sample hosted PTY process trees for agents

EOF
)"
```

---

## Task 4: Wire `AgentRoster` + PROTOCOL_VERSION 4

**Files:**
- `src/protocol.rs` — messages, tag 26, validation caps, version bump **3 → 4**
- `tests/protocol.rs` — update version asserts + new encode/validate cases
- `tests/module_surface.rs` — `PROTOCOL_VERSION == 4`

Validate: entry cap 32, kind/cwd lengths, known kinds/states, duplicate pane ids, non-zero pane id.

- [ ] `cargo test protocol module_surface`
- [ ] Commit

```bash
git commit -m "$(cat <<'EOF'
feat: add AgentRoster wire message (protocol v4)

EOF
)"
```

---

## Task 5: Coordinator relay, cache, bootstrap, mailbox isolation

**Files:**
- `src/session.rs` — ControlMailbox roster channel; coordinator accept/validate/cache/prune/relay; member apply
- `tests/session_*.rs` and/or session unit tests

Must implement:
- Independent coalesced roster watch (must not share `publish_state` with `LayoutCommit`)
- Per-host generation; full replace including empty clear
- Reject entries for panes not hosted by sender
- Prune on pane delete / host change / member leave
- Bootstrap: after `SessionSnapshot`, deliver cached roster(s)
- Regression: rapid layout commits + roster publishes never lose layout

- [ ] Coordinator-relay integration tests listed in the spec
- [ ] Commit

```bash
git commit -m "$(cat <<'EOF'
feat: relay agent rosters through the coordinator

EOF
)"
```

---

## Task 6: Overlay TUI

**Files:**
- `src/tui.rs` — modal overlay state, toggle, double-tap pass-through, render, jump, footer
- TUI unit tests

Must implement:
- `Ctrl+A` toggle; double-tap ≤200ms → encode/forward Ctrl+A to focused PTY
- Modal: while open, no arbitrary PTY forwarding; not chord-idle-expiring
- Selection by `pane_id`; stable sort; empty state `No agents running`
- Enter jumps (including other tab) and closes; Esc closes
- Join roster + layout host labels + lease control text; sanitize strings
- Footer for overlay mode
- **No mouse** for overlay in v1

- [ ] TUI tests covering toggle, pass-through, jump cross-tab, empty state
- [ ] Commit

```bash
git commit -m "$(cat <<'EOF'
feat: add Ctrl+A agents overlay UI

EOF
)"
```

---

## Task 7: README + full suite

**Files:**
- `README.md` — Ctrl+A toggle, double-tap pass-through, agent-only allowlist, cwd sharing note

- [ ] `cargo test` full suite green
- [ ] Commit if README/polish needed
- [ ] Stop. Do not push/PR unless explicitly asked.

```bash
git commit -m "$(cat <<'EOF'
docs: document agents overlay chord and detection

EOF
)"
```

---

## Out of scope / do not

- Force-detect unknown AI tools / bare `cursor`
- Sidebar dashboard / mouse overlay
- Vendor status APIs
- Changing control lease rules
- Push or PR from coding tasks
- Implementing outside this worktree/branch
