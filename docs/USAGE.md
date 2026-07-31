# Using p2pmux

Everything the README does not need to say on first read. Start with
[the README](../README.md) if you have not installed p2pmux yet.

## Sessions

`create` and `join` start a session-scoped background node, then attach the local TUI client.
Ctrl+Q detaches that client without stopping shells, Iroh, or hosted panes. It prints the exact
`--resume`, `attach`, and `kill` commands needed to return. Use `p2pmux --resume` for the
live-session picker, `p2pmux attach <name>` to attach directly, `p2pmux rename <old> <new>` to
rename a session, and `p2pmux kill <name>` to shut it down gracefully. Killing a coordinator asks
for confirmation; use `--yes` for non-interactive scripts.

Every `create` automatically receives a memorable world-city session name such as `tokyo` or
`cape-town`; no session name is required. `create --session-name <name>` remains available when
you want to choose a name explicitly. A joining peer uses the coordinator's session name for its
local chrome and finder record when that name is free locally; on the same Mac it adds `-2`, `-3`,
and so on to avoid a collision. Automatically chosen create names avoid live local session names
the same way.

There is one local client per session: a second attach is refused rather than taking over. The
finder descriptor is the only durable session data, under `~/Library/Application Support/p2pmux/`;
the Unix socket is under `/tmp/p2pmux-$UID/`. Screens, PTYs, tickets, layout state, and focus are
never restored from disk. The node survives terminal closure but is not managed by launchd. Local
IPC is intentionally versioned as an implementation detail, so restart an existing session after
upgrading when attach protocol changes are present.

To dogfood the shared layout on one Mac:

```text
Terminal 1: cargo run -- create
Terminal 2: cargo run -- join <the code or ticket from Ctrl+S>
```

`cargo run -- local` starts one local shell with no session at all. Press Ctrl+Q to leave p2pmux.
Its PTY grid is fixed from the terminal size at startup: larger windows leave extra cells blank
and smaller windows crop the upper-left fixed viewport.

## Inviting someone

`Ctrl+S` opens the share panel. It shows two things, and `join` takes either:

```text
p2pmux join 4KP7Q-M2XRW      # the code: short, expires in 6 hours
p2pmux join p2pmux-v3:...    # the ticket: long, never expires, needs no service
```

Both are also available from a shell, printed to stdout alone so `p2pmux code | pbcopy` gives you
something directly pasteable:

```text
p2pmux code              # the one session hosted on this Mac
p2pmux ticket <name>     # when several are; the name is the one p2pmux ls shows
```

Treat either as a password. Both grant full shared-shell access to the session, to anyone holding
them, for as long as the session lives. Only a coordinator holds them; a guest is told so.

The code is the ticket, stored where a peer can fetch it. Your machine derives two independent
values from the code: an index to store the record at, and a key to seal it with. Only the index
reaches `rv.p2pmux.com`, so the service holds an opaque handle and a sealed blob and has nothing
that would let it join. Terminal traffic never goes near it — that is peer to peer, or over an
iroh relay when NAT requires it. If the service is unreachable when a session starts, the panel
says so and the ticket still works; `p2pmux join <ticket>` never contacts it at all.

Tickets are emitted as `p2pmux-v3:`, which carries 32 independent random bytes as the join
credential. `join` still parses `p2pmux-v1:` and `p2pmux-v2:` tickets, in which the credential
*was* the coordinator's endpoint public key — a value published to discovery, so those tickets
grant a session no secret ever protected.

## Control

Only one peer controls a pane while they are actively typing. After about thirty seconds without
activity, the host clears the controller and the pane becomes free. The next member's ordinary key
claims the free pane and is delivered as its first input; active typing is protected, so there is
no forced takeover. A pane's host owns its PTY, not its control lease: newly created split and tab
panes start free. Ctrl+Q detaches only the local p2pmux view and releases its local control
leases; F9 and F10 continue through to the focused PTY.

Shared-layout commands are sticky local mux modes and never reach a PTY. `Ctrl+P` or `Ctrl+T`
enters its mode; use the listed command repeatedly, press `Esc` to cancel, or type any normal key
to leave the mode and send that key to the focused PTY.

## Keys

- `Option+` `<shift>` + arrows — move focus to the nearest pane in that direction in the current
  tab. Some terminals need Shift with Option for horizontal arrows.
- `Ctrl+P`, then `n` — split the focused pane using its current aspect-ratio axis. The new PTY
  runs on the requester's Mac and inherits the target pane's cwd when that pane is hosted locally
  by the requester and its cwd is available; otherwise it starts in the p2pmux process cwd.
- `Ctrl+P`, then `r` / `l` / `d` / `u` — create the new pane right / left / down / up of the
  focused pane. Its local PTY inherits the target pane's cwd under the same conditions.
- `Ctrl+P`, then `X` — delete the focused pane. Only that pane's host may delete it.
- `Ctrl+P`, then `i` — copy this session's full join ticket to the clipboard, for inviting someone
  on another Mac. Coordinator only.
