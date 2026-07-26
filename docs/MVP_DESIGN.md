# MVP Design (locked)

**LOCKED** 2026-07-24 — Pelazas decisions + Codex (`gpt-5.6-terra`) normative wording.

This document is the **source of truth** for the MVP. Older Notion section pages are historical if they conflict.

---

## 0. Trust warning (product + README + invite UI)

This is a **fully trusted shared-shell** session. Anyone with the join ticket can see every pane and may obtain interactive control of unlocked terminals (run commands, see output, touch files reachable to that macOS user). A pane host can temporarily make its own pane host-only, but locking is not an ACL. Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege Mac account and avoid production credentials in shared panes.

**Clarification:** Processes and credential *files* stay on the pane host’s Mac (not uploaded to peers). That does **not** stop a controller from using or displaying them via the shared shell.

## 1. Product one-liner

Brew-installable macOS terminal mux: each pane’s process runs on its host’s machine; members in one encrypted P2P session see a shared layout, presence, and can take control of panes — secrets/files aren’t copied to other laptops, but teammates with control share that shell.

## 2. Locked decisions

- **Audience / success bar:** Dogfood & polish primarily with 2 people; protocol supports **N members**, **v1 hard cap 8** concurrent.
- **Platform:** macOS only
- **Approach:** custom thin mux + Iroh P2P (not Zellij wrap)
- **Invite:** one **reusable shareable join ticket** for the life of the live session. Not single-use. Caps at 8 members.
- **Roles:** **coordinator** serializes shared structural state (layout, admission, lifecycle). **Pane host** = whose Mac runs that PTY (not exclusive control rights).
- **Trust:** all members fully trusted; full shell control when they are controller.
- **Visibility:** everyone sees every pane; no private panes.
- **Spike 3 structural authority:** any admitted member may split a pane or create a tab; the new
  fixed-grid PTY runs on that requester’s Mac. Only a pane’s host may delete that pane. A tab may
  be deleted only by a member that hosts every pane in it. There is no close confirmation in this
  spike; stale requests are rejected rather than replayed.
- **Input:** a pane has a controller only while that peer is actively typing. After about eight
  seconds idle, the host clears the controller and the pane is free; the next ordinary key claims
  it and is delivered as the first input. Active typing is protected; there is no forced takeover.
- **Pane lock:** a pane host may toggle its own pane to host-only. All members still see its
  screen, but only its host may input or claim control. Locking clears a guest controller and does
  not claim control for the host.
- **Later-spike disconnect behavior:** 5-minute unavailable placeholders and coordinator failover
  are MVP goals, not implemented by Spike 3.
- **Layout:** nested binary 50/50; **depth ≤ 4**; **≤ 8 panes/tab**; max **9 tabs**.
- **Latency:** ≈ same-region SSH; relay normal
- **Non-goals:** cloud VM execution, sandbox/ACL tiers, Win/Linux, drag-resize, mosh prediction, private panes, per-person ticket revocation in v1

## 3. How it works (plain language)

1. You `create` → reusable join ticket → first shell on your Mac. You start as coordinator.
2. Others `join <ticket>` (up to 8). Everyone sees the same tabs/panes.
3. Shells run on whoever **hosts** that pane’s Mac. Others watch; an idle pane is free for the
   next member's ordinary key to claim and deliver. Active typing is protected until the host clears
   the controller after the idle timeout.
4. Any member can split an available pane or create a tab; the requester hosts the new PTY, but is
   not its controller and the new pane starts free. Only the host can delete a pane. A member can
   delete a tab only when it owns every pane in that tab.
5. Spike 3 is localhost layout/control work. Disconnect grace and coordinator failover remain later spikes.

## 4. Architecture

```text
Members (≤8) on macOS
TUI + local PTYs for panes they host
Guest render + control/input when controller
        \______ Iroh P2P (+ relay) ______/
                    ^
            reusable join ticket
```

**Modules:** `cli`, `tui`, `pty_host`, `session` (coordinator + registry), `protocol`, `transport` (Iroh), ticket helper.

### Local detach / resume

A live session has one headless node process and at most one local TUI client. The same binary
starts the private node entrypoint; it owns PTYs, Iroh, rendezvous, and in-memory layout/focus.
The client connects through a Unix socket and Ctrl+Q detaches, releasing local control leases
without stopping the node. `--resume` shows live sessions and `attach <name>` reconnects.
`kill <name>` is graceful (and warns before coordinator shutdown).

The durable finder descriptor lives in `~/Library/Application Support/p2pmux/sessions`; its socket
lives in `/tmp/p2pmux-$UID`. Finder records contain no ticket, PTY, screen, layout, or focus state.
There is no local-client takeover, launchd registration, disk screen restore, coordinator failover,
or offline-pane grace in this implementation. Offline host placeholders/grace remain follow-up work.

