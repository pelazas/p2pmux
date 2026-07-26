# Option+Shift drag pane resize with host PTY reflow

## Goal

Let any admitted member resize a shared split by dragging inside a pane while
keeping two facts true at once:

1. the split proportion is one authoritative, revisioned value shared by every
   member; and
2. every pane host resizes its own PTY and VT screen to the grid implied by its
   own local terminal chrome.

This replaces the current fixed-at-birth behavior for shared-layout panes.
Today a split only gives the newly created pane a birth grid; the existing
pane's PTY does not resize, and outer terminal resize only crops or letterboxes
the fixed grids. This feature makes both drag-driven reflow and outer-window
reflow resize hosted PTYs.

## Non-goals

- Divider-only hit testing or a divider drag alias.
- Keyboard ratio nudges.
- Dual-axis / diagonal resize in one gesture.
- Continuous layout commits while moving the mouse.
- Resizing a remote PTY from a guest.

## Interaction

### Starting and locking a gesture

The resize gesture is `Option`/`Alt` + `Shift` + left-button drag that starts
inside a pane's **chrome content rect** (the bordered pane interior, not only
the currently visible fixed-grid viewport). The down event captures all of:

- the pressed `pane_id`;
- the snapshot revision (`base_revision`);
- pointer origin;
- the modifier decision; and
- the starting layout ratio.

Both Alt and Shift must be present on mouse-down. Subsequent `Drag` and `Up`
events use that captured decision; they must not depend on modifiers which the
terminal may no longer report. A left drag lacking either modifier remains the
ordinary text-selection path. The implementation must never infer resize from
an Escape timing sequence.

Before the pointer moves two terminal cells from its origin, the gesture is
pending and does not alter selection or layout. At that threshold it locks once
to the axis with the greater displacement; an exact tie locks left/right. It
does not change axis later in the drag.

For the locked axis, the target is the nearest ancestor `Node::Split` of that
axis containing the pressed pane. The client may locate that node to draw its
overlay, but the authoritative target is only `(pane_id, axis)` and is resolved
again by the coordinator. If there is no such ancestor, the modifier gesture is
consumed and cancelled: it must neither select text nor send a request.

### Direction and release

The target split's `first_share_bps` denotes its first child's portion. The
drag maps pointer movement to the share using the target split's original
allocated span and captured ratio:

- for a pressed pane under the first child, positive movement right/down grows
  it and negative movement shrinks it;
- for a pressed pane under the second child, the sign is reversed.

Thus motion toward the pressed pane's outer edge grows it; motion toward the
shared boundary shrinks it. The value is recomputed from the down-time baseline
rather than compounded from individual drag events.

After the axis is locked, the TUI draws a local, ephemeral geometry overlay.
It changes neither `LayoutSnapshot` nor PTYs. On mouse-up it clears the overlay
and sends one `SetSplitRatio` request with the down-time `base_revision` if a
target was found and the proposed share differs from the original. A drag that
never crosses the threshold, has no target, or ends at the original share sends
no request.

A newer authoritative layout state received while a resize drag is active
cancels the gesture and drops its overlay before rendering that state. A stale
ratio rejection also drops the overlay; the client does not retry an old
absolute ratio. Concurrent changes are therefore first-commit-wins.

Normal footer help includes `Option+Shift+drag RESIZE` when its existing
space-aware rendering has room; truncation may omit this suffix before it
displaces status or join-code information.

## Shared ratios and allocation

`Node::Split` gains `first_share_bps: u16`. Valid values are `1..=9999`; every
new split starts at `5000`. The ratio belongs to the split, so create, delete,
member-removal, and other tree rewrites preserve it whenever that split
survives. Collapsing a split removes that split and naturally removes its ratio.

The protobuf `LayoutSplit.first_share_bps` is an optional field. An absent
field decodes as `5000` for compatibility with stored/in-memory old shapes;
explicit `0` and `10000` (and every value outside `1..=9999`) are invalid.

The allocator uses the same ratio for rendering and grid calculation. For an
available split span `S`, its unconstrained first span is
`floor(S * first_share_bps / 10000)`. It then clamps that split point to the
recursive minima of its children when the parent can satisfy them. A leaf has a
minimum **outer** rectangle of 3 columns by 3 rows, including its borders. A
left/right split has width `first.width + second.width` and height
`max(first.height, second.height)`; a top/bottom split has width
`max(first.width, second.width)` and height `first.height + second.height`.

