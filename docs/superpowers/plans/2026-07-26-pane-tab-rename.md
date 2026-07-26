# Mid-session pane and tab rename Implementation Plan

> **For agentic workers:** Execute only in `/Users/pelazas/Desktop/p2pmux-pane-tab-rename` on branch `feat/pane-tab-rename`. Make one local commit per completed task. Do **not** push or open a PR during implementation unless an explicit handoff asks for it; the eventual PR target is `main`.

**Goal:** Ship shared, persistent pane and tab chrome titles through `Ctrl+P e` and `Ctrl+T e`, with a non-expiring modal prompt and ordinal fallback labels.

**Architecture:** `layout::SessionState` is the sole title authority and includes `Option<String>` on its `Pane`/`Tab` snapshot model. Protocol v6 carries titles in descriptors plus one-action rename requests. The coordinator validates/serializes mutations and broadcasts normal `LayoutCommit`s. TUI owns the local prompt state, chord dispatch, input editing, and width-safe chrome rendering, including the agents-overlay location labels. No title is stored in local finder records: the live `SharedLayoutRuntime` snapshot flows through `SharedLayoutNode::snapshot()` and `node::write_snapshot()` to a reattached local renderer, while `SessionStore` records only locate that node.

**Tech Stack:** Rust 2024; existing serde layout model, Prost protocol, Tokio coordinator/session transport, Ratatui + Crossterm TUI, existing unit/integration test suites.

**Spec:** `docs/superpowers/specs/2026-07-26-pane-tab-rename-design.md` (authoritative).

---

## File map

| File | Responsibility |
|---|---|
| `docs/superpowers/specs/2026-07-26-pane-tab-rename-design.md` | Authoritative UX, state, and wire contract |
| `docs/superpowers/plans/2026-07-26-pane-tab-rename.md` | This ordered implementation plan |
| `src/layout.rs` | `Pane.title` / `Tab.title`, normalized title validation, rename mutations, snapshot validation |
| `src/protocol.rs` | Protocol v6, descriptor fields, rename messages/action tags, bounded validation |
| `src/session.rs` | Descriptor ↔ layout conversion and coordinator rename dispatch/rejection mapping |
| `src/tui.rs` | Rename modal, `e` chords/footer, intent/request plumbing, chrome truncation/rendering |
| `Cargo.toml` | Direct `unicode-width` dependency for terminal-cell-width-safe chrome allocation |
| `tests/protocol.rs` | Wire/tag/version/validation coverage |
| `tests/session_layout.rs` | Pure coordinator rename/late-snapshot coverage |
| `tests/session_layout_control.rs` | Shared-layout control-stream commit/rejection coverage |
| `tests/module_surface.rs` | Protocol-version assertion |
| `README.md` | User-facing rename key/behavior documentation, if needed |
| `docs/MVP_DESIGN.md` | Locked controls/chrome text, if needed |

---

## Task 1: Commit the design and plan

**Files:**
- `docs/superpowers/specs/2026-07-26-pane-tab-rename-design.md`
- `docs/superpowers/plans/2026-07-26-pane-tab-rename.md`

- [x] Verify the documents match the locked product decisions, especially lowercase `e`, empty-clears, modal behavior, and protocol v6. Completed in `d12aa60`.
- [x] Commit docs only. Completed in `d12aa60` (`docs: specify pane and tab rename`).

```bash
git add docs/superpowers/specs/2026-07-26-pane-tab-rename-design.md docs/superpowers/plans/2026-07-26-pane-tab-rename.md
git commit -m "docs: specify pane and tab rename"
```

---

Implementation begins at Task 2. Keep Task 1's docs-only commit intact; this review pass is a
separate docs commit.

---

## Task 2: Add authoritative title state and pure layout mutations

**Files:**
- Modify: `src/layout.rs`
- Modify/add: layout unit tests in `src/layout.rs`

- [ ] Add `title: Option<String>` to `Pane` and `Tab`; update every fixture/construction site deliberately rather than defaulting silently.
- [ ] Add a layout-owned normalization helper returning `Result<Option<String>, LayoutError>`: trim Unicode whitespace; empty => `None`; otherwise enforce ≤32 Unicode scalars and no controls. Snapshot validation additionally rejects non-normalized values (`Some("")` and values whose trim changes them).
- [ ] Add `SessionState::rename_pane(requester, base_revision, pane_id, title)` and `rename_tab(...)`. Both must call `check_mutation`, `require_member`, and `ensure_no_reservation`; lookup IDs; normalize/store; then `advance_revision` exactly once. Do not require target hosting. Keep the reservation gate: `reserve_*` captures a base revision that its later `PaneReady` must still match, so allowing a title-only commit during a pending reservation would falsely stale that ready. Rename may proceed after the reservation commits, cancels, fails, or expires.
- [ ] Write focused failing/passing tests for any-member authorization, remote-host rename, clear, whitespace clear, bounds/control failures, unknown IDs, stale revision, pending-reservation rejection, snapshot round-trip/invalid snapshot, and unchanged roots/grids/hosts.
- [ ] Run:

