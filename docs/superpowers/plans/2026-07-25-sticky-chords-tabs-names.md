# Sticky Chords, Tabs Chrome, and Display Names Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship sticky pane/tab chords with contextual footers, mouse focus, `Tab #N` active highlighting, and persistent peer-visible display names.

**Architecture:** Keep layout/session authority unchanged. TUI owns sticky chord state, mouse hit-testing, and chrome. New `config` module loads/saves `display_name`. Protocol/layout `Member` carries optional/required display name for roster chrome; cryptographic peer id remains identity.

**Tech Stack:** Rust 2024, clap, ratatui/crossterm (mouse), toml + serde for config, existing prost layout protocol.

**Spec:** `docs/superpowers/specs/2026-07-25-sticky-chords-tabs-names-design.md`

---

## File map

| File | Responsibility |
|------|----------------|
| `src/config.rs` (new) | Load/save `~/.config/p2pmux/config.toml`, validate names |
| `src/lib.rs` | Export `config` module |
| `src/cli.rs` | `config` subcommands; `--name`; resolve name on create/join |
| `src/layout.rs` | `Member.display_name` |
| `src/protocol.rs` | `MemberDescriptor.display_name` (+ Join field if admission needs it) |
| `src/session.rs` | Plumb names through admission/snapshots |
| `src/tui.rs` | Sticky chords, footers, tab chrome, mouse focus, name chrome |
| `tests/*` | Focused coverage per task |
| `README.md` | Document new UX + config + Zellij mouse note |

---

### Task 1: Spec already on branch — verify and commit docs if needed

**Files:**
- `docs/superpowers/specs/2026-07-25-sticky-chords-tabs-names-design.md`
- `docs/superpowers/plans/2026-07-25-sticky-chords-tabs-names.md`

- [ ] Ensure both files exist on the feature branch
- [ ] Commit if not already committed:

```bash
git add docs/superpowers/specs/2026-07-25-sticky-chords-tabs-names-design.md docs/superpowers/plans/2026-07-25-sticky-chords-tabs-names.md
git commit -m "$(cat <<'EOF'
docs: specify sticky chords, tab chrome, and display names

EOF
)"
```

---

### Task 2: Sticky chord modes + contextual footer

**Files:**
- Modify: `src/tui.rs` (`handle_key`, footer helpers, tests)
- Modify: `README.md` chord docs

- [ ] **Step 1: Write failing tests** for:
  - pane mode stays across two Right arrows
  - Esc clears mode without forwarding
  - a normal char while in pane mode clears mode and is returned/forwarded once
  - footer text switches for Pane / Tab / Normal

- [ ] **Step 2: Run tests — expect FAIL**

```bash
cargo test --lib sticky_ -- --nocapture
cargo test --lib footer_ -- --nocapture
```

- [ ] **Step 3: Implement** sticky mode (do **not** clear `chord_mode` before handling chord keys; only clear on Esc / quit / forward / mode switch). Update `shared_footer_text` / render path to use mode-specific help.

- [ ] **Step 4: Tests PASS**

- [ ] **Step 5: Commit**

```bash
git add src/tui.rs README.md
git commit -m "$(cat <<'EOF'
feat: keep pane and tab chords sticky with contextual help

EOF
)"
```

---

### Task 3: Tab labels + active highlight

**Files:**
- Modify: `src/tui.rs` tab bar rendering + tests

- [ ] **Step 1: Failing tests** asserting labels `Tab #1`… and active tab uses red bg + white fg (TestBackend / buffer cell inspection)

- [ ] **Step 2: Implement** ordinal labels from `tabs` order; style active vs inactive

- [ ] **Step 3: Tests PASS + commit**

```bash
git add src/tui.rs
git commit -m "$(cat <<'EOF'
feat: label tabs and highlight the active tab

EOF
)"
```

---

### Task 4: Mouse click focuses pane

**Files:**
- Modify: `src/tui.rs` (hit test helper + event loop mouse enable)
- Tests: hit-test unit tests

- [ ] **Step 1: Failing test** — given geometry panes, click coordinate maps to correct `pane_id`; miss maps to None

- [ ] **Step 2: Implement** `pane_at(x,y)`, enable mouse capture in shared-layout loops, on left click set `focused_pane` (no lease/input). Keep chord mode.

- [ ] **Step 3: README note** about Zellij mouse

- [ ] **Step 4: Commit**

```bash
git add src/tui.rs README.md
git commit -m "$(cat <<'EOF'
feat: focus panes with mouse clicks

EOF
)"
```

---

### Task 5: Local display-name config + CLI

**Files:**
- Create: `src/config.rs`
- Modify: `src/lib.rs`, `src/cli.rs`
- Create/modify: `tests/config.rs` or unit tests in `config.rs`

- [ ] **Step 1: Failing tests** for validate/save/load round-trip; reject empty/long/control

- [ ] **Step 2: Implement** config path (`dirs` crate or `XDG_CONFIG_HOME`/`HOME`), toml serde, `p2pmux config set|get name`

- [ ] **Step 3: Wire create/join** to resolve name: `--name` > config > TTY prompt > non-interactive error

- [ ] **Step 4: Commit**

```bash
git add src/config.rs src/lib.rs src/cli.rs Cargo.toml tests README.md
git commit -m "$(cat <<'EOF'
feat: add persistent display name config

EOF
)"
```

---

### Task 6: Share display names in layout/protocol/chrome

**Files:**
- Modify: `src/layout.rs`, `src/protocol.rs`, `src/session.rs`, `src/tui.rs`
- Modify: related tests (`tests/layout.rs`, `tests/protocol.rs`, `tests/session_*`)

- [ ] **Step 1: Extend** `Member` / `MemberDescriptor` with `display_name: String` (validate length on decode)
- [ ] **Step 2: Plumb** name from create/join admission into coordinator roster and snapshots
- [ ] **Step 3: Chrome helper** `format_member_label(name, peer_id, roster)` with duplicate disambiguation `name · aabbccdd`
- [ ] **Step 4: Replace** `short_peer`-only host/controller chrome with labels
- [ ] **Step 5: Update tests**; full `cargo fmt`, `clippy --all-targets --all-features -- -D warnings`, `cargo test`
- [ ] **Step 6: Commit**

```bash
git add src/layout.rs src/protocol.rs src/session.rs src/tui.rs tests README.md docs
git commit -m "$(cat <<'EOF'
feat: advertise display names in the shared roster

EOF
)"
```

---

### Task 7: Push and open PR

- [ ] Push branch to origin
- [ ] Open PR against `main` with summary + test plan covering sticky chords, footer, tabs, mouse, config, roster names
- [ ] Ensure CI green or fix follow-ups with additional commits

**PR title:** `UX: sticky chords, tab chrome, mouse focus, display names`

**PR body sketch:**

```markdown
## Summary
- sticky Ctrl+P/Ctrl+T modes with contextual footer help
- Tab #N labels with red active highlight
- mouse click focuses panes
- persistent display names via config + roster

## Test plan
- [ ] cargo fmt/clippy/test
- [ ] sticky pane focus with repeated arrows
- [ ] contextual footers
- [ ] config set name + create/join chrome
- [ ] duplicate name disambiguation
- [ ] mouse focus (outside Zellij)
```

---

## Success criteria

- Spec behaviors implemented
- One commit per task (2–6) plus docs commit
- PR opened from the feature worktree branch
- CI quality gates pass
