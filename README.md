# p2pmux

[![CI](https://img.shields.io/github/actions/workflow/status/pelazas/p2pmux/ci.yml?branch=main&label=ci)](https://github.com/pelazas/p2pmux/actions)
[![Release](https://img.shields.io/github/v/release/pelazas/p2pmux)](https://github.com/pelazas/p2pmux/releases)
[![License: MIT](https://img.shields.io/github/license/pelazas/p2pmux)](LICENSE)

A terminal multiplexer for two or more people, where **every pane runs on the machine of whoever
opened it**. Two developers, two AI subscriptions, one terminal — and neither holds the other's
keys.

**A shared control surface, not a shared computer.** Your processes and credential files never
leave your Mac. But whoever holds a pane has a real shell on the machine hosting it — both halves
are true at once, and [the trust model](#trust) says exactly what that means.

<!-- ─────────────────────────────────────────────────────────────────────────────
     DEMO GIF GOES HERE.

     1. Save the file as docs/assets/demo.gif
     2. Delete this comment block and uncomment the line below it.

     Shoot the 0:00–0:06 cold open from docs/LAUNCH_KIT.md §4: your terminal grid,
     a second cursor that is not yours appears in a pane and starts typing. No
     caption before 0:03, no intro. Loop it, keep it under ~6s and ~5MB.
     ───────────────────────────────────────────────────────────────────────── -->
<!-- ![A teammate's cursor typing into a pane hosted on my Mac](docs/assets/demo.gif) -->

```sh
brew tap pelazas/tap && brew trust pelazas/tap && brew install p2pmux

p2pmux create        # then Ctrl+S for the join code
p2pmux join 4KP7Q-M2XRW
```

Your teammate types that code on their own Mac, on their own network, and lands in your grid.

## Why it exists

Every way two developers share a terminal today collapses onto one box with one set of
credentials. SSH, tmux, screen sharing, cloud dev environments — somebody's machine runs
everything, and somebody's keys pay for it.

p2pmux keeps the machines separate. You bring your Mac and your AI subscription, your teammate
brings theirs, and you work in one grid. Your teammate can start Claude Code in a pane on your
Mac, on your subscription, and never hold your API key. It goes both ways at the same time: their
panes run on their hardware, and you can drive those without holding anything of theirs.

**That is all it does.** It is not a cloud VM, not a remote box everyone's processes run on, and
not an agent orchestration platform.

## What you get

- Every pane is a PTY on its owner's Mac — shell, Docker, an agent — with that machine's PATH,
  env, and subscriptions. Host and guest are **per pane**, not per person.
- Take control of any free pane by typing into it, or hand yours over. Active typing is protected,
  so there is no forced takeover.
- Zellij-like tabs, panes, and nested splits, shared live: up to 8 members, 9 tabs, 8 panes a tab.
- Presence — a color per member, showing who is on which tab and watching which pane.
- End-to-end encrypted peer-to-peer streaming, over an iroh relay when NAT requires it. The tab bar
  says which you got: `direct 55ms` or `relayed 120ms`.
- One ten-character join code that expires in 6 hours, or a ticket that contacts no service at all.
- An agents overlay (`Ctrl+A`) tracking Claude Code, Codex, Cursor, Pi and OpenCode across every
  machine in the session — including which ones are blocked waiting on a human.

## Status

**Early, but real.** v0.1.0 runs sessions between machines on different networks and different
continents. macOS only. Coordinator failover and disconnect grace are not built yet, so a
coordinator that dies ends the session.

## Install

```sh
brew tap pelazas/tap
brew trust pelazas/tap
brew install p2pmux
```

Tapping once is what buys the short name. `brew trust` is Homebrew 6 and newer: it refuses to load
a formula from a third-party tap until you say you trust that tap, so without it the install stops
with `Refusing to load formula … from untrusted tap`. On Homebrew 5 the command does not exist and
is not needed — skip that line.

Or, without Homebrew:

```sh
curl -fsSL https://p2pmux.com/install.sh | sh
```

Both fetch a binary and its SHA256 from GitHub Releases and check the hash before installing. The
script is served as plain text so you can read it first, and
`cargo install --git https://github.com/pelazas/p2pmux --locked` is a supported path for anyone
who would rather not run an installer at all.

## Trust

A p2pmux session is a **fully trusted shared shell**. Anyone with the join code or ticket can see
every pane and can obtain interactive control of the terminals in it — run commands, read output,
touch any file that macOS user can touch.

Processes and credential *files* stay on the pane host's Mac and are never uploaded to peers. That
is a real boundary, and it is the point of the design. It is **not** a sandbox: a controller can
still use your credentials, or print one to the screen, through the shell you handed them.

So: treat the code like a password, and share it only with people you would hand an unlocked
laptop to. For anyone else, use a separate low-privilege macOS account and keep production
credentials out of shared panes.

## Using it

`Ctrl+S` shares, `Ctrl+P` is pane mode, `Ctrl+T` is tab mode, `Ctrl+A` opens the agents overlay,
and `Ctrl+Q` detaches your view while the session keeps running. `p2pmux --resume` brings it back.

Full walkthrough — keys, control leases, presence, mouse, scrollback, agent hooks and every config
key — is in [docs/USAGE.md](./docs/USAGE.md).

## Docs

| Doc | Description |
|-----|-------------|
| [docs/USAGE.md](./docs/USAGE.md) | Everything you can do once it is running |
| [docs/PRODUCT.md](./docs/PRODUCT.md) | Vision, is/isn't, why it matters |
| [docs/MVP_DESIGN.md](./docs/MVP_DESIGN.md) | **Locked** MVP design (source of truth) |
| [docs/SPIKE_PLAN.md](./docs/SPIKE_PLAN.md) | Build order / spikes |

Built in Rust with ratatui, portable-pty, vt100, and iroh. macOS only for v1.

Found a rough edge? Please open an issue — a specific report of what broke is the most useful
thing anyone can send right now.

## License

[MIT](LICENSE).
