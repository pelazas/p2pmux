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

Spike 2 provides one host-created, fixed-grid shared terminal pane over an authenticated Iroh
connection. It intentionally has no tabs, splits, resize protocol, or multiple panes yet.

## Local Spike 1

Run `cargo run -- local` to start one local shell. Press F10 to leave p2pmux.

The PTY grid is fixed from the terminal size at startup. Resizing the outer terminal never resizes
the child shell or vt100 parser: larger windows leave extra cells blank and smaller windows crop
the upper-left fixed viewport. Dynamic resize is intentionally outside Spike 1 and the MVP wire
protocol.

To dogfood the shared host/guest pane:

```text
Terminal 1: cargo run -- create
Terminal 2: cargo run -- join <printed 10-character code>
```

`create` prints `Join with: p2pmux join <CODE>`, waits for Enter so you can copy it, then
enters the host shell. The same code stays in the host status bar. `join` renders that remote pane.
Short join codes resolve through a restrictive local cache on the same Mac, so they are for current
dogfooding only; they work while the corresponding `create` process is alive and are removed when
it exits. Long `p2pmux-v1:` tickets remain accepted for backwards compatibility.

Only one peer controls input at a time: after about eight seconds of idle time another guest can
type to hop in; while someone is actively typing, press F9 to take control. F10 exits only
the local p2pmux view.

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
