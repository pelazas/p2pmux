# Shared Control UI Design

## Goal

Make the host and guest experience describe and enforce the same shared-control
model, make control state unmistakable in the pane chrome, and remove
function-key shortcuts that collide with macOS media keys.

## Interaction model

There is no forced takeover command. A member gains control by sending ordinary
input to a pane whose current controller has been idle for the existing eight
second timeout. The claiming input, including its first character, is buffered
until the host accepts the lease claim, then forwarded to the shell. While a
controller is actively typing, other members cannot interrupt; their input is
not forwarded and they wait until the pane becomes idle before trying again.
`Ctrl+Q` is the only TUI command and exits p2pmux. The TUI consumes it before
forwarding any other input to the hosted shell, including a nested Zellij
session.

The pane host's existing `LeaseManager` remains authoritative. It serializes
concurrent idle claims; the first accepted claim advances the lease epoch and
the other claimant receives the published current lease, clears its buffered
input, and remains a spectator. Stale claim and input epochs are rejected by the
same manager. Every accepted input republishes the current lease; guests use the
receipt time of that update as their activity clock. Both render loops compare
their activity clock against the timeout every event-poll cycle and redraw when
the active/idle state changes. This change does not make the timeout
configurable.

Remove forced takeover from the wire command as well as the keyboard UI. A
take-control request has no force flag, so all claims use the ordinary idle-only
lease operation and an active controller cannot be bypassed by a crafted peer
message.

## Shared help and permissions

Host and guest render the identical control-help text: `type to claim idle |
active typing is protected | Ctrl+Q quit`. The host may retain its join ticket
in a separate status prefix, but it must not replace or alter that control help.
The current prototype has one shared pane, so its UI must not imply host-only
privilege or advertise a manual handoff action.

## Control-pane chrome

The controlled pane has a distinct border in both renderers. When the controller
is actively typing, use a vivid red-orange border and the top-left label `this
user is typing`. When the same controller remains assigned but becomes idle,
retain the border, use a muted gray-orange color, and label it `this user has
control`. This differentiates active input from idle ownership without changing
the lease protocol. Before a guest receives its first lease, render no
control-state border and prefix the same shared help with `waiting for control
state | `; a disconnect continues to terminate the TUI with its existing error.
`join_pane` must not synthesize a local initial lease: the first host-published
lease is authoritative for the controller and epoch, and guest input remains
disabled until it arrives.

## Testing

Add unit coverage around TUI-facing control state and input-command selection so
the shared help, Ctrl+Q handling, and active/idle chrome cannot regress. Cover
the first keystroke following an idle claim, concurrent/stale claim rejection,
and the pre-lease state. Update README instructions so neither F9/F10 nor forced
takeover is advertised. Run the full Cargo test suite. Finally run `p2pmux
create` in one terminal and `p2pmux join <ticket>` in another, exercising idle
automatic handoff, active-input protection, the two border states, common footer
text, and Ctrl+Q shutdown on both sides.
