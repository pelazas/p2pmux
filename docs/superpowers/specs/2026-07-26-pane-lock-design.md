# Host pane lock

## Goal

Let a pane host temporarily make that pane host-only. Every member continues to receive its screen,
but only the host may provide PTY input or take its control lease.

## Rules

- `Pane.locked: bool` is authoritative shared layout state. The locker is always `host_peer_id`;
  there is no separate locker field.
- Only the pane host may request `SetPaneLock`. A guest request receives the existing `NotHost`
  layout rejection.
- The pane host enforces the lock before `LeaseManager`: guest `Input` and `TakeControl` events are
  discarded. Lease behavior itself is otherwise unchanged.
- Locking clears a guest controller and publishes the resulting free lease. It preserves an
  existing host controller and never auto-claims for the host. Unlocking restores normal lease
  claim behavior.
- Client, node IPC, and remote forwarding suppress guest key/paste sends as UX. Host enforcement
  remains the authority.

## UI

`Ctrl+P`, then lowercase `k`, toggles the focused host-owned pane. Pane mode remains sticky and the
footer shows `<k> LOCK`. Locked chrome shows `control: host-only` in the left title and a
right-aligned `(locked by <host display name>)` badge. The badge receives width budget first; the
left title uses the remainder.

## Wire

This is protocol version 7. `PaneDescriptor.locked` is bool tag 6. `LayoutRequest.set_pane_lock`
is message tag 11 with `SetPaneLock { pane_id = 1, locked = 2 }`. Requests must contain exactly
one layout action; mixed v6/v7 peers fail the existing version handshake.

## Non-goals

This is not a visibility feature, ACL, forced takeover, or change to idle timeouts. It does not
lock tabs or preserve a lock across a new PTY.
