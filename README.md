# Peer-to-peer multiplayer terminal multiplexer

> Name: to decide

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

Early design locked (2026-07-24). Spike 1 local terminal implementation is available; the
networked multiplexer is not started.

## Local Spike 1

Run `cargo run -- local` to start one local shell. Press Ctrl-Q to leave p2pmux.

The PTY grid is fixed from the terminal size at startup. Resizing the outer terminal never resizes
the child shell or vt100 parser: larger windows leave extra cells blank and smaller windows crop
the upper-left fixed viewport. Dynamic resize is intentionally outside Spike 1 and the MVP wire
protocol.

`create` and `join <ticket>` remain scaffolding-only commands; the local shell is not networked or
multi-pane.

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

TBD.