- `Ctrl+P`, then `k` — lock the focused pane. A locked pane accepts input only from its own host,
  and its header reads `locked by <name>` for everyone else.
- `Ctrl+P`, then `Shift+L` — lock the whole session. The coordinator then refuses any peer that
  has never joined it, telling them the session is locked rather than just dropping them. Peers
  already inside are untouched, and one that reconnects after a drop is still let back in. The tab
  bar reads `locked` while it holds. Coordinator only; a guest is told so.
- `Ctrl+P`, then `e` — rename the focused pane for every admitted member. Enter saves; Esc
  cancels; a blank title restores `Pane #N`.
- `Ctrl+P`, then arrows — move focus.
- `Ctrl+T`, then `N` — create a tab with a local PTY on the requester's Mac.
- `Ctrl+T`, then `X` — delete the current tab only when the requester hosts every pane in it.
- `Ctrl+T`, then `e` — rename the current tab for every admitted member. Enter saves; Esc cancels;
  a blank title restores `Tab #N`.
- `Ctrl+T`, then left/right — switch tabs.

The final pane in a tab must be removed by deleting its tab; the final tab cannot be deleted. When
a pane shell exits, p2pmux preserves its final screen and marks the pane exited for everyone.
Exited panes accept no input or control claims; only the pane host can close one with `Ctrl+P`,
then `X`.

## Chrome

The tab bar's right edge reports the session's connectivity: `direct 55ms` when traffic is
peer-to-peer, `relayed 120ms` when it is going through a relay server, and `×N` when more than one
peer is connected — the number shown is always the worst path, since one peer stuck on a relay is
the thing worth noticing. `locked · direct 55ms` when the session is locked.

Nested ratio-controlled splits go four deep, at most 8 panes per tab and at most 9 tabs. Each pane
title shows `Pane #N host: <name> control: free|<name>|…`; free focused panes use a white border
and actively controlled panes use red-orange. Pane mode gives the locally focused pane a
soft-green border; Tab mode dims inactive tab labels. Click tab labels to switch tabs without
claiming control or sending input.

The dark contextual footer uses red key accents: normal mode is
`Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS    type to claim when free`;
pane mode is
`PANE MODE  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <k> LOCK   <L> LOCK SESSION   <i> INVITE   <Esc> BACK`;
tab mode is `TAB MODE  <←→> SWITCH   <e> RENAME   <n> NEW   <x> CLOSE   <Esc> BACK`.

## Presence

Presence shows where every other member is looking. Each member gets a color from their slot in
the session's member list, and that color identifies them everywhere: a dot per member on each tab
they are on, their initial on the bottom border of the pane they are watching, and a members block
in the agents overlay listing everyone with their tab and pane. Watching is not controlling — the
pane border keeps meaning focus and control state, and the member holding the control lease is
drawn as a reversed chip.

Presence is silent when nobody moves. A member publishes their focus only when it changes, the
coordinator caches the latest one per member and replays it to joiners, and there is no heartbeat
or timer in the path. Detached members report no location rather than leaving a marker behind.

## Mouse

Clicking a pane focuses it locally without taking control or sending input. When p2pmux runs
inside Zellij, Zellij may swallow mouse events; try Zellij with mouse mode disabled or a locked
passthrough configuration.

Drag inside a pane's terminal content to select text; releasing the mouse copies it to the macOS
clipboard. Drag a shared pane border to resize its split. Corner drags lock to one axis after a
short motion threshold, preview locally, and commit one shared ratio on release. The affected pane
hosts then resize their own PTY and VT screen and publish their local grids.

When the focused pane's program turns on xterm mouse reporting — an editor, a pager, or a coding
agent's input box — clicks, drags, and the wheel go to that program instead, so clicking moves its
text cursor. Hold `Shift` to select and copy as usual. The mux keeps every event such a program
should not see: presses on borders, tab labels, and the footer, presses on a pane that is not yet
focused (which focus it instead), and anything over a pane scrolled into history. A plain shell
prompt does not report mouse, so nothing changes there.

## Scrollback

For locally hosted panes, mouse-wheel scrollback is loaded from the host on demand when you first
scroll up, then cached while you browse it. Starting a scroll freezes a host-authored viewport for
that browse session, so output can continue at the live edge without changing what is being read;
resize, alternate-screen changes, and reconnects discard that frozen history.

Alternate-screen applications have no attach scrollback, and a resize establishes the existing
history floor (so pre-attach history can be empty after the attach-triggered resize). Remote-hosted
panes return an empty scrollback window in this v1 implementation.

Slow viewers may receive coalesced screen deltas and then a fresh snapshot to recover. Resizing an
outer terminal reflows locally hosted panes: the host resizes its PTY and VT screen and commits any
changed host-owned grids. Guests only receive the resulting commit and screen snapshot.

## Agents overlay