If a local terminal is too small to satisfy those minima, allocation remains
safe and deterministic using the ratio with saturating rect arithmetic; it may
render smaller leaves. It never writes a compensating ratio to shared state.
Different window sizes consequently produce different pixel/cell allocations
without disagreement about the shared ratio.

## Authority, protocol, and grid ownership

Protocol version moves from 3 to 4. `LayoutRequest` remains exactly one action
and gains two v4 actions:

- `SetSplitRatio { pane_id, axis, first_share_bps }` (new request field/tag);
- `UpdatePaneGrids { panes: repeated PaneGrid { pane_id, grid_rows, grid_cols } }`
  (a separate new request field/tag).

`LayoutSplit.first_share_bps` uses a new tag. Existing `LayoutCommit.state`
already carries `PaneDescriptor.grid_rows` and `grid_cols`, so no separate
commit body is needed. All fields fit in v4.

For `SetSplitRatio`, the coordinator requires a nonzero request ID and base
revision, exactly one action, a valid ratio/axis/pane, and an admitted sender.
It applies the strict revision check and the existing pending-reservation freeze
before finding the nearest matching ancestor and changing its ratio. It does
not require the requester to host the pane: any admitted member may resize.
The accepted mutation advances the revision and broadcasts the resulting
`LayoutCommit`.

`UpdatePaneGrids` is the host-only reconciliation path. It has the same strict
revision and reservation-freeze rules, validates a nonempty, duplicate-free
batch of nonzero valid grids, and permits a sender to update only panes it
hosts. It updates `SessionState::Pane` grid metadata, advances the revision,
and broadcasts the full `LayoutCommit`. A runtime sends only entries that
actually differ. If a grid-update request is stale, it re-derives grids from
the newest ratio and local window before trying again; it never retries a stale
ratio request.

This allows multiple hosts to settle safely: a ratio commit may cause several
hosts to calculate grids; one grid update wins, later hosts rebase and publish
only their still-different hosted panes. The process ends once every pane's
hosted grid matches its host's calculation. Guests never call `MasterPty::resize`
for a remote pane; they receive the grid metadata through a commit and the
resized screen through the existing snapshot/delta stream.

## PTY and screen reflow

When a runtime applies an authoritative ratio commit, it compares old and new
layout, calculates local chrome content rectangles for the changed tab, and
reflows every locally hosted descendant leaf whose allocated rectangle changed
(reflowing all locally hosted leaves in that tab is an acceptable equivalent).
The calculation uses the host's local terminal area, even if that tab is not
currently selected. It converts each pane content rect with the existing
`grid_for_pane` rule, so grids remain at least 1 by 1 even under extreme
terminal sizes.

For each changed hosted grid the runtime, in order:

1. calls `PtyHost::resize` / portable-pty `MasterPty::resize`;
2. resizes the matching `HostScreen` / vt100 parser;
3. publishes a fresh screen snapshot to its `watch` subscribers (never a delta
   based on the pre-resize dimensions); and
4. batches the changed grids into `UpdatePaneGrids`.

The drag overlay performs none of these operations. v1 commits only on mouse
release, so PTYs resize after a ratio commit or a received authoritative ratio
commit, not on every drag event.

Outer `Event::Resize` is intentionally included. The shared-layout runtime
recomputes all locally hosted panes against its current chrome and follows the
same PTY/screen/grid-update path. This makes dynamic grids consistent whether
the allocation changed because of a drag or because the host window changed.
The single-pane `local` mode remains outside this feature unless its own resize
loop is deliberately changed in later work.

## Failure behavior

- A PTY or screen resize failure leaves that pane's current published grid
  unchanged, reports a local status error, and does not send a false grid
  update. Other panes may continue to reconcile.
- A rejected or stale `UpdatePaneGrids` is recomputed from the newest state;
  a rejected `SetSplitRatio` is not retried.
- A malformed protocol layout, invalid ratio, unknown matching ancestor, a
  non-host grid update, or a mutation during reservation freeze is rejected by
  the coordinator and leaves authoritative state untouched.

## Testing and documentation

Tests cover ratio defaults/validation and rewrite preservation, protocol v4
wire and rejection behavior, coordinator permissions/revisions/freeze,
ratio-aware recursive allocation, modifier capture and drag cancellation,
overlay-only rendering, PTY and HostScreen resizing, forced post-resize
snapshots, host grid reconciliation, and guest non-resize behavior. README
status and interaction text must be updated to remove the fixed-grid / no
resize claims and describe host-owned dynamic grids.
