# Option+Shift drag pane resize with host PTY reflow Implementation Plan

**Goal:** Add an Option/Alt+Shift drag resize gesture whose ratio is shared and
revisioned, then reflow real hosted PTYs, VT screens, and published pane grids
after a ratio or outer-window change.

**Architecture:** Persist `first_share_bps` on each split and make allocation
ratio-aware with recursive local minima. A drag holds an overlay in
`MultiPaneTui` and sends one `SetSplitRatio` request on release. The
coordinator authorizes that ratio mutation for any admitted member. Each pane
host then derives grids from its own local chrome, resizes only its own
`PtyHost`/`HostScreen`, and publishes those grids through a host-only,
revisioned `UpdatePaneGrids` request. A guest only consumes the resulting
layout and screen stream.

**Tech Stack:** Rust, ratatui, crossterm mouse events, portable-pty, vt100,
prost, existing revision/reservation coordinator.

**Spec:** `docs/superpowers/specs/2026-07-26-option-shift-drag-resize-design.md`

## File map

| File | Responsibility |
|---|---|
| `src/layout.rs` | Split ratio model, validation, nearest-ancestor mutation, hosted grid mutation |
| `src/protocol.rs` | v4 ratio/grid request fields, `LayoutSplit` ratio wire field, validation |
| `src/session.rs` | Coordinator dispatch, protocol/model conversions, host-grid authority |
| `src/tui.rs` | Ratio-aware geometry, drag overlay, event dispatch, runtime PTY reflow and grid requests |
| `src/pty_host.rs` | Portable-pty master resize wrapper |
| `src/screen.rs` | vt100 resize and forced full snapshot publication |
| `tests/layout.rs` | Ratio state, rewrite, revision, and grid ownership tests |
| `tests/protocol.rs` | v4 wire shape/default/validation tests |
| `tests/session_layout.rs` | Coordinator ratio/grid request authorization and race tests |
| `tests/session_layout_control.rs` | v4 layout-control transport coverage |
| `tests/pty_host.rs` | Real `MasterPty::resize` smoke coverage |
| `tests/screen.rs` | Resized screen snapshot and guest replacement coverage |
| `README.md` | Dynamic-grid interaction and behavior documentation |

## Tasks

### Task 1: Commit the design and execution plan

- [ ] Add the normative design and this checkbox plan at the exact paths below.
- [ ] Confirm no application source, test, or README changes are staged.
- [ ] Commit:

```bash
git add docs/superpowers/specs/2026-07-26-option-shift-drag-resize-design.md docs/superpowers/plans/2026-07-26-option-shift-drag-resize.md
git commit -m "docs: plan Option+Shift+drag pane resize with PTY resize"
```

### Task 2: Add authoritative split ratios and host grid mutation to the pure layout model

**Files:** `src/layout.rs`, `tests/layout.rs`

- [ ] Write failing `tests/layout.rs` cases for a new split defaulting to
  `first_share_bps: 5000`; snapshot validation rejecting 0 and 10000; a
  `SetSplitRatio`-equivalent model method choosing the nearest matching
  ancestor for `(pane_id, axis)`; and `UnknownPane`/no-matching-axis,
  stale-revision, non-member, and pending-reservation failures.
- [ ] Write failing preservation cases: nested split ratios survive a pane
  delete that retains the split and member-removal rewrites, while a newly
  created split starts at 5000.
- [ ] Add failing grid-update cases proving one admitted host may update only
  its own nonempty, duplicate-free valid pane-grid batch, the revision advances
  once, and invalid/non-host/stale/frozen batches leave the snapshot unchanged.
- [ ] Implement `Node::Split { axis, first_share_bps, first, second }`, ratio
  constants/validation, recursive nearest-ancestor lookup, strict
  `SessionState::set_split_ratio`, and strict host-only
  `SessionState::update_pane_grids`. Carry the ratio through every tree rebuild
  (`replace_leaf`, `remove_leaf`, and member-removal paths) without silently
  changing it.
- [ ] Run focused tests:

```bash
cargo test --test layout
```

- [ ] Commit:

```bash
git add src/layout.rs tests/layout.rs
git commit -m "feat: store revisioned split ratios and hosted pane grids"
```

### Task 3: Define and validate the v4 wire contract

**Files:** `src/protocol.rs`, `tests/protocol.rs`

- [ ] First add failing protocol tests asserting `PROTOCOL_VERSION == 4`, the
  exact new `LayoutSplit.first_share_bps` field/tag, absent-ratio decoding to
  5000 through the session conversion boundary, and rejection of explicit 0,
  10000, unknown axis, and malformed ratio requests.