`Ctrl+A` opens the Agents overlay, which lists supported coding agents running below hosted pane
shells: Claude Code (`claude`), Codex (`codex`), Cursor Agent (including its `agent`/Node argv),
Pi (including Node-based launches), and OpenCode (`opencode`). Each agent is a two-line card: the
first line shows its kind and best-effort working directory; the second shows its state, chrome
location (`Tab #N · Pane #M`), host, and control holder. Working cards have an animated spinner
and live duration; `○ idle`, `✓ done`, `◆ needs you`, and `✗ error` use distinct visuals. The panel
title counts the agents blocked on a human (`Agents · 2 need you`).

Press `Esc` to close, use arrows or `j`/`k` to select, and press Enter or left-click a card to jump
to that pane (including on another tab). Scroll the overlay with the mouse wheel while it is open.
To retain readline's beginning-of-line shortcut, press `Ctrl+A` twice within 200ms to forward one
Ctrl+A to the focused PTY instead.

Working directories are shared with every member as part of the existing trusted shared-shell
model, so do not use a session with people who should not see repository paths.

`idle`, `working`, and `done` are inferred from PTY output timing, so they need no setup and work
for every agent above. `needs you` and `error` cannot be inferred at all — silence looks identical
whether an agent is thinking or waiting on a permission prompt — so they only appear for an agent
that reports its own state through a hook.

### Wiring up Claude Code

Add this to `~/.claude/settings.json`. Each hook pipes its payload to `p2pmux notify`, which writes
one line to the pane's session and exits. Outside a p2pmux pane it is a silent no-op, so it is safe
to leave registered everywhere.

```json
{
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "p2pmux notify claude --status running", "timeout": 5 }] }],
    "PreToolUse":       [{ "matcher": "*", "hooks": [{ "type": "command", "command": "p2pmux notify claude --status running", "timeout": 5 }] }],
    "PostToolUse":      [{ "matcher": "*", "hooks": [{ "type": "command", "command": "p2pmux notify claude --status running", "timeout": 5 }] }],
    "Notification":     [{ "hooks": [{ "type": "command", "command": "p2pmux notify claude --status pending", "timeout": 5 }] }],
    "Stop":             [{ "hooks": [{ "type": "command", "command": "p2pmux notify claude --status done", "timeout": 5 }] }],
    "SessionEnd":       [{ "hooks": [{ "type": "command", "command": "p2pmux notify claude --status idle", "timeout": 5 }] }]
  }
}
```

A turn that ends by asking a question reports `needs you` rather than `done`, since a green card on
a turn that is actually waiting reads as safe to ignore. A hook only ever reports for the pane it
runs in, on the machine it runs on; the node refuses a pane it does not host itself. The agent's
messages and your prompts are read to decide the status but never leave the machine — only the
state, the agent kind, and the working directory are shared with the session. A pushed state is
dropped 20 seconds after its pane returns to a shell prompt, so an agent killed mid-turn stops
asking for attention.

Hooks also make the mux cheaper: the process scan that infers state drops from every second to
every five when no pane needs inference.

## Configuration

Set a peer-visible display name once with `p2pmux config set name pelazas`; inspect it with
`p2pmux config get name`. It is stored in `$XDG_CONFIG_HOME/p2pmux/config.toml` (or
`~/.config/p2pmux/config.toml`). `create` and `join` accept `--name <name>` to override and save
the value for that run and future sessions.

Run `p2pmux config init` to create a fully commented configuration template; it refuses to
overwrite an existing file.

### Theme

Local TUI chrome colors live under `[ui.theme]`. Every theme key is optional, and omitted keys
retain today's built-in colors; the template documents every key and its default. Colors accept
lowercase named values (`white`, `yellow`, `gray`, `dark_gray`) or case-insensitive `#RRGGBB`
values. The theme is client-local and is never synchronized over P2P; detach and reattach after
editing the file to reload it.

`member_colors` under `[ui.theme]` is a list of up to eight colors, one per member slot in join
order, used for the presence dots and chips. Listing fewer than eight overrides from the front and
leaves the rest at their built-in color. The defaults are cool hues on purpose: warm colors are
reserved for the active tab and for a pane under remote control, so a member tinted like either
would read as an alert.

### Notifications

Agent-completion notifications live under `[ui.notifications]`. `sound_enabled = false` keeps the
local unread stars while silencing sound. By default p2pmux plays
`/System/Library/Sounds/Tink.aiff`; set the optional `sound_path` to any local sound file. These
settings are client-local and load when the client attaches.

An agent counts as finished when it rings the terminal bell, or failing that after `quiet_seconds`
of silence (default 20, clamped to 5-3600). The bell is by far the better signal — silence cannot
distinguish an agent that finished from one waiting on a model response — so configure your agent
to ring when it completes if it supports that. Set `require_bell = true` to notify *only* on the
bell; that removes every false notification, at the cost of showing an agent that never rings as
working until it exits. Each pane is announced once per work episode, so revisiting a pane does not
replay its notification.

## Performance logging

Set `P2PMUX_PERF=1` to write optional local performance timing logs. By default they append to
`std::env::temp_dir()/p2pmux-perf.log`; set `P2PMUX_PERF_LOG` to override that path. Logging is
best-effort and remains silent if the destination cannot be opened.
