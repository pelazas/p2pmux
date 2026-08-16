<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-wordmark-dark.svg">
  <img src="docs/assets/logo-wordmark-light.svg" alt="p2pmux" width="340">
</picture>

<p>
  <a href="https://github.com/pelazas/p2pmux/actions"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/pelazas/p2pmux/ci.yml?branch=main&label=ci"></a>
  <a href="https://github.com/pelazas/p2pmux/releases"><img alt="Release" src="https://img.shields.io/github/v/release/pelazas/p2pmux"></a>
  <a href="https://crates.io/crates/p2pmux"><img alt="crates.io" src="https://img.shields.io/crates/v/p2pmux?logo=rust"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/github/license/pelazas/p2pmux"></a>
</p>

<p>
  <a href="https://p2pmux.com">Website</a> ·
  <a href="#install">Install</a> ·
  <a href="./docs/USAGE.md">Usage</a> ·
  <a href="#trust">Trust model</a> ·
  <a href="./CONTRIBUTING.md">Contributing</a>
</p>

</div>

**Everyone brings their own terminal.** Every pane runs on the machine of whoever opened it — their
toolchain, their libraries, their env, their AI subscription. Hop into a teammate's pane and you get
a real shell on *their* machine, running *their* setup, without ever holding their keys.

<p align="center">
  <img src="docs/assets/demo.gif" width="820"
       alt="userB joins with a code, opens a pane on their own machine, then types into a pane hosted by userA">
</p>

<p align="center">
  <em>userB joins with the ten-character code, opens a pane of their own, then takes control of
  userA's pane and runs a command in it. Both windows are real clients; the pane titles say who
  hosts what.</em>
</p>