- [ ] Add failing wire/validation tests that `LayoutRequest` still accepts
  exactly one action and carries `SetSplitRatio { pane_id, axis,
  first_share_bps }` plus `UpdatePaneGrids { panes }`; require a nonempty,
  duplicate-free valid grid batch and preserve existing request fields/tags.
- [ ] Bump the version 3 → 4 and add new, non-conflicting protobuf tags and
  message types. Validate the optional split field as default-5000 when absent
  and as `1..=9999` when present. Validate both new request actions before any
  session code sees them.
- [ ] Run focused tests:

```bash
cargo test --test protocol
```

- [ ] Commit:

```bash
git add src/protocol.rs tests/protocol.rs
git commit -m "feat: add v4 split ratio and pane grid protocol"
```

### Task 4: Route ratio and grid requests through the coordinator and control transport

**Files:** `src/session.rs`, `tests/session_layout.rs`, `tests/session_layout_control.rs`

- [ ] Add failing coordinator tests for an admitted non-host successfully
  setting a ratio; first-commit-wins stale rejection; no matching ancestor;
  invalid ratio; and a pending pane reservation freezing ratio mutation.
- [ ] Add failing grid reconciliation tests for host-only batch admission,
  strict stale behavior, reservation freeze, atomic rejection when one entry is
  invalid/not hosted, full `LayoutCommit` pane metadata, and two hosts settling
  by rebasing the loser on the later revision.
- [ ] Add an async control-stream test that serializes/deserializes both v4
  request actions and delivers their commits/rejections to a member.
- [ ] Extend action counting/dispatch, request conversion, reject mapping, and
  `protocol_node` / `layout_node_from_protocol` to preserve ratios. Reuse the
  existing revision and reservation checks; do not create a second authority
  path or a client-side ratio retry.
- [ ] Run focused tests:

```bash
cargo test --test session_layout
cargo test --test session_layout_control
```

- [ ] Commit:

```bash
git add src/session.rs tests/session_layout.rs tests/session_layout_control.rs
git commit -m "feat: coordinate shared ratios and host grid updates"
```

### Task 5: Make the real PTY and VT host screen resizable

**Files:** `src/pty_host.rs`, `src/screen.rs`, `tests/pty_host.rs`, `tests/screen.rs`

- [ ] Add failing `tests/pty_host.rs` coverage that spawns the existing shell
  fixture, calls `PtyHost::resize(PtySize)`, then keeps its reader/writer alive
  and exits cleanly. Keep the test macOS-compatible and bounded by the current
  timeout pattern.
- [ ] Add failing `tests/screen.rs` coverage that `HostScreen::resize(rows,
  cols)` changes its vt100 dimensions, advances sequence, emits a complete
  snapshot that replaces a guest's old-size parser, and cannot be sent as a
  delta based on the old grid.
- [ ] Implement `PtyHost::resize` by retaining and resizing its
  `MasterPty`. Implement `HostScreen::resize` with dimension validation,
  vt100 parser resizing, a refreshed `previous` screen, and a frame whose
  `base_sequence` forces existing stream selection to choose a snapshot.
- [ ] Run focused tests:

```bash
cargo test --test pty_host
cargo test --test screen
```

- [ ] Commit:

```bash
git add src/pty_host.rs src/screen.rs tests/pty_host.rs tests/screen.rs
git commit -m "feat: resize hosted PTYs and vt100 screens"
```

### Task 6: Make layout geometry ratio-aware and minimum-safe

**Files:** `src/tui.rs`

- [ ] Add failing `src/tui.rs` unit tests for 5000 compatibility with current
  equal allocation; non-50/50 left/right and top/bottom allocation; nested
  recursive 3×3 outer minima; and deterministic safe allocation when the local
  area cannot meet those minima.
- [ ] Add tests for a geometry override that changes only rendered rectangles,
  leaves `MultiPaneTui::snapshot()` byte-for-byte/equality unchanged, and is
  used consistently by pane-content/grid calculations.
- [ ] Replace `allocate_node`'s `/ 2` split with the documented integer
  basis-point allocation and recursive minimum calculation. Add a pure
  override mechanism keyed to the locally located split for drag rendering;
  do not mutate `LayoutSnapshot`.
- [ ] Run focused TUI unit tests while iterating:

```bash
cargo test tui::tests
```

- [ ] Commit:

```bash
git add src/tui.rs
git commit -m "feat: allocate shared panes by ratio with local minima"
```

### Task 7: Add captured-modifier drag handling, overlay, and ratio intent

**Files:** `src/tui.rs`