```bash
cargo test --lib layout -- --nocapture
cargo fmt --check
```

- [ ] Commit:

```bash
git add src/layout.rs
git commit -m "feat: store shared pane and tab titles"
```

---

## Task 3: Define protocol v6 rename messages and validation

**Files:**
- Modify: `src/protocol.rs`
- Modify: `tests/protocol.rs`
- Modify: `tests/module_surface.rs`

- [ ] Bump `PROTOCOL_VERSION` from 5 to 6, including explicit test names/assertions and all expected envelopes.
- [ ] Add `PaneDescriptor.title: Option<String>` at Prost tag 5 and `TabDescriptor.title: Option<String>` at tag 3.
- [ ] Add `RenamePane { pane_id: u64 tag 1, title: String tag 2 }` and `RenameTab { tab_id: u64 tag 1, title: String tag 2 }`.
- [ ] Add `LayoutRequest.rename_pane` tag 9 and `rename_tab` tag 10. Extend the exact-one-action count from six to eight, including every request fixture/default.
- [ ] Define `MAX_LAYOUT_TITLE_BYTES: usize = 128`; protocol validation accepts empty title (clear), rejects zero IDs and title byte lengths above the cap. Keep scalar/control/normalization authority in `layout.rs` as specified.
- [ ] Ensure layout-state decode/conversion reaches layout snapshot validation so descriptor `Some("")`, overlong, control-containing, or untrimmed titles cannot render locally.
- [ ] Test byte cap; v6 encode/decode; descriptor optional-title round trip; both exact message tags/actions; empty clear; zero target; multiple action; malformed title; and v5/v7 rejection.
- [ ] Run:

```bash
cargo test --test protocol
cargo test --test module_surface
cargo fmt --check
```

- [ ] Commit:

```bash
git add src/protocol.rs tests/protocol.rs tests/module_surface.rs
git commit -m "feat: add rename layout actions in protocol v6"
```

---

## Task 4: Plumb titles through the coordinator and shared sessions

**Files:**
- Modify: `src/session.rs`
- Modify: `tests/session_layout.rs`
- Modify: `tests/session_layout_control.rs`
- Modify any session fixtures made exhaustive by descriptor/request fields

- [ ] Map title fields in `protocol_layout_state` and `layout_snapshot_from_state`.
- [ ] Import/dispatch `RenamePane` and `RenameTab` in `LayoutCoordinator::handle_request_at`; include both in action counting and call the Task 2 mutations.
- [ ] Preserve existing reject mapping: unknown target → `UnknownId`, invalid title/multiple action → `Malformed`, stale → `Stale`, and non-member → `NotHost`. Do not add title ACLs.
- [ ] Add coordinator tests for remote-member rename of both kinds, commit revision/state contents, stale/unknown/invalid rejections, and a joining member receiving title-bearing `SessionSnapshot`.
- [ ] Add shared-layout control tests proving a member-issued rename reaches peers as `LayoutControlEvent::Commit`; verify a rejection does not mutate title.
- [ ] Run:

```bash
cargo test --test session_layout
cargo test --test session_layout_control
cargo fmt --check
```

- [ ] Commit:

```bash
git add src/session.rs tests/session_layout.rs tests/session_layout_control.rs
git commit -m "feat: replicate pane and tab title changes"
```

---

## Task 5: Implement the modal rename prompt and commands

**Files:**
- Modify: `src/tui.rs`
- Modify/add: TUI unit tests in `src/tui.rs`

- [ ] Refactor modal state enough to make agents overlay and rename mutually exclusive (an enum is preferred over independent booleans). Preserve all existing agents-overlay behavior and its tests (including Ctrl+A double-tap, navigation, click/Enter selection, scrolling, and rendering); do not make drive-by overlay UX changes while adding rename.
- [ ] Add a rename state with target kind/ID, editable `String`, and local validation error. On `Ctrl+P e` capture `focused_pane`; on `Ctrl+T e` capture `current_tab`; exit chord mode when opening.
- [ ] Add lowercase `e` to `handle_pane_chord`, `handle_tab_chord`, `is_chord_command`, and contextual footer segments/text. Keep lowercase `r` exclusively the pane right-split action.
- [ ] Process active rename before `expire_chord_mode`, normal chords, overlay toggle, mouse focus/selection/resize, and PTY forwarding. Implement printable input, Backspace, Enter normalization/validation → `UiIntent::RenamePane`/`RenameTab`, Esc cancel, and Ctrl+Q global detach precedence. Consume unsupported input. Route global Ctrl+Q ahead of either modal as necessary, but otherwise retain the agents overlay's present key semantics.
- [ ] Emit intents to the existing session runner; construct one-action `LayoutRequest`s with title fields and all non-rename action fields `None`. Ensure a prompt closes before a request/rejection is handled and never silently retargets after a layout update.
- [ ] Render the centered `Clear`/bordered prompt plus edit field, prompt-local error, and `Enter save · Esc cancel`; it must be non-idle-expiring.
- [ ] Test chord target capture and `e` recognition/footer; prefill; printable/shifted Unicode and Backspace; Enter sends expected intent including empty clear; invalid stays open; Esc has no intent; timeout does not close; Ctrl+P/Ctrl+T/Ctrl+A/arrows/mouse do not escape the modal; Ctrl+Q wins; no PTY forwarding; and overlay/rename exclusion.
- [ ] Run:

