# Free-pane control and chrome Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make idle panes free (no retained controller), retitle pane chrome, white free borders, clickable tabs, and Zellij-inspired contextual footers.

**Architecture:** Extend `LeaseManager` to clear controller on idle and publish empty-controller leases. TUI chrome/footer/mouse hit-testing consume the new states. Keep host-owned PTY and display-name roster unchanged.

**Tech Stack:** Existing Rust / ratatui / crossterm / prost lease path.

**Spec:** `docs/superpowers/specs/2026-07-25-free-pane-chrome-design.md`

---

### Task 1: Commit spec + plan

- [ ] Ensure spec and this plan are on the branch; commit:

```bash
git add docs/superpowers/specs/2026-07-25-free-pane-chrome-design.md docs/superpowers/plans/2026-07-25-free-pane-chrome.md
git commit -m "$(cat <<'EOF'
docs: specify free-pane control and chrome polish

EOF
)"
```

---

### Task 2: Free-pane lease semantics

**Files:** `src/lease.rs`, `tests/lease.rs`, session/TUI lease publish paths as needed

- [ ] Allow empty `controller_peer_id` meaning free
- [ ] On idle timeout path used by host loops: clear controller, bump epoch, publish
- [ ] Input on free pane claims for sender and accepts data
- [ ] Input while another peer actively controls → reject
- [ ] Tests for clear-on-idle, free claim, active reject
- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
feat: clear pane control when idle so panes go free

EOF
)"
```

---

### Task 3: Pane titles + white free borders

**Files:** `src/tui.rs`, docs/README as needed

- [ ] Title `Pane #N  host: …  control: free|name|…`
- [ ] Focused free border white; typing red-orange; remove brown idle style
- [ ] Tests for title helper / border color selection
- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
feat: name panes and use white borders when free

EOF
)"
```

---

### Task 4: Clickable tabs

**Files:** `src/tui.rs`

- [ ] Track tab label hit rects; mouse click switches tab
- [ ] Unit test hit-test
- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
feat: switch tabs with mouse clicks

EOF
)"
```

---

### Task 5: Footer restyle

**Files:** `src/tui.rs`, `README.md`

- [ ] Implement normal / pane / tab footers per spec with accent key styling
- [ ] Update help copy: “type to claim when free”
- [ ] Tests for mode footer content
- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
feat: restyle contextual footer help

EOF
)"
```

---

### Task 6: Docs alignment + verify + PR

- [ ] Align README / design snippets that still describe idle retained control
- [ ] `cargo fmt`, `clippy --all-targets --all-features -- -D warnings`, `cargo test`
- [ ] Push and open PR:

**Title:** `UX: free-pane control, pane titles, tab clicks, footer chrome`

**Body:** Summary + test plan for free lease, titles, borders, tab click, footer.

---

## Success criteria

- Idle panes publish free (empty controller)
- Chrome/borders/footer/tabs match spec
- One commit per task; PR open; CI green
