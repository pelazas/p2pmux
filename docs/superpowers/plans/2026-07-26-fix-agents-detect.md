# Fix agents overlay detection (Cursor `agent`, Pi node, OpenCode)

## Problem

Overlay shows **No agents running** while panes clearly run Cursor Agent (`agent --yolo`) and Pi. Current matcher only accepts exact exe basenames `claude|codex|cursor-agent|pi`. Live processes often look like:

- Cursor: argv0 `/…/bin/agent` with args containing `…/cursor-agent/…/index.js` (exe may be `node`/`bash`)
- Pi: `#!/usr/bin/env node` → exe basename `node`, cmdline contains `pi` / `pi-coding-agent` / `cli.js`
- OpenCode: binary `opencode` (add to allowlist)

## Goal

1. Detect Claude Code, Codex, Cursor Agent, Pi, OpenCode under hosted PTY trees via **cmdline/argv heuristics**, not exe basename alone.
2. Overlay lists: agent label, coarse activity (`idle`/`working`/`done`), cwd/location, pane #, host.
3. Enter **or click** a row focuses that pane and switches tab if needed.
4. Verify with a real terminal session (spawn agents under panes, open Ctrl+A) and iterate until green.

## Non-goals

- Parsing agent chat transcripts for “what it’s coding”
- Matching bare system `*Agent*` daemons outside the pane tree
- Matching bare `cursor` editor

## Detection rules (v2)

Sampler must capture for each process: `pid`, `parent_pid`, `exe_basename`, `name`, `cmdline: Vec<String>` (argv), optional `cwd`, optional `start_time`.

Classify using `AgentKind::from_process(exe_basename, name, cmdline)`:

| Kind | Match (any one) |
|------|-----------------|
| Claude | basename/name/argv0 == `claude` |
| Codex | basename/name/argv0 == `codex` (PTY-tree scoped; ignores ChatGPT.app helpers outside tree) |
| Cursor | basename/name/argv0 == `cursor-agent` **OR** any argv string contains `cursor-agent` (covers live `…/bin/agent --use-system-ca …/cursor-agent/…/index.js`) |
| Pi | basename/name/argv0 == `pi` **OR** any argv contains `pi-coding-agent` **OR** (exe `node` AND an argv path ends with `/bin/pi` or contains `@earendil-works/pi`) |
| OpenCode | basename/name/argv0 == `opencode` **OR** any argv contains `/opencode` or ends with `opencode` |

Cursor matching is **argv-based** (not a separate `exe_path` field). Tests must include a fixture mirroring live Cursor argv.

Wire values: keep existing + add `opencode`. Protocol validation allowlist updated; bump protocol only if required by exact-version policy (same v4 additive string allowlist is OK).

Still: only processes under the pane’s PTY session PID; deterministic depth/start/pid pick.

## UI

- Row format roughly: `▸ Cursor Agent  working  ~/proj  Pane #1  host: pelazas`
- Empty state unchanged when truly none
- Mouse: left-click on a row → same as Enter (jump + close). Hit-test row rects while overlay open; clicks outside close or ignore (prefer ignore outside / Esc still closes)

## Tasks / commits

1. **docs** — short design note for detection v2 + mouse jump
2. **detect** — cmdline in snapshot; new matchers + unit fixtures for agent/node/pi/opencode
3. **protocol** — allow `opencode` kind in validation + tests
4. **overlay UI** — richer row text; mouse hit-test jump
5. **manual verify** — build; run `p2pmux create`; in panes start `agent --yolo` / `pi` (or stand-ins); Ctrl+A shows rows; Enter/click jumps incl. cross-tab; commit any fixups; open PR

Worktree: `/Users/pelazas/Desktop/p2pmux-fix-agents-detect` branch `fix/agents-detect-modal`. One commit per task. Push + `gh pr create` at end.
