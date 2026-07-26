# p2pmux

A brew-installable **macOS** terminal multiplexer where **each pane’s process runs on its host’s machine**, and members in one encrypted peer-to-peer session share layout, presence, and can take control of panes.

**Shared control surface — not a shared computer.**

## Core idea

- Every user in a session can create tabs and panes.
- Each pane is backed by a **local PTY** on that user’s machine.
- Processes (shell, Docker, Claude Code, etc.) run on the host’s hardware — PATH, env, API keys, and subscriptions stay on that Mac.
- Other members can watch live output and take control to type into that pane.
- Host/guest is **per pane**, not per person.

Example: Pelazas starts Claude Code in his pane (his subscription). Tis takes control and prompts a code change. Nothing runs on Tis’s machine for that pane; Pelazas never needs Tis’s keys. At the same time, Tis can host panes that Pelazas joins.

## What it is / isn’t

**Is**

- Lightweight local binary (`brew install …` later)
- Zellij-like tabs, panes, and nested splits
- Real-time multiplayer presence (who’s on which tab/pane)
- End-to-end encrypted peer-to-peer pane streaming (+ relay when NAT requires it)
- Interactive access (spectate + take control)

**Isn’t**

- A cloud-hosted execution environment
- A shared remote VM where everyone’s processes run
- An agent orchestration / rules engine / sandbox platform

## Trust warning

This is a **fully trusted shared-shell** session. Anyone with the join ticket can see every pane and may obtain interactive control of available terminals (run commands, see output, touch files reachable to that macOS user).

Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege Mac account and avoid production credentials in shared panes.

Processes and credential *files* stay on the pane host’s Mac (not uploaded to peers). That does **not** stop a controller from using or displaying them via the shared shell.

## Status

Spike 3 provides a localhost shared layout for up to eight members. The coordinator owns a
revisioned tab/split tree; every pane is a fixed-grid PTY owned by the member that created it.
Each process serves its own locally hosted panes directly to the other admitted members.

This is still a dogfooding spike: relay/internet validation, disconnect grace, coordinator
failover, presence, and dynamic resize remain later work. The protocol deliberately has no
resize message: each pane grid is fixed when that pane is created and is letterboxed or clipped
by smaller local rectangles.

## Local Spike 1

Run `cargo run -- local` to start one local shell. Press Ctrl+Q to leave p2pmux.

The PTY grid is fixed from the terminal size at startup. Resizing the outer terminal never resizes
the child shell or vt100 parser: larger windows leave extra cells blank and smaller windows crop
the upper-left fixed viewport. Dynamic resize is intentionally outside Spike 1 and the MVP wire
protocol.

To dogfood the shared layout on one Mac:

```text
Terminal 1: cargo run -- create
Terminal 2: cargo run -- join <printed 10-character code>
```

Set a peer-visible display name once with `p2pmux config set name pelazas`; inspect it with
`p2pmux config get name`. It is stored in `$XDG_CONFIG_HOME/p2pmux/config.toml` (or
`~/.config/p2pmux/config.toml`). `create` and `join` accept `--name <name>` to override and save
the value for that run and future sessions.

`create` prints `Join with: p2pmux join <CODE>`, waits for Enter so you can copy it, then
enters the shared-layout TUI. The same code stays in the footer. `join` first receives the
authoritative layout, then attaches directly to every remote pane.
Short join codes resolve through a restrictive local cache on the same Mac, so they are for current
dogfooding only; they work while the corresponding `create` process is alive and are removed when
it exits. Long `p2pmux-v1:` tickets remain accepted for backwards compatibility.

Only one peer controls a pane while they are actively typing. After about eight seconds without
activity, the host clears the controller and the pane becomes free. The next member's ordinary key
claims the free pane and is delivered as its first input; active typing is protected, so there is
no forced takeover. A pane's host owns its PTY, not its control lease: newly created split and tab
panes start free. Ctrl+Q exits only the local p2pmux view; F9 and F10 continue through to the
focused PTY.

Shared-layout commands are sticky local mux modes and never reach a PTY. `Ctrl+P` or `Ctrl+T`
enters its mode; use the listed command repeatedly, press `Esc` to cancel, or type any normal key
to leave the mode and send that key to the focused PTY.

`Ctrl+A` opens the Agents overlay, which lists only supported coding agents running below hosted
pane shells: Claude Code (`claude`), Codex (`codex`), Cursor Agent (`cursor-agent`), and Pi
(`pi`). It shows the agent's best-effort working directory, pane host, control holder, and
idle/working/done state. Press `Esc` to close, use arrows or `j`/`k` to select, and press Enter
to jump to that pane (including on another tab). To retain readline's beginning-of-line shortcut,
press `Ctrl+A` twice within 400ms to forward one Ctrl+A to the focused PTY instead. The overlay is
keyboard-only in v1. Working directories are shared with every member as part of the existing
trusted shared-shell model, so do not use a session with people who should not see repository
paths.

Clicking a pane focuses it locally without taking control or sending input. When p2pmux runs
inside Zellij, Zellij may swallow mouse events; try Zellij with mouse mode disabled or a locked
passthrough configuration.

Drag inside a pane's terminal content to select text; releasing the mouse copies it to the macOS
clipboard.

- `Option+` `<shift>` + arrows — move focus to the nearest pane in that direction in the current
  tab. Some terminals need Shift with Option for horizontal arrows.
- `Ctrl+P`, then `n` — split the focused pane using its current aspect-ratio axis. The new
  fixed-grid PTY runs on the requester’s Mac.
- `Ctrl+P`, then `r` / `l` / `d` / `u` — create the new pane right / left / down / up of the
  focused pane.
- `Ctrl+P`, then `X` — delete the focused pane. Only that pane’s host may delete it.
- `Ctrl+P`, then arrows — move focus.
- `Ctrl+T`, then `N` — create a tab with a local PTY on the requester’s Mac.
- `Ctrl+T`, then `X` — delete the current tab only when the requester hosts every pane in it.
- `Ctrl+T`, then left/right — switch tabs.

The final pane in a tab must be removed by deleting its tab; the final tab cannot be deleted.
Nested 50/50 splits are part of Spike 3 (depth 4, at most 8 panes per tab, at most 9 tabs). Each
pane title shows `Pane #N host: <name> control: free|<name>|…`; free focused panes use a white
border and actively controlled panes use red-orange. Click tab labels to switch tabs without
claiming control or sending input. Mouse wheel scrolls pane history locally. The dark contextual footer uses red key accents: normal mode is
`Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS    type to claim when free`; pane mode is
`Pane  <←↓↑→> FOCUS   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <Esc> BACK`; tab mode is
`Tab  <←→> SWITCH   <n> NEW   <x> CLOSE   <Esc> BACK`.

Slow viewers may receive coalesced screen deltas and then a fresh snapshot to recover. Resizing an
outer terminal crops or letterboxes the immutable host grid; it never resizes the host PTY.

Full product/architecture docs live in [`docs/`](./docs/).

| Doc | Description |
|-----|-------------|
| [docs/PRODUCT.md](./docs/PRODUCT.md) | Vision, is/isn’t, why it matters |
| [docs/MVP_DESIGN.md](./docs/MVP_DESIGN.md) | **Locked** MVP design (source of truth) |
| [docs/SPIKE_PLAN.md](./docs/SPIKE_PLAN.md) | Build order / spikes |

## Quick links

- Notion (design workspace): tis & pelazas
- Platform target: macOS only for v1
- Stack direction: Rust, ratatui, portable-pty, vt100, Iroh, prost

## License

This project is licensed under the [MIT License](LICENSE).
