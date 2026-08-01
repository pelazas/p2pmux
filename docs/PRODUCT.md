# Product brief

Source: Notion — *peer-to-peer multiplayer terminal multiplexer*  
Name: **p2pmux** — settled by shipping. v0.1.0, p2pmux.com, the Homebrew tap and the binary
all carry it, so the open item in [docs/LAUNCH_KIT.md](./LAUNCH_KIT.md) §8 is closed.

> **Scope note (2026-07-29).** This brief describes the mux accurately and stays normative for
> what the binary does. It is *not* the company sequencing. As of 2026-07-29 the mux is substrate
> rather than the wedge: the entry surface is single-player multi-machine agent supervision — one
> human, several agents, several boxes — and human multiplayer arrives as an upgrade to something
> already in use. See Notion, *Strategy — Agent rooms, the session ledger & async direction*.

## Core idea

The multiplexer is a **shared control surface, not a shared computer**.

- Every user in a session can create their own panes and tabs.
- Each pane is backed by a local PTY on that user’s machine.
- Processes in that pane (shell, Docker, Claude Code, etc.) run on the owner’s hardware, with their PATH, env, API keys, and subscriptions.
- Other users can attach as guests: see live output, then claim a free pane with ordinary input.
- The same person is host of their panes and guest on everyone else’s. Host/guest is **per pane**, not per person.

Example: Pelazas starts Claude Code in his pane (his subscription). Tis hops into that pane and prompts to do a code change. Pelazas never sees Tis’ keys; nothing runs on Tis’ machine for that pane. A code change has been made on Pelazas’ machine. At the same time, Tis can host panes that Pelazas guests into.

## What it is / isn’t

**Is**

- Lightweight local binary (brew install …)
- Zellij-like tabs, panes, and layouts
- Real-time multiplayer presence (who’s in which tab/pane)
- End-to-end encrypted peer-to-peer pane streaming
- Interactive guest access (spectate and type into a peer’s live session)

**Isn’t**

- A cloud-hosted execution environment
- A shared remote VM where everyone’s processes run
- An agent orchestration / rules engine / sandbox platform (out of scope for this product)

## Product experience

- Create a session; peers join P2P.
- Open tabs and split panes like a modern mux (Zellij-style).
- Create a pane → it runs locally on you.
- Click a teammate’s pane to focus it locally; type to claim it once it is free.
- Presence indicators show who is active where.
- If a host disconnects or sleeps their machine, their panes become unavailable for a grace period; guests cannot keep a host pane alive without the host machine.

## Technical direction

- Language: Rust
- Local: PTY management, tabs/panes/TUI
- Network: encrypted P2P streams of terminal I/O (+ optional relay for NAT)
- Collab: presence metadata alongside streams; controller keystrokes injected into the host PTY
- Agent tools: no special integration required for v1 — sharing the PTY shares the Claude Code (or other CLI) session inside it

## Why it matters

Teams already collaborate in terminals by screen-sharing or crowding onto one SSH box. This keeps execution and credential files local while making collaboration first-class: hop into a peer’s live agent or shell session without moving their machine or credentials onto yours.

## High-level success criteria

Developers on different machines can join one session, each create panes, see presence, and type into each other’s panes with correct locality (processes and subscriptions stay with the pane host), over an encrypted P2P connection.

See [MVP_DESIGN.md](./MVP_DESIGN.md) for the locked normative MVP rules.
