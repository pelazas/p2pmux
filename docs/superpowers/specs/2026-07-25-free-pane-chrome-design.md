# Free-pane control, pane titles, tab clicks, and footer chrome

## Goal

Polish shared-layout UX after sticky chords / display names:

1. Control means **actively typing only**; idle panes are **free**
2. Pane titles: `Pane #N  host: <name>  control: …`
3. Click tab labels to switch tabs
4. A focused free pane has a white border
5. Zellij-inspired contextual footer with accent keys

## Non-goals

- Changing idle timeout duration (keep ~8s)
- Forced takeover / F9
- Custom user-typed pane or tab titles
- Perfect mouse under Zellij (document caveat)
- Brew onboarding changes

## Control model

### Semantics

- A pane has a controller **only while that peer is actively typing** (activity within `IDLE_AFTER`).
- When idle timeout elapses, the pane host **clears** the controller and publishes a new lease epoch with empty `controller_peer_id`.
- A free pane accepts the next member’s input: claim + deliver the first keystroke (same buffering rules as today’s idle claim).
- Concurrent claims on a free pane: host `LeaseManager` serializes; one Publish winner; losers clear buffered input.
- Active typing still rejects other members’ input.

### Wire / lease

- `ControlLease.controller_peer_id` may be empty ⇒ free.
- Clearing on idle bumps `lease_epoch` and republishes to watchers (same path as today’s lease publish).
- Guests: empty controller ⇒ free chrome; do not show a retained owner.
- Free panes show `control: free`.

### Initial pane state

- Newly created panes start **free** until someone types into them.
- Creator is still the **host** (PTY owner); host ≠ controller.

## Pane chrome

Ordinal `N` is 1-based among panes visible on the **current tab** (stable traversal order of that tab’s leaves).

Title format:

```text
Pane #1  host: pelazas  control: free
Pane #2  host: tis  control: pelazas
Pane #3  host: pelazas  control: …
```

| State | `control:` value | Border (focused) | Border (unfocused) |
|-------|------------------|------------------|--------------------|
| Free | `free` | White | Dark gray |
| Typing | display name (disambiguated if needed) | Red-orange `Rgb(255,69,0)` | same red-orange or slightly dim |
| Waiting for lease bootstrap | `…` | Yellow or white | Dark gray |

Free panes use the colors in the table.

## Tab click

- Hit-test clicks against rendered tab label rectangles.
- Left-click on `Tab #N` → local `SwitchTab` / set current tab (same as keyboard).
- Does not affect leases or input.
- Pane-mode / tab-mode sticky chords may remain active across the click.

## Footer design

Dark footer. Muted labels. Accent red on the key glyph inside `<…>` (or equivalent). Spaced groups — fewer than Zellij.

**Normal**

```text
Ctrl+ <p> PANE   <t> TAB   <q> QUIT    type to claim when free
```

**Pane mode**

```text
Pane  <←↓↑→> FOCUS   <n> NEW   <x> CLOSE   <Esc> BACK
```

**Tab mode**

```text
Tab   <←→> SWITCH   <n> NEW   <x> CLOSE   <Esc> BACK
```

Coordinator join code remains a quiet right-side / truncated suffix when present. Status prefixes (rejection, etc.) still take left priority when set.

## Docs

Keep README, shared-control, and MVP snippets aligned with free panes and the current chrome.

## Testing

- Lease: idle clear publishes empty controller + new epoch; free pane accepts first input; active rejects others.
- Chrome: title strings; border colors for free vs typing.
- Tab click hit-test.
- Footer mode strings / render accents (unit-level).
- `cargo fmt`, `clippy -D warnings`, `cargo test`.

## Manual acceptance

1. Type in a pane → red border + `control: <you>`.
2. Wait ~8s → white focused border + `control: free`.
3. Other member types one char → they become controller; char delivered.
4. Pane titles show `Pane #N` + host name.
5. Click tabs to switch.
6. Footer matches mode styling.
