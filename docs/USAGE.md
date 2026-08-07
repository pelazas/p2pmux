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

`p2pmux ls` prints the live sessions on this machine — the names every one of those commands takes:

```text
NAME       ROLE         CODE         UP
firenze    coordinator  7REWA-P3QEQ  12m
firenze-2  member       -            11m
```

`CODE` is `-` for a session you joined rather than created, and for a coordinator that started
while the rendezvous was unreachable and so has only a ticket. `p2pmux --version` reports the
build, which is the first thing to put in a bug report.

Every `create` automatically receives a memorable world-city session name such as `tokyo` or
`cape-town`; no session name is required. `create --session-name <name>` remains available when
you want to choose a name explicitly. A joining peer uses the coordinator's session name for its
local chrome and finder record when that name is free locally; on one machine it adds `-2`, `-3`,
and so on to avoid a collision. Automatically chosen create names avoid live local session names
the same way.

There is one local client per session: a second attach is refused rather than taking over. The
finder descriptor is the only durable session data, under `~/Library/Application Support/p2pmux/`
on macOS and `$XDG_STATE_HOME/p2pmux/` — `~/.local/state/p2pmux/` unless you set it — on Linux.
The Unix socket is under `$XDG_RUNTIME_DIR/p2pmux/` where Linux provides one, and `/tmp/p2pmux-$UID/`
otherwise, which is also where macOS keeps it. Screens, PTYs, tickets, layout state, and focus are
never restored from disk. The node survives terminal closure but is managed by neither launchd nor
systemd. Local IPC is intentionally versioned as an implementation detail, so restart an existing
session after upgrading when attach protocol changes are present.

To dogfood the shared layout on one machine:

```text
Terminal 1: cargo run -- create
Terminal 2: cargo run -- join <the code from Ctrl+S>
```

`cargo run -- local` starts one local shell with no session at all. Press Ctrl+Q to leave p2pmux.
Its PTY grid is fixed from the terminal size at startup: larger windows leave extra cells blank
and smaller windows crop the upper-left fixed viewport.

## Inviting someone

`Ctrl+S` shows the line your guest runs. Enter copies it; send it as-is:

```text
p2pmux join 4KP7Q-M2XRW
```

That is the whole invite. Ten characters, good for 6 hours, and it resolves from any machine on
any network — your guest needs nothing else, and nothing about your setup.

Treat it like a password: it grants full shared-shell access to the session, to anyone who runs
it, for as long as the session lives. Only a coordinator can invite; a guest is told so.

It is also available from a shell, printed to stdout alone so `p2pmux code | pbcopy` — or
`| wl-copy`, or `| xclip -selection clipboard` — gives you something directly pasteable:

```text
p2pmux code              # the one session hosted on this machine
p2pmux code <name>       # when several are; the name is the one p2pmux ls shows
```

### The ticket behind the code

The code is not a second credential. It *is* a ticket, stored where your guest can fetch it. Your
machine derives two independent values from the code: an index to store the record at, and a key
to seal it with. Only the index reaches `rv.p2pmux.com`, so the service holds an opaque handle and
a sealed blob and has nothing that would let it join. Terminal traffic never goes near it — that
is peer to peer, or over an iroh relay when NAT requires it.

You deal with the ticket directly in one case: the rendezvous being unreachable when your session
starts, which leaves no code to mint. The share panel says so and offers the ticket instead — some
170 characters, never expires, contacts no service at all. `t` copies it, `p2pmux ticket <name>`
prints it, and `p2pmux join <ticket>` works the same way from the other end. That fallback is why
an outage in a service we run cannot stop you sharing a session.

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
  runs on the requester's machine and inherits the target pane's cwd when that pane is hosted
  locally by the requester and its cwd is available; otherwise it starts in the p2pmux process cwd.
- `Ctrl+P`, then `r` / `l` / `d` / `u` — create the new pane right / left / down / up of the
  focused pane. Its local PTY inherits the target pane's cwd under the same conditions.
- `Ctrl+P`, then `X` — delete the focused pane. Only that pane's host may delete it.
- `Ctrl+P`, then `i` — copy this session's full join ticket to the clipboard, for inviting someone
  on another machine. Coordinator only.
- `Ctrl+P`, then `k` — lock the focused pane. A locked pane accepts input only from its own host,
  and its header reads `locked by <name>` for everyone else.
- `Ctrl+P`, then `Shift+L` — lock the whole session. The coordinator then refuses any peer that
  has never joined it, telling them the session is locked rather than just dropping them. Peers
  already inside are untouched, and one that reconnects after a drop is still let back in. The tab
  bar reads `locked` while it holds. Coordinator only; a guest is told so.
