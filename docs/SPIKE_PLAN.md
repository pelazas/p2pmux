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

Four phases, in order. The ordering matters: an earlier draft put the public-IP Linux droplet
first, but a droplet has no NAT, so it proves only the easy case *and* needs a Linux build
validated before it reports anything. The hotspot test needs no infrastructure and proves the hard
case in half an hour. Cheapest test of the riskiest thing goes first.

**A — Instrument first.** `direct | relayed` in the status bar, RTT beside it, path-change
transitions logged. Without this every later failure is undiagnosable: rendezvous or NAT?

**B — Connectivity ladder.** Two Macs on one LAN; then hotspot Mac to home-wifi Mac (the real NAT
proof); then forced relay with the direct UDP paths firewalled; then Mac to Linux droplet, which
also gives the dev loop an always-on peer. With only one Mac, hotspot-Mac to droplet still
exercises cellular CGNAT on the side that matters.

**C — What localhost cannot show.** Expected in order of pain: screen-snapshot bandwidth starving
typing over a relay (no frame-rate cap or delta coalescing is tuned for a ~100ms path); input
latency with no local echo, since mosh prediction is a non-goal that may not survive 80-150ms;
transient drops killing sessions because disconnect grace is Spike 5, which will masquerade as a
connectivity failure unless minimal reconnect is pulled forward; and ticket staleness, since a v2
ticket snapshots `EndpointAddr` at mint time and a network switch invalidates its direct
addresses. Whether n0 discovery still resolves the node ID alone decides the shape of any future
rendezvous record, so test it explicitly.

**D — Spend the breaking-change window.** ALPN is `p2pmux/2` with no external users, so protocol
breaks are free now and expensive after distribution. Mint `session_id` as independent random
bytes instead of the coordinator's endpoint public key, check it at `handshake_join`, and have the
coordinator hold a roster of admitted node public keys. Not ACLs — just make identity exist, so
revocation and per-pane permissions become additive later.

**Done when:** `direct | relayed` and RTT are correct; the hotspot test carries pane control and
layout on whichever path it gets; a forced-relay session is usable for typing, or the latency
number saying otherwise is written down; the droplet build runs a session against a Mac; ticket
staleness across a network switch is characterized either way; `session_id` is independent of the
endpoint key; presence is coherent.

**Status (2026-07-29).** A — done: `direct 55ms` / `relayed 120ms ×3` in the tab bar, asserted on
both peers by `scripts/e2e/scenario_l_internet.py`. B — Mac↔droplet done, direct and forced-relay,
up to +300ms shaping; the hotspot rung is the one still needing hands, see *Two-Mac checklist*
below. D — done, and it turned out to be a real hole rather than tidiness: `session_id` *was* the
coordinator's public key, a value presented in every TLS handshake and published to discovery, so
anyone who learned the node id could mint a working ticket. Ticket v3 carries 32 independent
random bytes; v1/v2 still parse and are flagged `secret_is_public()`. The coordinator now keeps an
admitted roster, which is what the session lock is built on. C is the remainder: bandwidth and
input-latency behaviour under a relay, and ticket staleness across a network switch.

### Two-Mac checklist (hands-on, ~10 minutes)

Everything below is already automated against the droplet; this is the part only a human can do.

1. On Mac A: `p2pmux create --name a`, then `Ctrl+P` then `i` to copy the full ticket.
2. On Mac B: `p2pmux join <ticket> --name b`. Both tab bars should read `direct` with an RTT near
   your LAN ping. Create a pane on each; each pane's shell runs on its own Mac.
3. Put Mac A on a phone hotspot and repeat. This is the carrier-NAT case no droplet can produce:
   `direct` means holepunching survived CGNAT, `relayed` means it fell back, and either is a
   result worth writing down — the point is that the badge now tells you which.
4. `Ctrl+P` then `Shift+L` on Mac A, then have Mac B leave and try to rejoin: it must be told the
   session is locked, not merely dropped.
5. `Ctrl+P` then `k` on a Mac A pane while Mac B is typing into it: Mac B's keystrokes must stop
   reaching it, and the pane header must say `locked by a`.

### Spike 5 — Disconnect grace + coordinator failover

5-minute placeholders; structural freeze during coordinator grace; earliest-join promotion.
Minimal reconnect lands early, in Spike 4, so internet tests are not read as failures.

### Spike 6 — Brew formula

Last. Source-build tap is enough for dogfood.

## Non-goals during spikes

Drag/resize, >8 peers, mosh prediction, custom relay deploy, sandbox/ACL tiers. Short local codes
are only dogfooding convenience; portable ticket distribution is validated in Spike 4.
