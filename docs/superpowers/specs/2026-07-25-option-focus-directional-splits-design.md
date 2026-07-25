# Option Focus and Directional Pane Splits

## Goal

Make pane navigation available without entering pane mode, and let pane creation
choose which side of the focused pane receives the new PTY.

## Interaction model

### Option (Alt) focus

- Exact `Option` / `Alt` plus an arrow key moves focus to the nearest pane in
  that direction within the current tab.
- The existing nearest-neighbour geometry selection remains authoritative.
- The shortcut works in normal mode and pane mode. It is consumed in both
  modes, including when no neighbour exists, so it never reaches a PTY.
- Tab mode is unchanged.
- Some terminals and outer multiplexers do not encode Option+arrow as an Alt
  modified key; that is a terminal/mux configuration caveat, not a fallback
  binding.

### Directional pane creation

`Ctrl+P` remains a sticky pane chord. Existing lowercase `n` keeps its current
aspect-ratio axis choice and inserts the new pane as the second child. New
lowercase commands explicitly select an axis and placement:

| Key | Axis | New pane position |
|---|---|---|
| `r` | left/right | second (right) |
| `l` | left/right | first (left) |
| `d` | top/bottom | second (down) |
| `u` | top/bottom | first (up) |

The existing split depth limit of four remains unchanged. Creating a pane keeps
focus on the original pane and retains the current pre-split grid sizing.
Directional command keys count as chord commands even when no creation intent
can be issued, preserving sticky pane mode on a depth-limit rejection.

## Authority and wire compatibility

Placement is layout authority, not a TUI rendering preference. A
`NewPanePosition { First, Second }` value flows from `UiIntent::CreatePane`
through the runtime request, protocol, coordinator reservation, and pending
reservation into the `pane_ready` split child order. Missing placement defaults
to `Second` to retain `n` semantics.

`CreatePane` gets a new protobuf field using a new tag. Protocol version moves
from 2 to 3 so an older coordinator cannot silently ignore a left/up placement
and create it on the wrong side. Protocol validation rejects unknown enum
values.

## Footer text

Normal mode includes `Alt+←↓↑→ FOCUS`. Pane mode includes
`r/l/d/u SPLIT`. Tab mode is unchanged. The renderer continues to style the
key portions with its existing accent treatment.

## Testing

- Layout tests cover first and second child order, default-second compatibility,
  and depth rejection for both placements.
- Protocol tests cover the new field's stable wire shape, default, validation,
  and version 3 behaviour.
- TUI tests cover Option-arrow consumption/focus in normal and pane modes,
  directional chord intents, sticky handling, and unchanged `n` semantics.
- Footer tests assert the new normal and pane help strings.
