# Detach / Resume Implementation Plan

> **For Codex:** implement commit-by-commit in this worktree. One logical commit per section below. TDD where practical. Do not include offline-pane grace in this PR.

**Branch:** `feat/detach-resume`  
**Worktree:** `/Users/pelazas/Desktop/p2pmux-detach-resume`  
**Goal:** Session-scoped headless node + TUI client; Ctrl+Q detaches; `p2pmux --resume` picker reattaches.

**Agreed (Cursor + Codex `gpt-5.6-terra`):** Ship detach/resume in this PR. Defer offline placeholders/grace to a follow-up.

---

## Commit 1 — `feat: add live session descriptors and names`

Add `src/session_store.rs`, tests, export from `lib.rs`.

- Descriptor at `~/Library/Application Support/p2pmux/sessions/<id>.json`
- Socket path `/tmp/p2pmux-$UID/<id>.sock`
- Fields: version, opaque id, memorable name, socket path, node PID, role (`coordinator`/`member`), created_at; reject malformed
- 0700 dirs, 0600 files, atomic write/rename
- Memorable name generation + rename validation
- List without claiming live; stale cleanup only after failed socket probe
- Injected paths for tests

Do **not** store layout, screens, PTYs, tickets, or focus on disk.

---

## Commit 2 — `feat: define the private node-client socket protocol`

Add `src/local_ipc.rs` + `tests/local_ipc.rs`. Isolated from Iroh wire protocol.

Messages: client hello (terminal size); attach accepted/rejected (`already attached`); initial snapshot (room name, role, summary, layout, screens, leases, rosters, tab/focus); incremental updates; client input/structural intent/resize/focus/detach/rename/shutdown; detach + shutdown acks.

Bounded outbound queues; attachment generation/token so stale writers cannot detach a newer client.

---

## Commit 3 — `refactor: extract the headless shared-layout node`

Add `src/node.rs`. Extract non-terminal half of `SharedLayoutRuntime` into `SharedLayoutNode`.

- Owns host/member, Iroh, dispatcher, PaneServer, PTYs, screens, remote subs, leases, agent sampling
- Accepts node commands (not crossterm); emits render-state for one client
- Owns last local tab/focus while alive
- `release_all_local_control()` on detach/EOF
- Keep temporary foreground adapter until client exists so existing tests stay green

---

## Commit 4 — `feat: run a session-scoped background node`

Private mode e.g. `p2pmux __node --bootstrap <file>`. Parent writes 0600 bootstrap, launches child with new process group + null stdio, waits for socket ready.

Node: Iroh + panes + socket + descriptor before ready; owns rendezvous for coordinator lifetime; joins while detached OK; one client only; detach/EOF releases leases and keeps session; shutdown cleans everything.

No launchd.

---

## Commit 5 — `feat: add the local TUI client runtime`

Socket-backed client; keep MultiPaneTui/ratatui/chords/mouse/TerminalGuard.

- Ctrl+Q → Detach (not Shutdown), print resume/attach/kill hints
- Socket EOF → restore terminal, report node ended; no destructive cleanup
- Top bar / title: `p2pmux (<memorable-name>)`
- Persist tab/focus to node after local changes

---

## Commit 6 — `feat: add resume picker and live-session commands`

- bare `p2pmux`: picker if live sessions else create/onboarding
- `p2pmux --resume`: always picker
- `attach <name>`, `kill <name> [--yes]`, `rename <old> <new>`
- Keep `create`/`join`; `--name` remains peer display name; add room/session name option if needed
- Picker: ↑/↓, type-to-filter, Enter; show memorable name, coordinator display name, tab/pane counts, distinct hosts, created date, running time
- Probe sockets; live only
- Coordinator kill: warn + tty confirm; `--yes` for non-interactive

---

## Commit 7 — `test: verify detach-resume lifecycle and document it`

Full suite + README/MVP_DESIGN updates. Document limits: one local client, no takeover, no launchd, no disk screen restore, no coordinator failover, offline panes follow-up.

---

## Out of scope (follow-up PR)

Offline host grey placeholders, shared grace countdown, post-grace peer removal.

---

## Success criteria

1. Create session → Ctrl+Q detaches → node stays up → `--resume` / `attach` works  
2. Join still works while detached  
3. Second attach refused  
4. Leases cleared on detach  
5. Last tab/focus restored on reattach  
6. `kill` tears down; coordinator warns  
7. Rename works; top bar shows memorable name  