- [ ] Write failing pure-TUI tests for: plain left drag retaining text
  selection; Alt+Shift down in a content rect starting a pending resize without
  selection; modifier loss on later drag/up not cancelling it; two-cell axis
  lock (including horizontal tie); nearest matching ancestor resolution; sign
  correctness for first versus second child; no-match consume/cancel; and one
  release intent using the mouse-down revision.
- [ ] Add tests that a newer authoritative layout application and a stale
  request rejection clear an active overlay, and that no request is emitted for
  below-threshold or unchanged drags.
- [ ] Add `UiIntent::SetSplitRatio`, a private captured resize-drag state, and
  mouse helpers on `MultiPaneTui`. In `SharedLayoutRuntime::run`, dispatch
  modifier left down/drag/up to that state before selection handling. Capture
  modifiers only on down; keep missing-Alt/Shift events on the existing
  selection path. Render the ratio overlay through Task 6's override.
- [ ] Add the space-aware normal footer segment
  `Option+Shift+drag RESIZE` and a renderer test proving it appears when room
  permits without displacing higher-priority footer content.
- [ ] Run focused tests:

```bash
cargo test tui::tests
```

- [ ] Commit:

```bash
git add src/tui.rs
git commit -m "feat: resize panes with Option Shift drag overlays"
```

### Task 8: Reflow local hosted panes after commits and outer-window resize

**Files:** `src/tui.rs`

- [ ] First add tests around `SharedLocalPane`/runtime helpers using a
  testable resize abstraction or controlled local pane fixture: only local
  panes are resized; remote panes are never resized; changed ratios reflow the
  affected descendant leaves; unchanged grids emit no update; and a resize
  causes a fresh screen frame for viewers.
- [ ] Add runtime-helper tests for an outer `Event::Resize` calculating grids
  from current chrome for every locally hosted pane, including panes in an
  unselected tab, and for stale grid-update retry recomputing from the current
  revision without resending a stale ratio.
- [ ] Extend `SharedLocalPane` with a resize operation that calls Task 5's PTY
  and screen methods, publishes the fresh frame, and reports the resulting
  grid. On every authoritative state application, compare old/new ratio-aware
  geometry before replacing the snapshot, reflow changed local leaves, then
  send one host-only `UpdatePaneGrids` batch with the current revision. On
  `Event::Resize`, use the same reflow path. Do this after commit/release, not
  per mouse movement.
- [ ] Extend `handle_intent`/`send_request` for `SetSplitRatio` with its
  captured base revision. Clear overlay on every newer authoritative layout
  state and on its rejection. Route grid update rejection to recomputation of
  current local geometry; never blindly retry an absolute ratio.
- [ ] Run focused tests:

```bash
cargo test tui::tests
```

- [ ] Commit:

```bash
git add src/tui.rs
git commit -m "feat: reflow hosted pane grids after layout changes"
```

### Task 9: Align user documentation and perform the full verification pass

**Files:** `README.md`, `docs/superpowers/specs/2026-07-26-option-shift-drag-resize-design.md`, `docs/superpowers/plans/2026-07-26-option-shift-drag-resize.md`

- [ ] Update README status/interaction text to remove the claims that shared
  panes, split-existing panes, and outer terminal resize have immutable grids.
  Document `Option+Shift+drag` resize, shared ratios versus host-owned absolute
  grids, commit-on-release behavior, and that guests never resize remote PTYs.
- [ ] Re-read the design and plan against the implementation. Check every
  locked decision: captured modifiers, one-axis lock, nearest ancestor,
  overlay cancellation, no stale ratio retry, 1..=9999/default 5000, v4,
  reservation freeze, recursive minima, PTY/HostScreen resize, grid commit,
  remote-guest rule, outer-window reflow, footer, and non-goals.
- [ ] Run the required whole-repository verification:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- [ ] Commit:

```bash
git add README.md docs/superpowers/specs/2026-07-26-option-shift-drag-resize-design.md docs/superpowers/plans/2026-07-26-option-shift-drag-resize.md
git commit -m "docs: document dynamic pane resize behavior"
```

## Success criteria

- A plain drag still selects text, while an Alt+Shift drag is captured at down,
  locks to one axis, renders only an ephemeral overlay, and sends at most one
  current-revision ratio request on release.
- Ratios are validated, shared, revisioned, preserved through valid rewrites,
  and allocated with recursive local minima without cross-peer ratio churn.
- Any admitted member can set a ratio; only a pane host can publish that pane's
  grid; strict revision and reservation freeze apply to both paths.
- A committed ratio or outer-window resize resizes each affected local PTY and
  HostScreen, publishes a replacement screen snapshot, and commits host-owned
  grid metadata. Remote guests only consume those updates.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` pass. Execution produces exactly one commit for each task above.
