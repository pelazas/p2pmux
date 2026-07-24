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

### Spike 3 — Real internet

Same code, two Macs / different networks. Show **direct | relayed**. Reusable ticket join.

**Done when:** typing works over relay if needed; disconnect grace behavior matches the design.

### Spike 4 — N-member roster + split-any + presence + idle hop-in

Still dogfood with 2; exercise coordinator layout commits, anyone-can-split, presence, idle hop-in / Take control when typing.

**Done when:** joiner-created/split panes host on splitter’s machine; stale/over-limit layout ops rejected; leave behaviors match failure table.

### Spike 5 — Tabs + nested splits

Exercise tree: L/R then split right top/bottom; max 9 tabs / depth 4 / ≤8 panes.

**Done when:** all members see the same tree; late join gets one coherent bootstrap (layout + ownership + screen snapshots).

### Spike 6 — Disconnect grace + coordinator failover

5-minute placeholders; structural freeze during coordinator grace; earliest-join promotion.

### Spike 7 — Brew formula

Last. Source-build tap is enough for dogfood.

## Non-goals during spikes

Drag/resize, >8 peers, mosh prediction, custom relay deploy, short codes before Spike 3 works, sandbox/ACL tiers.