**A shared control surface, not a shared computer.** Your processes and credential files never
leave your machine. But whoever holds a pane has a real shell on the machine hosting it — both
halves are true at once, and [the trust model](#trust) says exactly what that means.

```sh
curl -fsSL https://p2pmux.com/install.sh | sh

p2pmux                       # you host; Ctrl+S shows the line to send
p2pmux join 4KP7Q-M2XRW      # them, on their own machine
```

`Ctrl+S` gives you that second line, ready to paste into a chat window. Your teammate runs it on
their own machine, on their own network, and lands in your grid. Ten characters, nothing else to
set up on either side. A Mac and a Linux box work the same session; nobody has to match anybody.

## Why it exists

Every other way to share a terminal collapses onto one box. SSH, tmux, tmate, screen sharing, cloud
dev environments — one machine runs everything, one person's toolchain is the only one in the room,
and one person's keys pay for it. Whoever joins arrives empty-handed.

p2pmux keeps the machines separate and joins the surface instead. You bring your laptop, your
dotfiles, your language versions and your Claude subscription. Your teammate brings theirs. Both
setups are live in the same grid at once, and either of you can reach into the other's.

Concretely: your teammate opens a pane on your machine and runs Claude Code on your subscription —
they drive it, you pay for it, they never see the key. The pane beside it is on their box, with
their Python env and their GPU, and you drive that one without installing a thing.

**That is all it does.** It is not a cloud VM, not a remote box everyone's processes run on, and
not an agent orchestration platform.

## What you get

- Every pane is a PTY on its owner's machine — shell, Docker, an agent — with that machine's PATH,
  env, and subscriptions. Host and guest are **per pane**, not per person.
- Take control of any free pane by typing into it, or hand yours over. Active typing is protected,
  so there is no forced takeover.
- Zellij-like tabs, panes, and nested splits, shared live: up to 8 members, 9 tabs, 8 panes a tab.
- Presence — a color per member, showing who is on which tab and watching which pane.
- The session outlives the machine that started it. If the coordinator's laptop closes, every pane
  on every other machine keeps running and keeps taking input; only structural edits pause, and
  after five minutes the earliest-joined survivor takes the role over.
- End-to-end encrypted peer-to-peer streaming, over an iroh relay when NAT requires it. The tab bar
  says which you got: `direct 55ms` or `relayed 120ms`.
- One ten-character join code, good for 6 hours, and nothing else to exchange — backed by a ticket
  that contacts no service at all, for when our rendezvous is down.
- An inbox — the screen bare `p2pmux` opens — listing every Claude Code, Codex, Cursor, Pi and
  OpenCode agent running on every machine in the session, sorted by which one is blocking you.
  Press Enter on a row and you are typing in that terminal; `Ctrl+O` brings you back. `needs you`
  comes only from the agent's own hooks, never from guessing at output timing: run
  `p2pmux setup` once, and `p2pmux doctor` to check. An agent with no hooks says
  *state unknown — no hooks* on its own row rather than being guessed about.
- `p2pmux pair` associates two machines you own, once and permanently. After that, bare `p2pmux`
  rejoins on either with no code typed, and `p2pmux machines` says which of them are awake.
  For a machine with nobody sitting at it, `p2pmux enroll` prints a revocable token to paste into
  a provisioning script, and the VM joins your fleet unattended.
- What your machines may start on one of them is written on that machine: `p2pmux work allow`,
  matched in full, default closed. Being in your fleet grants nothing on its own.

<p align="center">
  <a href="docs/assets/workflow.mp4">
    <img src="docs/assets/workflow.png" width="820"
         alt="a laptop and two droplets in one session; an opencode agent running on each droplet, both listed in the laptop's inbox as needing a human">
  </a>
</p>

<p align="center">
  <em><a href="docs/assets/workflow.mp4">Ninety seconds, three machines</a>: a MacBook and two
  DigitalOcean droplets share one session, an <code>opencode</code> agent is started from the
  laptop on each droplet, and the inbox says which of them is blocking a human. Recorded live by
  <a href="scripts/demo/record_workflow.py"><code>scripts/demo/record_workflow.py</code></a>.</em>
</p>

## Status

**Early, but real.** v0.1.9 runs sessions between machines on different networks and different
continents. macOS and Linux, both architectures. Bare `p2pmux` opens the inbox, two machines pair
once and rejoin without a code, and two agents report their own state through hooks — Claude Code
and OpenCode. A coordinator that dies no longer ends the session — a survivor takes the role over,
with a new join code — but a pane whose host is gone stays in the layout as a placeholder rather
than being reaped on a timer. Codex, Cursor and Pi are detected but have no hooks yet, so their
rows say so rather than guessing.

v0.1.6 is about the other machines. The inbox tells a machine you own from a person who joined,
and only ever offers to start work on the first kind. You can open a terminal on one of your
machines from the fleet list, subject to an allowlist that machine's owner writes on the machine
itself — commands matched in full, default closed, and no blocklist, because a blocklist on an
interactive shell is a guardrail against accidents and not a boundary. Your machines follow you
into sessions they were never paired into, kept there by a fleet agent under launchd or systemd.
Agents running *outside* p2pmux — a bot under systemd, something in a stray tmux — appear in the
inbox too, and pressing enter opens their own chat client on their own machine. Hermes and
OpenClaw are detected; the row says which of the two things enter does, because `openclaw chat`
joins the conversation its gateway is having and `hermes chat` starts a new one.

v0.1.7 finishes that thought. An agent running outside p2pmux — one you started in another
terminal, or a bot under systemd — now reports *what it is doing*, so the inbox shows it working,
blocked or done rather than listing a process it knows nothing about: its hooks leave a record on
the machine, and the scan that found the process reads it back. Dragging a selection past a pane's
top or bottom scrolls it, and keeps scrolling for as long as you hold it there, so what you copy is
no longer limited to what fits on screen. And when a newer release exists the inbox says so, naming
the one command that fits how your copy was installed.

v0.1.8 is about the fleet, and about the inbox telling the truth. A machine you pair while a
session is already open now stays in the fleet — it used to announce that it belonged to none for
as long as that p2pmux ran, so nothing ever wrote it down and its row vanished with the session.
One machine is one row, however many p2pmux it has run. `p2pmux enroll` puts a machine you own in
your fleet from a provisioning script, with a revocable token instead of a code somebody types
within ten minutes. `p2pmux work` is how a machine says what your other machines may start on it,
which until now could only be written by hand into a file most people never found — and a refusal
names the command that lifts it, on the machine to run it on. Agents in *another* p2pmux session on
the same machine are named as such rather than called "running outside p2pmux": the row is drawn
dim, the cursor and the pointer both pass over it, and it carries the command that reaches that
agent — `p2pmux attach <name>`, from a terminal, since a p2pmux nested in a pane of another one is
not a way in. The `inbox N` badge stops counting an agent once you have been to its pane, and never
counts one you cannot get to from here. `m` moves the cursor into the fleet and the arrow keys walk
it.

v0.1.9 is about the first command and the rows you cannot act on. Bare `p2pmux` now always ends in
a session: a session already serving a terminal is one to pass over rather than a failure to
report, so a second window on a machine that already has p2pmux open rejoins the paired session, or
creates one if this machine is on its own, instead of stopping at `Error: already attached`. When
the rejoin has to dial a machine that is asleep it says so before spending the thirty seconds, not
after. The listing is `p2pmux list`, with `ls` kept as an alias, and `p2pmux attach` takes the name
optionally. In the inbox, an agent in *another* p2pmux session on the same machine is drawn dim and
carries the command that reaches it: the cursor walks past it, a click on it opens nothing, and it
is left out of the `inbox N` badge — the badge counts summonses somebody can answer from here. The
elapsed clock now dates whichever state a row is in rather than only a working one, so `needs you`
says how long it has been waiting and an interrupted turn stops restarting its clock. An emoji
presentation glyph no longer eats the character beside it and shifts everything after it, Shift+Tab
reaches the pane instead of being swallowed, and the wheel is aimed by the pointer rather than by
focus. A refused local connection is no longer taken as proof the node is gone, one bad local
connection cannot end a session, and a node that dies says what happened.

**v0.1.8 and v0.1.9 share sessions.** The wire protocol did not move, so a peer on either can join
the other. v0.1.7 and older still cannot join either: that pin moved in v0.1.8 — a machine now
tells the session it has joined a fleet, and a joining machine can present an enrolment token —
and a peer on the wrong side of it is refused with an unsupported-protocol error rather than
half-joining.

## Install

```sh
curl -fsSL https://p2pmux.com/install.sh | sh
```

macOS and Linux, Apple Silicon, Intel and arm64. The script fetches a binary and its SHA256 from
GitHub Releases and checks the hash before installing. It is served as plain text so you can read
it first.

If you would rather not run an installer at all, it is [on crates.io](https://crates.io/crates/p2pmux):

```sh
cargo install p2pmux --locked
```

That path builds from source and needs **Rust 1.91 or newer** — iroh 1.0 sets the floor, and an
older toolchain refuses the lockfile rather than building something subtly different.

Linux builds link glibc 2.35 or newer — Ubuntu 22.04, Debian 12, Fedora 36 and up. A musl system
has to build from source.

Updating is however you installed:

```sh
brew update && brew upgrade p2pmux                                   # Homebrew tap
curl -fsSL https://p2pmux.com/install.sh | sh                        # the install script, again
cargo install p2pmux --locked --force                                # from source
```

`p2pmux --version` says what you are running. Sessions already running keep the binary they
started with — the node is a long-lived process — so `p2pmux kill <name>` and start again to pick
a new version up.

## Trust

A p2pmux session is a **fully trusted shared shell**. Anyone with the join code or ticket can see
every pane and can obtain interactive control of the terminals in it — run commands, read output,
touch any file that user account can touch.

Processes and credential *files* stay on the pane host's machine and are never uploaded to peers.
That is a real boundary, and it is the point of the design. It is **not** a sandbox: a controller
can still use your credentials, or print one to the screen, through the shell you handed them.

So: treat the code like a password, and share it only with people you would hand an unlocked
laptop to. For anyone else, use a separate low-privilege account and keep production credentials
out of shared panes.

## Using it

Run `p2pmux` with no arguments and you land on the inbox. `Ctrl+O` returns to it from anywhere,
including from inside a live terminal; `Ctrl+A` does the same, for the muscle memory the old
agents overlay built. `Ctrl+S` shares, `Ctrl+P` is pane mode, `Ctrl+T` is tab mode, and `Ctrl+Q`
asks whether to detach — leaving the session running — or to end it. `p2pmux --resume` brings back
a specific session by name.

Full walkthrough — keys, control leases, presence, mouse, scrollback, agent hooks and every config
key — is in [docs/USAGE.md](./docs/USAGE.md).

## Docs

| Doc | Description |
|-----|-------------|
| [docs/USAGE.md](./docs/USAGE.md) | Everything you can do once it is running |
| [docs/PRODUCT.md](./docs/PRODUCT.md) | Vision, is/isn't, why it matters |
| [docs/MVP_DESIGN.md](./docs/MVP_DESIGN.md) | **Locked** MVP design (source of truth) |
| [docs/SPIKE_PLAN.md](./docs/SPIKE_PLAN.md) | Build order / spikes |

Built in Rust with ratatui, portable-pty, vt100, and iroh. macOS and Linux.

Found a rough edge? Please open an issue — a specific report of what broke is the most useful
thing anyone can send right now.

## License

[MIT](LICENSE).
