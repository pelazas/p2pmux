# Sticky Chords, Contextual Footer, Tab Chrome, and Display Names

## Goal

Improve local mux UX after Spike 3 + shared control UI:

1. Sticky `Ctrl+P` / `Ctrl+T` chord modes with repeatable navigation
2. Contextual footer help for normal / pane / tab modes
3. Mouse click to change local pane focus
4. Human tab labels with active-tab highlight (`Tab #N`, red + white)
5. Persistent display names shared to peers (config + first-run prompt)

## Non-goals

- Changing outer-mux leader keys (`Ctrl+G` remap) in this change
- Interactive prompts during `brew install` (caveat text only, later)
- Mid-session rename chord
- Custom tab titles typed by users (use ordinal `Tab #1` … for now)
- Mouse claiming control or injecting input
- Guaranteeing mouse works when nested under Zellij (document workaround)

## Interaction model

### Sticky chords

- `Ctrl+P` enters **pane mode** and stays there until exit.
- In pane mode:
  - arrows move focus repeatedly without re-pressing `Ctrl+P`
  - `N` creates a pane (existing rules)
  - `X` deletes focused pane (host-only rules unchanged)
- `Ctrl+T` enters **tab mode** and stays until exit.
- In tab mode:
  - `←` / `→` switch tabs repeatedly
  - `N` creates a tab
  - `X` deletes current tab (all-host rule unchanged)
- Exit chord mode on:
  - `Esc` (consume; do not forward)
  - `Ctrl+Q` (quit still wins; clear mode)
  - any key that would otherwise forward to the focused PTY (printable / paste / etc.)
- When exiting because of a forwarding keystroke: leave mode, then forward **that same keystroke exactly once** to the focused pane’s normal input path (idle-claim rules still apply).
- Entering the other chord prefix while already in a mode switches modes (`Ctrl+T` while in pane mode → tab mode).

### Contextual footer

Three footers (join code still appendable for coordinator as today):

| Mode | Footer text |
|------|-------------|
| Normal | `Ctrl+P panes \| Ctrl+T tabs \| type to claim idle \| active typing is protected \| Ctrl+Q quit` |
| Pane | `arrows move focus \| N new pane \| X delete pane \| Esc cancel` |
| Tab | `←/→ switch tab \| N new tab \| X delete tab \| Esc cancel` |

Status prefixes (rejection / waiting) still win left side when present; mode help replaces the controls segment.

### Mouse focus

- Enable mouse capture in the shared-layout TUI event loop.
- Left-click inside a pane’s content/chrome rectangle → set local `focused_pane` to that pane.
- Click does **not** take control, claim lease, or send input.
- Clicking while in pane/tab mode may keep the mode (navigation convenience) or leave it; prefer **keep mode** so click + arrows still work.
- Click on tab bar: optional MVP stretch — if easy, click `Tab #N` switches tab; otherwise keyboard-only tabs are fine for this PR.
- Document: under Zellij, mouse may be swallowed; try `zellij` with mouse mode off / locked passthrough.

### Tab chrome

- Label tabs by session ordinal: `Tab #1`, `Tab #2`, … following the order of `snapshot.tabs` (stable left-to-right).
- Active tab (matches `current_tab`): background `Color::Rgb(220, 50, 47)` (or close red), foreground white, bold if easy.
- Inactive tabs: default/dark-gray foreground, no fill (or subtle dark fill).
- Flat highlight is enough; Powerline chevrons are optional and not required.

### Display names

**Storage**

- File: `$XDG_CONFIG_HOME/p2pmux/config.toml` (default `~/.config/p2pmux/config.toml`)
- Field: `display_name = "pelazas"`
- Validation: trimmed, non-empty, max 32 Unicode scalars, no control characters.

**CLI**

- `p2pmux config set name <name>`
- `p2pmux config get name`
- Optional: `p2pmux config path` prints resolved config path (nice-to-have).

**Session entry**

- On `create` / `join`: load config name.
- If missing and stdin is a TTY: prompt once  
  `Choose a display name (visible to session peers):`  
  validate, save to config, continue.
- If missing and non-interactive: error with exact remediation  
  `missing display name; run: p2pmux config set name <name>`
- `--name <name>` on `create`/`join` overrides for this process and updates the saved config.

**Protocol / roster**

- Add optional `display_name` string to membership descriptors used in layout state / admission (`Member` / `MemberDescriptor` and Join if needed so the coordinator stores the joiner’s chosen name).
- Names are labels only — never auth. Peer id remains the identity.
- Chrome uses display names for host badge and controller text.
- Duplicate names allowed. When two visible members share the same display name (case-sensitive compare after trim), render  
  `{name} · {short_fingerprint}`  
  where fingerprint is the existing 4-byte hex `short_peer` (or equivalent stable short id). Unique names render as `{name}` only.

**Brew (later, not this PR)**

- Formula caveat only: point at `p2pmux config set name`. No interactive brew install step.

## Testing

- Unit: sticky mode stays across multiple arrows; Esc clears; forwarding keystroke exits and is queued once.
- Unit: footer strings for each mode.
- Unit/render: active tab style and `Tab #N` labels.
- Unit: config round-trip + validation rejects empty/too-long/control chars.
- Protocol/layout: member display_name round-trips in snapshots; duplicate disambiguation helper.
- Mouse: unit-test hit-testing pane rect → pane id (no full interactive mouse required in CI).
- `cargo fmt`, `clippy -D warnings`, `cargo test`.

## Manual acceptance

1. `p2pmux config set name pelazas` then create/join — chrome shows name.
2. Sticky: `Ctrl+P`, right, right focuses across panes; type `h` exits mode and types `h` if lease allows.
3. Footer swaps in pane/tab modes.
4. Tabs show `Tab #1`… with red active highlight.
5. Second member with same name shows disambiguated chrome.
6. Click pane focuses (outside Zellij or with Zellij mouse passthrough).
