# Shared Control UI Design

## Goal

Make every member render and operate the same shared tab/split layout while preserving the
authority of the member that hosts each PTY. This is the Spike 3 localhost design; later
internet, presence, disconnect-grace, and failover work must not be implied as complete.

## Interaction model

One controller leases each pane. A member gains control by ordinary input when the current
controller has been idle for the existing eight-second timeout; the claiming input is buffered
until the host accepts the lease claim. Active typing is protected: no UI or wire-level forced
takeover exists. Ctrl+Q exits only the local p2pmux view; F9 and F10 pass through to the focused
PTY.

The local mux consumes its commands before any focused PTY receives them:

- `Ctrl+P`, then `N` splits the focused pane; `X` requests deletion; arrows move focus.
- `Ctrl+T`, then `N` creates a tab; `X` requests tab deletion; left/right switch tabs.
- `Esc` cancels a pending chord.

The new PTY for a split or tab runs on the requester’s Mac with a fixed creation grid. A pane’s
host alone may delete it. A tab deletion succeeds only when the requester hosts every pane in
that tab. The last pane in a tab is removed by deleting the tab; the final tab cannot be deleted.
All splits are nested 50/50 (depth 4; up to 8 panes/tab and 9 tabs).

The pane host's existing `LeaseManager` remains authoritative. It serializes
concurrent idle claims; the first accepted claim advances the lease epoch and
the other claimant receives the published current lease, clears its buffered
input, and remains a spectator. Stale claim and input epochs are rejected by the
same manager. Every accepted input republishes the current lease, so remote members use receipt
time as their activity clock. A guest waits for its first host-published lease rather than
synthesizing an initial one. This change does not make the timeout configurable.

## Shared help and permissions

Every member renders the same tab bar, focused-pane chrome, host badge, and controller state.
The footer communicates `Ctrl+P panes | Ctrl+T tabs | type to claim idle | active typing is
protected | Ctrl+Q quit`; the coordinator may append its join code. Focus never grants control.
The UI shows reservation, rejection, and waiting-for-snapshot/lease status without blocking pane
drain or layout control.

## Control-pane chrome

Every controlled pane has a distinct border: vivid red-orange and `this user is typing` while its
controller is active, muted gray-orange and `this user has control` once idle. Before
screen/lease bootstrap, the leaf remains in the authoritative layout with a compact waiting state.
Remote screen data is letterboxed or clipped inside its leaf; no client resizes the host PTY.

## Testing

Cover chord consumption, focus, fixed-grid geometry, lease transitions, coordinator request /
reservation / ready / commit lifecycle, host-only deletion, all-host tab deletion, and remote
subscription retry after a transient failure. Also cover Ctrl+Q, F9/F10 PTY forwarding, active /
idle chrome, host lease publication after accepted input, and waiting for the first lease. Run the
full Cargo suite and strict clippy. Finally run `p2pmux create` in one terminal and `p2pmux join
<ticket>` in another, exercising split, pane delete, tab create/delete, idle handoff, active-input
protection, and Ctrl+Q on both processes.
