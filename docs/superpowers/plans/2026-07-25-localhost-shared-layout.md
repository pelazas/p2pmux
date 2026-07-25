# Spike 3 Localhost Shared Layout Implementation Plan

> **For agentic workers:** REQUIRED: Use `superpowers:subagent-driven-development`. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let localhost session members create tabs and panes whose PTYs run on the creator's Mac, while permitting deletes only by the pane host (or the host of every pane in a tab).

**Architecture:** A coordinator owns a full, revisioned `SessionState` (membership, pane descriptors, tabs, and split trees) and publishes whole-state commits over one persistent member-control stream. Each pane still uses independent direct host-to-viewer Snapshot/Delta and control/lease streams. A create operation is prepared before its PTY starts, then committed only after its creator reports the pane ready.

**Tech Stack:** Rust 2024, Tokio, Iroh 1.0, Prost, ratatui/crossterm, portable-pty, vt100.

---

## Locked behavior and invariants

- At most 8 members, 9 tabs, 8 panes/tab, and split depth 4.
- The layout always retains at least one tab, and every tab retains at least one pane. A last leaf cannot be deleted; its owner can delete its tab when another tab exists.
- New panes split the selected leaf 50/50. The requesting TUI chooses `LeftRight` or `TopBottom` from its local rectangle and sends that axis.
- PTY grids are fixed at pane creation. Every viewer letterboxes or clips a fixed grid; no resize protocol is added.
- `Ctrl+P`, then `N`/`X` creates/deletes the selected pane. `Ctrl+T`, then `N`/`X` creates/deletes the selected tab. `Ctrl+P`, arrows move pane selection; `Ctrl+T`, left/right switch tabs; `Esc` cancels a chord. Ctrl+Q quits; F9 and F10 pass through to the focused PTY.
- A request rejected for a stale revision refreshes the UI state but is never automatically replayed.
- This spike's host-owned deletion is combined with the shared-control policy: Ctrl+Q exits, while F9/F10 remain available to nested applications. The documentation task must make `docs/MVP_DESIGN.md`, `docs/SPIKE_PLAN.md`, and the shared-control design agree.

## Files and boundaries

| File | Responsibility |
| --- | --- |
| `src/layout.rs` | Pure state model, limits, validation, and structural reducer. |
| `src/protocol.rs` | v2 envelopes, serialized bounded member addresses, layout messages, and validation. |
| `src/transport.rs` | v2 ALPN and existing framed-stream primitives. |
| `src/session.rs` | Coordinator/member control stream, pane registry, direct pane subscriptions, and reconciliation events. |
| `src/tui.rs` | Multi-pane local/remote view state, rectangles, focus, chords, and host PTY lifecycle. |
| `src/cli.rs` | Starts the coordinator/member runtime and routes terminal events to it. |
| `tests/layout.rs` | Deterministic reducer coverage. |
| `tests/protocol.rs`, `tests/session_stream.rs`, `tests/session_layout.rs` | Wire, direct-pane, and localhost session integration coverage. |

## Chunk 1: Pure state and protocol boundary

### Task 1: Commit this plan

**Files:** Create `docs/superpowers/plans/2026-07-25-localhost-shared-layout.md`

- [ ] Commit this plan without source changes.

### Task 2: Add the pure layout reducer

**Files:** Create `src/layout.rs`, `tests/layout.rs`; modify `src/lib.rs`.

- [ ] Write failing tests for initial state, right/down split, depth/tab/pane limits, sibling expansion, host-only pane delete, all-host tab delete, stale revisions, and last leaf/tab rejection.
- [ ] Run `cargo test --test layout` and observe the missing-module failure.
- [ ] Implement only the tested reducer with coordinator-generated numeric IDs and full-state commits.
- [ ] Run `cargo fmt --all && cargo test --test layout`.
- [ ] Commit `feat: add pure shared layout reducer`.

### Task 3: Introduce protocol v2

**Files:** Modify `src/protocol.rs`, `src/transport.rs`, `tests/protocol.rs`, `tests/transport.rs`.

- [ ] Write failing frame round-trip and validation tests for `SessionSnapshot`, `LayoutRequest`, `PaneReservation`, `PaneReady`, `LayoutCommit`, `LayoutReject`, and `PaneSubscribe`.
- [ ] Verify the tests fail because v1 has none of these bodies.
- [ ] Bump protocol version and ALPN to v2. Keep screens out of state commits; member addresses are bounded serialized `EndpointAddr` data and pane descriptors refer to a member ID.
- [ ] Define explicit action payloads: `LayoutRequest { request_id, base_revision, action }`, `CreatePane { target_pane_id, axis, grid_rows, grid_cols }`, `DeletePane { pane_id }`, `CreateTab { grid_rows, grid_cols }`, and `DeleteTab { tab_id }`. `PaneReservation` and `PaneReady` both carry the same coordinator-generated reservation ID. The coordinator rejects readiness for an expired, mismatched, or already-consumed reservation. It derives the owner from the authenticated control connection; it never trusts a host ID supplied by a request.
- [ ] Preserve size, identity, and malformed-frame checks for every new message.
- [ ] Run protocol and transport tests, then commit `feat: add shared-layout protocol v2`.