**Screen:** pane host keeps vt100 canonical state; sequenced snapshot+deltas to all members; gap → resync; never stall PTY on slow viewers.

**PTY grid:** fixed at pane creation; immutable; no Resize message; clients scale/letterbox.

**Protocol safety:** versioned messages, authenticated sender, max sizes; unsafe terminal escapes (clipboard/URL open/etc.) ignored or confirm locally.

### Coordinator failover (v1)

Coordinator alone serializes layout/admission/lifecycle.

If coordinator disconnects:

- Session continues; other hosts’ panes keep working.
- Coordinator’s hosted panes → unavailable placeholders.
- During **5-minute grace:** pane-level control on *available* panes still works; **structural edits paused** (split/close/join admission).
- If coordinator returns within grace → they remain coordinator; structural edits resume.
- If grace expires: promote connected member with **earliest join order** as coordinator in a new **coordinator epoch**. Returning old coordinator rejoins as ordinary member (does not auto-reclaim).

### Input control

- `controller` exists only during active typing; focus ≠ control.
- After ~8 seconds idle, the host publishes an empty controller and a new lease epoch: the pane is
  free. The next ordinary input claims it and carries its first key.
- Active typing rejects other ordinary input; no forced takeover exists.
- Claims are serialized so two streams never hit one PTY.

### Spike 3 layout and deletion

Any member splits any available pane or creates a tab → **new PTY on the requester’s machine**;
50/50 nested splits; depth≤4; ≤8 panes/tab; ≤9 tabs. All panes are visible to all. A pane host
alone may delete its pane. Deleting a tab requires the requester to host every leaf in that tab.
The last pane in a tab must be removed by deleting the tab; the final tab is retained.

### Spike 3 controls

`Ctrl+P` then `N` splits; `Ctrl+P` then `X` requests focused-pane deletion; `Ctrl+P` then `k`
toggles host-only lock for a host-owned focused pane; `Ctrl+P` then `e`
renames the focused pane for all admitted members. `Ctrl+P` plus arrows moves focus. `Ctrl+T` then
`N` creates a tab; `Ctrl+T` then `X` requests tab deletion; `Ctrl+T` then `e` renames the current
tab for all admitted members; `Ctrl+T` plus left/right switches tabs. Rename uses Enter to save and
Esc to cancel; a blank title restores the ordinal label. These mux chords are consumed locally. Ctrl+Q detaches
the local view while the session node remains live; F9 and F10 reach the focused PTY. Pane grids never resize.

### Disconnect grace (any member)

5 minutes unavailable placeholders for that host’s panes; reconnect restores; after expiry current coordinator removes them; departed client must kill orphaned local PTYs on next reconnect (can’t remotely force-kill an offline machine).

## 5. UI / presence

Tabs + nested splits as above. Tab labels are clickable for local tab switching. Each pane title is
`Pane #N  host: <name>  control: free|<name>|…|host-only`; a locked pane also reserves a
right-aligned `(locked by <host>)` badge. A focused free pane has a white border, an
actively controlled pane a red-orange border, and an unbootstrapped lease a yellow focused border.
The dark contextual footer uses red key accents: normal mode shows
`Ctrl+ <p> PANE   <t> TAB   <q> QUIT    type to claim when free`; pane and tab modes show their
focus/switch, new, close, and back commands. Direct | relayed indicator.

## 6. Wire sketch

Membership, `LayoutCommit` revisions, presence, control lease / Take control, Snapshot/Delta, SessionSnapshot bootstrap for joiners. Split/close → coordinator commit (except during coordinator grace freeze).

## 7. Failure modes

| Event | Behavior |
|-------|----------|
| Member disconnect | 5 min placeholders for their hosted panes; session continues |
| Coordinator disconnect | As above + structural freeze; failover if grace expires |
| Slow viewer | Drop deltas; fresh snapshot; never stall PTY |
| Process exit | Mark exited; closeable |
| Direct↔relay | Transparent; show indicator |

## 8. Stack

Rust, tokio, clap, tracing, ratatui+crossterm, portable-pty, vt100, Iroh 1.x, prost. Reusable session ticket. No libp2p/WebRTC/CRDT/DHT.

## 9. Spike plan

See [SPIKE_PLAN.md](./SPIKE_PLAN.md).

## 10. MVP success criteria

Two (then more) Macs join via reusable ticket; shared layout; both host panes; anyone can split
available panes; pane hosts delete and lock their own panes; all-host tabs can be deleted; free-pane first-
key claims and protected active typing; presence; locality of processes; encrypted P2P/relay; ≈ SSH feel;
disconnect grace works; coordinator leave does not kill whole session.

## Caveat (honest)

Reusable ticket ≈ session-wide root credential (no per-person revoke in v1). Coordinator failover favors availability; rare partition edge cases possible — trusted small-group tool, not enterprise ACL.