- `Ctrl+P`, then `e` — rename the focused pane for every admitted member. Enter saves; Esc
  cancels; a blank title restores `Pane #N`.
- `Ctrl+P`, then arrows — move focus.
- `Ctrl+T`, then `N` — create a tab with a local PTY on the requester's machine.
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
title shows `Pane #N host: <name> control: free|<name>|…`. A pane being driven by a member is
drawn in that member's own color — the same color as their tab dot — so the border says who
holds it, and every member sees that pane the same color. A free pane you are focused on is
white; red-orange is left for a controller who has since dropped out of the session. Pane mode
gives the locally focused pane a red border; Tab mode dims inactive tab labels. Click tab labels
to switch tabs without claiming control or sending input.

The dark contextual footer uses red key accents: normal mode is
`Ctrl+ <p> PANE   <t> TAB   <q> QUIT   Option+ <shift> + <↑↓←→> FOCUS    type to claim when free`;
pane mode is
`PANE MODE  <←↓↑→> FOCUS   <e> RENAME   <n> NEW   <r/l/d/u> SPLIT   <x> CLOSE   <k> LOCK   <L> LOCK SESSION   <i> INVITE   <Esc> BACK`;
tab mode is `TAB MODE  <←→> SWITCH   <e> RENAME   <n> NEW   <x> CLOSE   <Esc> BACK`.

## Presence

Presence shows where every other member is looking. Each member gets a color from their slot in
the session's member list, and that color identifies them everywhere: a dot per member on each tab
they are on, their initial on the bottom border of the pane they are watching, the border of any
pane they are driving, and a members block in the agents overlay listing everyone with their tab
and pane. Watching is not controlling — a watcher only ever leaves an initial on the bottom
border, and the member holding the control lease is drawn as a reversed chip.

Presence is silent when nobody moves. A member publishes their focus only when it changes, the
coordinator caches the latest one per member and replays it to joiners, and there is no heartbeat
or timer in the path. Detached members report no location rather than leaving a marker behind.

## Mouse

Clicking a pane focuses it locally without taking control or sending input. When p2pmux runs
inside Zellij, Zellij may swallow mouse events; try Zellij with mouse mode disabled or a locked
passthrough configuration.

Drag inside a pane's terminal content to select text; releasing the mouse copies it to the system
clipboard — `pbcopy` on macOS, and `wl-copy`, `xclip` or `xsel` on Linux, whichever is installed.
Over `ssh` or in a bare TTY, where none of those exist, p2pmux asks the terminal emulator itself
via OSC 52; terminals that do not implement it drop the request silently, so a copy can be
reported that did not land. Drag a shared pane border to resize its split. Corner drags lock to one axis after a
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

## What a pane's terminal answers

A pane is a real PTY, and programs interrogate the terminal behind one. A query is a blocking
round trip — the program writes it and reads until an answer arrives — so an unanswered query does
not degrade a program, it stops it. A pane answers:

| Query | Answer |
| --- | --- |
| `CSI 6 n` (cursor position) | `CSI row ; col R` |
| `CSI ? 6 n` (DECXCPR) | `CSI ? row ; col ; 1 R` |
| `CSI 5 n` (status) | `CSI 0 n` |
| `CSI 18 t` (text area) | `CSI 8 ; rows ; cols t` |
| `CSI c`, `CSI > c` (device attributes) | a VT100 with the advanced video option |

The answers come from the machine hosting the pane, so a pane on another member's laptop is
answered by their node, about their grid — which is the only correct answer.

That covers the common way a program measures its terminal: park the cursor far past the end with
`CSI 999 ; 999 f` and ask where it landed. p2pmux treats `f` (HVP) as the `H` (CUP) it is, so the
cursor really moves and the reported size is the pane's own.

**Colour queries are not answered.** `OSC 10` and `OSC 11` ask for the terminal's foreground and
background, and no p2pmux process can see the terminal you are looking at — a pane hosted
elsewhere has no single such terminal at all. An invented answer would make an application pick a
palette against the wrong background, so these stay silent. Programs that use them for light/dark
detection fall back to their default.

## Agents overlay

`Ctrl+A` opens the Agents overlay, which lists supported coding agents running below hosted pane
shells: Claude Code (`claude`), Codex (`codex`), Cursor Agent (including its `agent`/Node argv),
Pi (including Node-based launches), and OpenCode (`opencode`).

