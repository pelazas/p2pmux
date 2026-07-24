# MVP Design (locked)

**LOCKED** 2026-07-24 — Pelazas decisions + Codex (`gpt-5.6-terra`) normative wording.

This document is the **source of truth** for the MVP. Older Notion section pages are historical if they conflict.

---

## 0. Trust warning (product + README + invite UI)

This is a **fully trusted shared-shell** session. Anyone with the join ticket can see every pane and may obtain interactive control of available terminals (run commands, see output, touch files reachable to that macOS user). Share the ticket only with people you trust with that access. For risky/unknown collaborators, use a separate low-privilege Mac account and avoid production credentials in shared panes.

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
- **Split/close:** any member may split or close any **available** pane (with close confirmation).
- **Input:** one controller per pane; **idle → hop in and type, no approval**; if controller is **actively typing**, explicit **Take control** required.
- **Disconnect grace:** **5 minutes** — departed host’s panes = unavailable placeholders; no input to those PTYs; reconnect within grace restores; after grace coordinator removes placeholders.
- **Coordinator disconnect:** session **continues**; that host’s panes unavailable; see Coordinator failover.
- **Layout:** nested binary 50/50; **depth ≤ 4**; **≤ 8 panes/tab**; max **9 tabs**.
- **Latency:** ≈ same-region SSH; relay normal
- **Non-goals:** cloud VM execution, sandbox/ACL tiers, Win/Linux, drag-resize, mosh prediction, private panes, per-person ticket revocation in v1

## 3. How it works (plain language)

1. You `create` → reusable join ticket → first shell on your Mac. You start as coordinator.
2. Others `join <ticket>` (up to 8). Everyone sees the same tabs/panes.
3. Shells run on whoever **hosts** that pane’s Mac. Others watch; when idle they hop in and type; if someone’s mid-typing, use Take control.
4. Anyone can split any available pane (new shell starts on the splitter’s Mac). Anyone can close a pane (confirm).
5. If someone disconnects: their hosted panes go idle/unavailable for 5 minutes (session keeps going). If they return in time, panes come back.
6. If the coordinator disconnects: same grace for their panes; structural edits pause briefly; if they don’t return, another member becomes coordinator (see below).

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

- `controller` + idle(~8s)/typing; focus ≠ control.
- Idle: another member may take over by hopping in / typing — **no approval**.
- Typing: explicit **Take control** (visible handoff).
- Control serialized so two streams never hit one PTY.

### Splits

Any member splits any available pane → **new PTY on the splitter’s machine**; 50/50; depth≤4; ≤8 panes/tab. All panes visible to all.

### Close

Any member may close any available leaf pane (confirm; kills that PTY; sibling expands). Placeholders during grace aren’t manually closable; restored or pruned after grace.

### Disconnect grace (any member)

5 minutes unavailable placeholders for that host’s panes; reconnect restores; after expiry current coordinator removes them; departed client must kill orphaned local PTYs on next reconnect (can’t remotely force-kill an offline machine).

## 5. UI / presence

Tabs + nested splits as above. Tab bar: who is on which tab. Pane: host badge, focus, **controller/driver**, idle vs typing cue. Direct | relayed indicator.

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

Two (then more) Macs join via reusable ticket; shared layout; both host panes; anyone can split available panes; idle hop-in typing; Take control when busy; presence; locality of processes; encrypted P2P/relay; ≈ SSH feel; disconnect grace works; coordinator leave does not kill whole session.

## Caveat (honest)

Reusable ticket ≈ session-wide root credential (no per-person revoke in v1). Coordinator failover favors availability; rare partition edge cases possible — trusted small-group tool, not enterprise ACL.
