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
revisioned tab/split tree, including authoritative split ratios. Every pane is a PTY owned by
the member that created it, and its host publishes the absolute grid for that host's window.
Each process serves its own locally hosted panes directly to the other admitted members.

This is still a dogfooding spike: relay/internet validation, disconnect grace, coordinator
failover and presence remain later work. Shared ratios are portable while absolute pane grids are
host-owned: guests consume the host's resized screen stream and never resize a remote PTY.

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

Run `p2pmux config init` to create a fully commented configuration template; it refuses to
overwrite an existing file. Local TUI chrome colors live under `[ui.theme]`. Every theme key is
optional, and omitted keys retain today's built-in colors; the template documents every key and
its default. Colors accept lowercase named values (`white`, `yellow`, `gray`, `dark_gray`) or
case-insensitive `#RRGGBB` values. The theme is client-local and is never synchronized over P2P;
detach and reattach after editing the file to reload it.

Agent-completion notifications live under `[ui.notifications]`. `sound_enabled = false` keeps
the local unread stars while silencing sound. By default p2pmux plays
`/System/Library/Sounds/Tink.aiff`; set the optional `sound_path` to any local sound file. These
settings are client-local and load when the client attaches.

An agent counts as finished when it rings the terminal bell, or failing that after
`quiet_seconds` of silence (default 20, clamped to 5-3600). The bell is by far the better
signal — silence cannot distinguish an agent that finished from one waiting on a model
response — so configure your agent to ring when it completes if it supports that. Set
`require_bell = true` to notify *only* on the bell; that removes every false notification, at
the cost of showing an agent that never rings as working until it exits. Each pane is announced
once per work episode, so revisiting a pane does not replay its notification.

Every `create` automatically receives a memorable world-city session name such as `tokyo` or
`cape-town`; no session name is required. `create --session-name <name>` remains available when
you want to choose a name explicitly. A joining peer uses the coordinator's session name for its
local chrome and finder record when that name is free locally; on the same Mac it adds `-2`, `-3`,
and so on to avoid a collision. Automatically chosen create names avoid live local session names
the same way.

`create` and `join` start a session-scoped background node, then attach the local TUI client.
Ctrl+Q detaches that client without stopping shells, Iroh, rendezvous, or hosted panes. It prints
the exact `--resume`, `attach`, and `kill` commands needed to return. Use `p2pmux --resume` for
the live-session picker, `p2pmux attach <name>` to attach directly, `p2pmux rename <old> <new>`
to rename a session, and `p2pmux kill <name>` to shut it down gracefully. Killing a coordinator
asks for confirmation; use `--yes` for non-interactive scripts.

There is one local client per session: a second attach is refused rather than taking over. The
finder descriptor is the only durable session data, under `~/Library/Application Support/p2pmux/`;
the Unix socket is under `/tmp/p2pmux-$UID/`. Screens, PTYs, tickets, layout state, and focus are
never restored from disk. The node survives terminal closure but is not managed by launchd.
Local scrollback is available only for locally hosted, non-alternate-screen panes. Starting a
scroll freezes a host-authored viewport for that browse session, so output can continue at the
live edge without changing what is being read; resize, alternate-screen changes, and reconnects
discard that frozen history. Local IPC is intentionally versioned as an implementation detail, so
restart an existing session after upgrading when attach protocol changes are present.
Short join codes resolve through a restrictive local cache on the same Mac, so they are for current
dogfooding only; they work while the corresponding `create` process is alive and are removed when
it exits. A peer on another machine cannot resolve one.

To invite someone on another Mac, use the full `p2pmux-v1:` ticket, which `join` also accepts:

```text
p2pmux ticket            # the one session live on this Mac
p2pmux ticket <code>     # when several are live; the code is in the session footer
```

From inside a session, `Ctrl+P`, then `i` copies the same ticket straight to the clipboard and
flashes `copied full ticket` in the footer. That overlay text is local chrome, so unlike running
the command in a shared pane it is not visible to the other members. Only a coordinator has a
ticket to copy; a guest is told so.

The ticket is printed to stdout on its own, so `p2pmux ticket | pbcopy` gives you something
directly pasteable. Treat it as a password: it grants full shared-shell access to the session, to
anyone holding it, for as long as the session lives. There is no hosted rendezvous yet, so this is
the portable invite until one exists.

Only one peer controls a pane while they are actively typing. After about thirty seconds without
activity, the host clears the controller and the pane becomes free. The next member's ordinary key
claims the free pane and is delivered as its first input; active typing is protected, so there is
no forced takeover. A pane's host owns its PTY, not its control lease: newly created split and tab
panes start free. Ctrl+Q detaches only the local p2pmux view and releases its local control
leases; F9 and F10 continue through to the focused PTY.

Shared-layout commands are sticky local mux modes and never reach a PTY. `Ctrl+P` or `Ctrl+T`
enters its mode; use the listed command repeatedly, press `Esc` to cancel, or type any normal key
to leave the mode and send that key to the focused PTY.

