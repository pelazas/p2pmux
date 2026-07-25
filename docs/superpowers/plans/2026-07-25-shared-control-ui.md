# Shared Control UI Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give hosts and guests consistent shared-control help, synchronized active/idle controller chrome, and a Ctrl+Q-only command model.

**Architecture:** The host remains the lease authority. Every accepted input republishes the current lease so a guest can use receipt time as an activity clock; both render loops redraw when that clock crosses the eight-second idle boundary. A joining guest waits for its first host-published lease instead of inventing one locally. A small pure TUI view helper owns chrome, footer, and bordered-content allocation. The `force` field is removed from take-control messages, leaving only idle-only lease claims.

**Tech Stack:** Rust 2024, ratatui 0.30, crossterm 0.29, prost, Cargo integration tests.

---

## Chunk 1: Safe activity synchronization

### Task 1: Remove forced takeover from the wire command

**Files:**
- Modify: `src/protocol.rs: TakeControl message definition and validation`
- Modify: `src/session.rs: GuestControlSender and control writer`
- Modify: `src/tui.rs: host and guest input paths`
- Modify: `tests/protocol.rs`, `tests/lease.rs`, `tests/session_stream.rs`

- [ ] **Step 1: Write failing protocol and lease tests**

Change protocol fixtures to construct `TakeControl` without `force`. Remove the synthetic `InitialLease` queued by `join_pane`; update the session-stream test to wait for the host-published initial lease and assert its current controller and epoch are used. Add/retain a lease test proving a normal claim during active input returns `RejectActiveController`, and add a session-level control-path test that an active controller cannot be displaced through a received take-control request.

- [ ] **Step 2: Run tests to verify the missing behavior**

Run: `cargo test protocol:: --test protocol && cargo test --test lease && cargo test --test session_stream active`

Expected: FAIL because `force` remains part of the message and host handling can call `force_take_control`.

- [ ] **Step 3: Implement the minimal safe command surface**

Remove `force` from `TakeControl` and `GuestControlSender::try_take_control`. Delete `LeaseManager::force_take_control` and its test. Remove the synthetic guest initial lease so guest input stays disabled until the first host-published lease. Route all host take-control events through `LeaseManager::take_control`; remove F9 takeover branches from both TUI loops.

- [ ] **Step 4: Run tests to verify it passes**

Run: `cargo test protocol:: --test protocol && cargo test --test lease && cargo test --test session_stream active`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/protocol.rs src/session.rs src/lease.rs src/tui.rs tests/protocol.rs tests/lease.rs tests/session_stream.rs
git commit -m "fix: prevent forced pane takeover"
```

### Task 2: Republish accepted-input activity

**Files:**
- Modify: `src/tui.rs: host local and remote input decisions`
- Test: `src/tui.rs` or `tests/session_stream.rs`

- [ ] **Step 1: Write a failing activity-publication test**

Add a focused test that accepts input through the host control path and observes a new lease watch update with the same controller and epoch. Add a first-character claim test: an idle claimant receives ownership and its buffered first byte is subsequently forwarded exactly once. Add a two-claimant test proving the serialized winner's byte is forwarded and the losing buffered byte is cleared.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test activity_publication && cargo test first_character_claim && cargo test concurrent_idle_claims`

Expected: FAIL because accepted input does not update `lease_tx` and claim behavior is not isolated for testing.

- [ ] **Step 3: Implement minimal activity publication and test seam**

After every accepted host-local or remote input, `send_replace` the current `LeaseState` to the existing watch channel. Extract only the necessary pure guest claim/buffer transition helper so its first-byte and loser-clear behavior can be tested without terminal I/O; preserve the current host arbitration and queue behavior.

- [ ] **Step 4: Run focused tests to verify they pass**

Run: `cargo test activity_publication && cargo test first_character_claim && cargo test concurrent_idle_claims`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui.rs tests/session_stream.rs
git commit -m "feat: broadcast pane typing activity"
```

## Chunk 2: Shared help and controller chrome

### Task 3: Model and render controller chrome

**Files:**
- Modify: `src/tui.rs: view helpers, renderers, draw loops, unit tests`

- [ ] **Step 1: Write failing renderer tests**

Add tests for a pure control-view helper and a `TestBackend` render: active uses `Color::Rgb(255, 69, 0)` and `this user is typing`; idle uses `Color::Rgb(140, 91, 68)` and `this user has control`; an actual guest with no host lease yet renders no border. Assert bordered content starts one cell right/down, is cropped to the inner rect, and the cursor is shifted and clipped to that inner rect.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test tui::tests::renders_control_chrome`

Expected: FAIL because there is no control-view or bordered renderer.

- [ ] **Step 3: Implement the minimal shared renderer**

Introduce a `ControlChrome` helper driven by host `LeaseState` or guest receipt time. Reserve the last terminal row for footer; enclose the remaining area in a ratatui `Block` and render the fixed vt100 grid only in `Block::inner`, cropping its bottom/right as necessary. Shift the cursor into that inner rect. Re-evaluate the helper every poll cycle and set `dirty` when active/idle changes so the border turns muted after eight seconds without a new screen frame.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test tui::tests::renders_control_chrome`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui.rs
git commit -m "feat: show active pane control chrome"
```

### Task 4: Share footer help and reserve Ctrl+Q

**Files:**
- Modify: `src/tui.rs: key helpers, host and guest footers, key tests`
- Modify: `README.md: local and shared control instructions`

- [ ] **Step 1: Write failing footer and key tests**

Add tests for exact `CONTROL_HELP`: `type to claim idle | active typing is protected | Ctrl+Q quit`. Assert host, normal guest, and pre-lease guest compose it unchanged (with permitted join/waiting prefixes). Change key tests to reserve Ctrl+Q, while F9/F10 encode normally, and assert bare Q/F10 do not quit.

- [ ] **Step 2: Run focused tests to verify they fail**

Run: `cargo test tui::tests::shared_control_help && cargo test tui::tests::encodes_supported_keys`

Expected: FAIL because role-specific F9/F10 copy and key reservations remain.

- [ ] **Step 3: Implement the minimal copy and command changes**

Create footer composition helpers, use the shared help in both loops, make `is_quit` recognize Ctrl+Q only, and exclude Ctrl+Q in `encode_key`. Update README to explain idle automatic handoff, active-input protection, Ctrl+Q, and the absence of a forced takeover key.

- [ ] **Step 4: Run focused and full verification**

Run: `cargo test tui::tests::shared_control_help && cargo test tui::tests::encodes_supported_keys && cargo test`

Expected: PASS with all tests green.

- [ ] **Step 5: Commit**

```bash
git add src/tui.rs README.md
git commit -m "feat: share ctrl-q control help"
```

## Chunk 3: Live acceptance

### Task 5: Perform the two-terminal acceptance check

**Files:**
- Modify: none

- [ ] **Step 1: Build the current branch**

Run: `cargo build`

Expected: successful debug build.

- [ ] **Step 2: Run host and guest in separate terminals**

Run host: `cargo run -- create`; copy the displayed ticket/code; then run guest: `cargo run -- join <ticket>`.

- [ ] **Step 3: Verify shared-control behavior**

Confirm that both footer variants contain the identical shared help; active typing shows the vivid `this user is typing` border; it becomes muted `this user has control` after eight seconds; an idle guest claim delivers its first character; a concurrent/active claim cannot interrupt or leak buffered input; and Ctrl+Q exits both views.

- [ ] **Step 4: Record fresh automated verification**

Run: `cargo test`

Expected: PASS.