```bash
cargo test --lib tui -- --nocapture
cargo fmt --check
```

- [ ] Commit:

```bash
git add src/tui.rs
git commit -m "feat: add modal pane and tab rename prompts"
```

---

## Task 6: Render custom chrome safely and finish documentation

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/tui.rs`
- Modify: `README.md` (if controls documentation is user-facing there)
- Modify: `docs/MVP_DESIGN.md` (if its locked controls/chrome paragraph must name `e`)
- Modify/add: TUI rendering tests in `src/tui.rs`

- [ ] Replace ordinal-only `tab_label`/`pane_title` inputs with title-aware helpers: use custom title exactly when `Some`, otherwise `Tab #N` / `Pane #N`; retain existing host/control pane chrome and active-tab styling. Carry the resolved tab/pane labels into `AgentOverlayRow` and render those labels in its location line under the same custom-title/ordinal-fallback rule; rows remain selected by `pane_id` and do not append an ordinal beside a custom title.
- [ ] Add `unicode-width` as a direct dependency and replace scalar-count width calculations on these render paths with terminal-cell-width-aware helpers. Add display-width-aware single-line ellipsis truncation at tab, pane, and agents-overlay render allocation boundaries. For pane chrome, allocate/protect the title first (custom or ordinal), ellipsize it within the block-title width, and only then append host/control from leftover width; host/control may clip or disappear before title does. Test narrow widths, Unicode-width characters, the title-versus-host/control precedence, overlay custom-title/fallback labels, and no border/tab overlap. Do not alter the canonical stored title while truncating.
- [ ] Update README/MVP controls text only where necessary: document `Ctrl+P e` / `Ctrl+T e`, all-member label authority, Enter/Esc, and blank-to-ordinal behavior; explicitly do not describe the unrelated session CLI `rename` as pane/tab rename.
- [ ] Run the complete quality gate:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

- [ ] Commit:

```bash
git add Cargo.toml Cargo.lock src/tui.rs README.md docs/MVP_DESIGN.md
git commit -m "feat: render shared pane and tab titles"
```

If no README/MVP change is warranted, omit those paths from `git add` but retain the render/tests
commit under the same message. If user-facing docs changed, commit them separately as a small
`docs:` follow-up after the rendering feature commit.

---

## Manual acceptance

1. Create or join with two admitted members; have either member focus a pane hosted by the other,
   press `Ctrl+P` then `e`, type `build logs`, and Enter. Both clients show `build logs` as that
   pane's border label while host/control metadata remains correct.
2. Press `Ctrl+T` then `e` on a tab, replace its prefilled custom title, and Enter. All clients
   see the updated tab label and active styling.
3. Reopen either prompt, erase its contents (or enter whitespace), press Enter, and verify exactly
   `Pane #N` / `Tab #N` returns.
4. Leave a prompt open longer than two seconds; it remains open. Esc closes without sending title
   text to the focused shell. Ctrl+Q detaches rather than writing to the prompt/PTY.
5. While open, try Ctrl+P, Ctrl+T, Ctrl+A, arrows, clicking panes/tabs, and typing arbitrary
   commands; none navigate, toggle overlay, resize, or reach a PTY.
6. Attempt a 33-scalar title and a control character path; the prompt keeps focus and shows a
   validation error. Confirm the coordinator rejects stale/removed targets without corrupting
   state.
7. Attach/rejoin a second client after the commits; it receives both custom titles. Detach and
   resume the local client while the node stays alive; titles remain.
8. Check narrow terminal widths: tab/pane chrome ellipsizes cleanly without overwriting borders or
   adjacent labels.

## Success criteria

- Protocol v6 is the only interoperable wire version and carries normalized optional titles in all
  layout snapshots/commits.
- Every admitted member can rename any pane/tab; permissions and PTY/control behavior otherwise do
  not change.
- Rename prompt modality, timeout, Enter/Esc, overlay interaction, and Ctrl+Q behavior match the
  authoritative spec.
- Empty custom titles reliably restore ordinal labels, and all layers have focused regression
  coverage plus a green full suite.
- All implementation commits are local, one per task, and no push/PR occurs without explicit
  handoff.
