# Spike plan & stack

Derived from the locked MVP design. Prove hard parts in order; pretty mux last.

## Stack (boring)

- Rust + `tokio` + `clap` + `tracing`
- TUI: `ratatui` + `crossterm`
- PTY: `portable-pty`
- Screen: `vt100`
- Network: **Iroh 1.x** (direct QUIC + public relays for dogfood)
- Protocol: **prost**, versioned length-delimited envelopes
- Invite: **reusable Iroh/session ticket** for the live session
- Short human invite codes: only later if still wanted (tiny HTTPS map)

## Spikes

### Spike 1 — Local terminal

Spawn a shell, render with vt100 in ratatui.

**Done when:** vim / top / a Claude-like TUI look correct locally.

### Spike 2 — Mirror on one machine (real protocol)

Two processes, Iroh even on localhost, real message protocol.  
Owner: PTY + snapshots/deltas. Guest: render + control/input.  
Separate queues so a fat screen update can’t block typing.  
Include control lease basics.

**Done when:** guest typing feels good on localhost; dropping an update forces resync; slow guest doesn’t stall the PTY.

### Spike 3 — Localhost shared layout (host-owned delete)

Up to eight local processes share coordinator-authoritative tabs and nested 50/50 splits. Every
new pane/tab PTY runs on the creating member’s Mac and serves direct pane streams from that member.
The coordinator reserves a pane, the creator registers its PTY, then reports ready before the
layout commit exposes it.

Commands: `Ctrl+P` then `N`/`X`/arrows; `Ctrl+T` then `N`/`X`/left/right; `Esc` cancels; Ctrl+Q
exits. F9 and F10 pass through to the focused PTY. Pane grids are fixed at creation. Pane deletion is host-only;
tab deletion requires every pane in that tab to be hosted by the requester. Maximums: 8 members,
9 tabs, 8 panes/tab, split depth 4.

**Done when:** `create` plus one or more local `join` processes show the same tree; each creator
hosts only its own new PTYs; late join receives layout then direct snapshots/leases; deletion
authority and stale/limit rejection work; retrying direct subscription survives a transient roster
race.

### Spike 4 — Real internet + presence

Run the shared-layout protocol on two Macs / different networks. Show **direct | relayed** and
add presence without changing host-owned deletion or fixed grids.

**Done when:** pane control and layout work over relay if needed; presence is coherent.

### Spike 5 — Disconnect grace + coordinator failover

5-minute placeholders; structural freeze during coordinator grace; earliest-join promotion.

### Spike 6 — Brew formula

Last. Source-build tap is enough for dogfood.

## Non-goals during spikes

Drag/resize, >8 peers, mosh prediction, custom relay deploy, sandbox/ACL tiers. Short local codes
are only dogfooding convenience; portable ticket distribution is validated in Spike 4.