Each agent is one line: a state glyph, the agent, the tail of its working directory, its state (with
elapsed time while working or blocked), and what it last said it was doing. The selected row adds a
detail line underneath with its chrome location (`Tab #N · Pane #M`), host, and control holder. The
panel is sized to its contents, and its title counts the agents blocked on a human
(`Agents · 2 need you`).

```
┌ Agents · 1 needs you ──────────────────────────────────────────────────────┐
│›⠼ claude     Desktop/p2pmux        working 4s   refactoring the sync gate…  │
│ ◆ claude     Desktop/api           needs you    Should I force-push to main?│
│ ● claude     Desktop/dtc-landing   done         All 310 tests pass.         │
│ Tab #1 · Pane #1 · host: devmid · control: free                            │
│ <↑↓> MOVE   <Enter> FOCUS                                                  │
└────────────────────────────────────────────────────────────────────────────┘
```

Press `Esc` to close, use arrows or `j`/`k` to select, and press Enter or left-click a row to jump
to that pane (including on another tab). Scroll the overlay with the mouse wheel while it is open.
To retain readline's beginning-of-line shortcut, press `Ctrl+A` twice within 200ms to forward one
Ctrl+A to the focused PTY instead.

Working directories are shared with every member as part of the existing trusted shared-shell
model, so do not use a session with people who should not see repository paths. What the agent
*said* is not: that line reaches your own overlay and stops there, because a session is shared with
everyone holding the ticket. A row for a pane hosted by another member shows their agent's state
but never its words.

### Every state comes from a hook

p2pmux does not guess what an agent is doing. It used to infer `working` and `done` from how long a
pane had been quiet, and that could not work: silence looks identical whether an agent is thinking,
waiting on a permission prompt, or finished. The guess fired completions mid-task and could never
once report `needs you`, the state that actually costs you time.

So the process scan answers one question — *which* agent is running in a pane — and the agent's own
hooks answer the rest. Until a hook reports, a row reads `· not reporting`, which is the honest
answer, and the overlay says so:

```
no agent hooks wired — run: p2pmux setup
```

### Wiring up Claude Code

```
p2pmux setup claude          # write the hooks
p2pmux doctor                # check they are wired
p2pmux setup claude --uninstall
```

`setup` writes six marker-owned entries into `~/.claude/settings.json` — one per lifecycle event —
through a temporary file and a rename. Every entry it writes carries `"owner": "p2pmux"`, so
installing replaces exactly its own entries and removing takes exactly those: your own hooks on the
same events (a completion chime on `Stop`, say) survive both untouched. Running it twice is the same
as running it once. It refuses to rewrite a `settings.json` it cannot parse rather than clobber it,
and `--dry-run` says what it would do.

Each hook pipes its payload to `p2pmux notify`, which writes one line to the pane's session and
exits. Outside a p2pmux pane it is a silent no-op, so it is safe to leave registered everywhere.
Restart any running Claude Code sessions to pick the hooks up.

A turn that ends by asking a question reports `needs you` rather than `done`, since a green row on
a turn that is actually waiting reads as safe to ignore. A hook only ever reports for the pane it
runs in, on the machine it runs on; the node refuses a pane it does not host itself. Your prompts
and the tools being run are read to decide the status and never leave the process. A pushed state is
dropped 20 seconds after its pane returns to a shell prompt, so an agent killed mid-turn stops
asking for attention.

Dropping the inference also made the mux cheaper: the process scan no longer refreshes every
process on the machine once a second, only once every five.

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
leaves the rest at their built-in color. The first slot — the session host — defaults to a vivid
red-orange so the host is easy to pick out; the rest are cool hues on purpose, because the
remaining warm colors are reserved for the active tab, for a chord-armed pane, and for a pane whose
controller has left, so a member tinted like any of those would read as an alert.

### Notifications

A pane is marked unread — the `*` on its title and its tab — when its agent arrives at a state that
wants you: `done`, `needs you`, or `error`, reported by a hook, while you are looking somewhere
else. Focusing the pane clears the mark, and that does not re-arm it for work you have already seen.

There is no completion sound. p2pmux used to play one, driven by the output-timing inference that
could not tell a finished turn from a quiet one, so it fired mid-task; the `[ui.notifications]`
config block that tuned it is gone with it. A hook-reported completion is already unmissable in the
overlay and the unread mark, and an agent that should make a noise can do it from its own `Stop`
hook, where it knows it has actually finished.

## Performance logging

Set `P2PMUX_PERF=1` to write optional local performance timing logs. By default they append to
`std::env::temp_dir()/p2pmux-perf.log`; set `P2PMUX_PERF_LOG` to override that path. Logging is
best-effort and remains silent if the destination cannot be opened.