`Ctrl+A` opens the Agents overlay, which lists supported coding agents running below hosted pane
shells: Claude Code (`claude`), Codex (`codex`), Cursor Agent (including its `agent`/Node argv),
Pi (including Node-based launches), and OpenCode (`opencode`). Each agent is a two-line card: the
first line shows its kind and best-effort working directory; the second shows its state, chrome
location (`Tab #N · Pane #M`), host, and control holder. Working cards have an animated spinner
and live duration, while idle and done cards use distinct `○` and `✓` visuals. Press `Esc` to
close, use arrows or `j`/`k` to select, and press Enter or left-click a card to jump to that pane
(including on another tab). Scroll the overlay with the mouse wheel while it is open. To retain
readline's beginning-of-line shortcut, press `Ctrl+A` twice within 200ms to forward one Ctrl+A to
the focused PTY instead. Working directories are shared with every member as part of the existing
trusted shared-shell model, so do not use a session with people who should not see repository
paths.

Clicking a pane focuses it locally without taking control or sending input. When p2pmux runs
inside Zellij, Zellij may swallow mouse events; try Zellij with mouse mode disabled or a locked
passthrough configuration.

Drag inside a pane's terminal content to select text; releasing the mouse copies it to the macOS
clipboard. Clicking the footer join command copies `p2pmux join <code>` to the clipboard. Drag a
shared pane border to resize its split. Corner drags lock to one axis after a short motion
threshold, preview locally, and commit one shared ratio on release. The affected pane hosts then
resize their own PTY and VT screen and publish their local grids.

When the focused pane's program turns on xterm mouse reporting — an editor, a pager, or a coding
agent's input box — clicks, drags, and the wheel go to that program instead, so clicking moves its
text cursor. Hold `Shift` to select and copy as usual. The mux keeps every event such a program
should not see: presses on borders, tab labels, and the footer, presses on a pane that is not yet
focused (which focus it instead), and anything over a pane scrolled into history. A plain shell
prompt does not report mouse, so nothing changes there.

- `Option+` `<shift>` + arrows — move focus to the nearest pane in that direction in the current
  tab. Some terminals need Shift with Option for horizontal arrows.
- `Ctrl+P`, then `n` — split the focused pane using its current aspect-ratio axis. The new PTY
  runs on the requester’s Mac and inherits the target pane’s cwd when that pane is hosted locally
  by the requester and its cwd is available; otherwise it starts in the p2pmux process cwd.
- `Ctrl+P`, then `r` / `l` / `d` / `u` — create the new pane right / left / down / up of the
  focused pane. Its local PTY inherits the target pane’s cwd when that pane is hosted locally by
  the requester and its cwd is available; otherwise it starts in the p2pmux process cwd.
- `Ctrl+P`, then `X` — delete the focused pane. Only that pane’s host may delete it.
- `Ctrl+P`, then `i` — copy this session’s full join ticket to the clipboard, for inviting
  someone on another Mac. Coordinator only.
- `Ctrl+P`, then `e` — rename the focused pane for every admitted member. Enter saves; Esc
  cancels; a blank title restores `Pane #N`.
- `Ctrl+P`, then arrows — move focus.
- `Ctrl+T`, then `N` — create a tab with a local PTY on the requester’s Mac.
- `Ctrl+T`, then `X` — delete the current tab only when the requester hosts every pane in it.
- `Ctrl+T`, then `e` — rename the current tab for every admitted member. Enter saves; Esc
  cancels; a blank title restores `Tab #N`.
- `Ctrl+T`, then left/right — switch tabs.

The final pane in a tab must be removed by deleting its tab; the final tab cannot be deleted.
When a pane shell exits, p2pmux preserves its final screen and marks the pane exited for everyone.
Exited panes accept no input or control claims; only the pane host can close one with `Ctrl+P`, then
`X`.
Nested ratio-controlled splits are part of Spike 3 (depth 4, at most 8 panes per tab, at most 9 tabs). Each
pane title shows `Pane #N host: <name> control: free|<name>|…`; free focused panes use a white
border and actively controlled panes use red-orange. Pane mode gives the locally focused pane a
soft-green border; Tab mode dims inactive tab labels. Click tab labels to switch tabs without
claiming control or sending input. Mouse wheel scrolls pane history locally unless the focused
pane's program reports mouse, in which case the wheel reaches that program. The dark contextual footer uses red key accents: normal mode is
`Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS    type to claim when free`; pane mode is
`PANE MODE  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <k> LOCK   <i> INVITE   <Esc> BACK`; tab mode is
`TAB MODE  <←→> SWITCH   <e> RENAME   <n> NEW   <x> CLOSE   <Esc> BACK`.

For locally hosted panes, mouse-wheel scrollback is loaded from the host on demand when you first
scroll up, then cached while you browse it. Alternate-screen applications have no attach
scrollback, and a resize establishes the existing history floor (so pre-attach history can be
empty after the attach-triggered resize). Remote-hosted panes return an empty scrollback window in
this v1 implementation. This release changes the local IPC screen payload: restart any running
p2pmux sessions after upgrading before attaching a new client.

Slow viewers may receive coalesced screen deltas and then a fresh snapshot to recover. Resizing an
outer terminal reflows locally hosted panes: the host resizes its PTY and VT screen and commits any
changed host-owned grids. Guests only receive the resulting commit and screen snapshot.

### Performance logging

Set `P2PMUX_PERF=1` to write optional local performance timing logs. By default they append to
`std::env::temp_dir()/p2pmux-perf.log`; set `P2PMUX_PERF_LOG` to override that path. Logging is
best-effort and remains silent if the destination cannot be opened.

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