## Chunk 2: Coordinator and per-pane transport

### Task 4: Add the coordinator session state machine

**Files:** Modify `src/session.rs`; create `tests/session_layout.rs`.

- [ ] Write failing loopback tests for capped admission, initial snapshot, full commit broadcast, stale rejection, and foreign-delete rejection.
- [ ] Verify the new tests fail against the single fixed-pane API.
- [ ] Replace the one-shot post-Welcome setup with a persistent member-control stream. Authenticate the connection peer, record its endpoint address, and broadcast full commits after admission or structural changes.
- [ ] Make the coordinator hold one pending create reservation. It validates a request, sends an ID reservation, accepts `PaneReady` only from the authenticated reservation creator, then atomically commits; timeout/failure leaves state unchanged. `CreateTab` uses the same reservation path and reserves both the tab and its required initial pane.
- [ ] Run `cargo test --test session_layout` and the existing handshake tests; commit `feat: coordinate revisioned shared layouts`.

### Task 5: Generalize direct pane streams

**Files:** Modify `src/session.rs`, `tests/session_stream.rs`.

- [ ] Write failing loopback tests showing two arbitrary pane IDs can independently deliver an actual initial `ControlLease`, Snapshot, and Delta; reject a subscription from a nonmember or for a mismatched host/pane.
- [ ] Verify they fail because `DEFAULT_PANE_ID` is hard-coded.
- [ ] Add a registry of local pane channels and a `PaneSubscribe` handshake. Every process accepts pane connections; viewers reconcile commit descriptors to local panes or remote subscriptions.
- [ ] A late joiner may see the snapshot before a pane host has processed the membership commit. Treat a host's `NotMember` subscription rejection as transient: retry after the next commit and with a bounded localhost backoff until the admission deadline. Cover that race in the loopback test.
- [ ] Keep a dedicated direct connection/stream bundle per viewed remote pane in this initial localhost implementation. Do not allow one pane's writer to block another's screen or input path.
- [ ] Remove the guest's synthetic initial lease and keep only host-emitted lease state.
- [ ] Run session stream/layout tests; commit `feat: stream panes from their owning members`.

## Chunk 3: Runtime and TUI

### Task 6: Connect session runtime to local PTY lifecycles

**Files:** Modify `src/cli.rs`, `src/session.rs`, `src/tui.rs`; extend `tests/session_layout.rs`.

- [ ] Write failing tests for the create reservation sequence: a pane appears in a commit only after registration and a failed registration produces no layout mutation.
- [ ] Verify the test fails.
- [ ] Add bounded TUI-to-session and session-to-TUI commands. The TUI registers channels for each local PTY before reporting `PaneReady`; deletion unregisters only after its commit and then shuts down that PTY.
- [ ] Start the coordinator's default pane through the same registry path; joining starts with no local pane and subscribes after its snapshot.
- [ ] Run the affected tests; commit `feat: manage local pane lifecycles through session runtime`.

### Task 7: Render and operate the multi-pane TUI

**Files:** Modify `src/tui.rs`; extend its unit tests.

- [ ] Write failing ratatui `TestBackend` tests for tab bar/recursive rectangles, focus movement, fixed-grid clipping, chord consumption, and request routing for each create/delete command.
- [ ] Verify they fail against the single-pane renderer.
- [ ] Replace separate one-pane host/guest loops with one multi-pane loop. Render host badges and lease state; render layout immediately but disable pane input until its Snapshot and ControlLease arrive.
- [ ] Route normal typing and paste to the focused pane. Keep mux chords out of every PTY; F9/F10 remain PTY input.
- [ ] Run TUI and full test suites; commit `feat: add tabs splits and pane chords to tui`.

## Chunk 4: Completion checks

### Task 8: Documentation and automated verification

**Files:** Modify `README.md`, `docs/MVP_DESIGN.md`, `docs/SPIKE_PLAN.md`, `docs/superpowers/specs/2026-07-25-shared-control-ui-design.md`.

- [ ] Add localhost instructions and the exact ownership/chord rules; move nested-split completion to Spike 3. Reconcile the existing shared-control design with host-owned delete, Ctrl+Q, and F9/F10 PTY forwarding before claiming the MVP document is authoritative.
- [ ] Run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] Commit `docs: document localhost shared layout`.

### Task 9: Manual acceptance and PR

- [ ] Build the binary and run one `create` plus at least two `join` processes on localhost in separate terminals/PTYs.
- [ ] Verify: each member creates a pane; foreign delete is rejected; host delete succeeds; tab create/delete works; a mixed tab delete is rejected; a late join receives all panes; Ctrl+Q exits cleanly and F9/F10 reach the focused PTY.
- [ ] Fix every failure and repeat this acceptance sequence until it passes.
- [ ] Inspect status/diff, run the final suite, push `codex/spike3-shared-layout`, and open a PR targeting `main`.
