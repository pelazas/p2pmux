# Mid-session pane and tab rename

## Goal

Allow any admitted member to give the focused pane or current tab a shared, persistent chrome
title without changing session-room names or control authority.

- `Ctrl+P`, then lowercase `e`, opens a modal rename prompt for the focused pane.
- `Ctrl+T`, then lowercase `e`, opens a modal rename prompt for the current tab.
- The title is shared layout metadata. It appears in layout commits and snapshots, so every member,
  including late joiners and a reattached local client, renders the same chrome.

## Non-goals

- Changing the CLI `rename` command, which names a session room and is unrelated.
- Titles as authentication, authorization, ownership, or a control-lease capability.
- Per-member/private titles, title history, undo, filesystem persistence, or a title list UI.
- Renaming panes/tabs while disconnected from the coordinator.

## Product rules

Titles are labels only. Every admitted member may rename every pane and tab, including panes they
do not host. Existing host-only pane deletion and all-host tab deletion rules do not apply.

The authoritative stored value is `Option<String>`:

- `None` means no custom title and renders the ordinal fallback: `Pane #N` or `Tab #N`.
- `Some(title)` is a normalized, non-empty custom title and replaces that ordinal label in chrome.
- On confirm, trim leading/trailing Unicode whitespace. An empty result clears the custom title
  (`None`). This means a blank or whitespace-only submission restores the ordinal fallback.
- A non-empty title is at most 32 Unicode scalar values and contains no control characters.

This deliberately mirrors the existing display-name length/control-character safety boundary while
giving title input its distinct empty-means-clear behavior. It does not reuse
`config::validate_display_name`, because that helper rejects empty values and lives in a CLI config
module rather than the shared layout model.

## Interaction model

### Entering rename

Pane mode gains `e` and tab mode gains `e`; `r` remains the pane-mode right split command.

| Sequence | Result |
|---|---|
| `Ctrl+P`, `e` | Rename the pane focused when `e` is pressed. |
| `Ctrl+T`, `e` | Rename the tab current when `e` is pressed. |

Opening the prompt consumes `e`, exits sticky chord mode, clears any text selection/resize drag as
appropriate, and snapshots the target ID. Later local focus/tab changes must not retarget an open
prompt.

### Rename prompt

Rename is a first-class TUI modal mode, mutually exclusive with the agents overlay and chord modes.
Render a small centered floating `Clear` + bordered panel over the current layout, rather than
overloading the footer. It contains a target-specific title (`Rename pane` / `Rename tab`), a
single-line editable field, and concise help: `Enter save · Esc cancel`. The underlying layout
remains visible but cannot receive interaction.

- Prefill the field with the target's existing custom title; an untitled target starts empty. Do
  not prefill `Pane #N` or `Tab #N`, because those are generated fallback labels rather than saved
  data.
- `Char` input with no control/alt modifier appends the typed Unicode character (including shifted
  printable characters). Backspace removes one Unicode scalar. `Delete`, cursor movement, paste,
  selection, and multiline editing are out of scope for v1; unsupported keys are consumed.
- Enter normalizes and validates the field. If valid, close the prompt immediately and emit one
  rename intent/request. Empty is valid and clears. If invalid, keep the prompt open, do not send
  a request, and render a local message (max 32 characters / no control characters).
- Esc closes and discards all edits, with no request and no PTY forwarding.
- The prompt never expires. The two-second `CHORD_IDLE_TIMEOUT` applies only to `ChordMode::Pane`
  and `ChordMode::Tab`, never to rename.

### Modal precedence and global keys

The event dispatcher must handle the active modal before sticky chords, focus navigation, mouse
selection, or normal PTY forwarding.

- While rename is open, all keys are consumed. `Ctrl+P` and `Ctrl+T` do not switch modes; `Ctrl+A`
  does not open/toggle the agents overlay; arrows and mouse do not navigate/focus/select/resize.
- `Ctrl+Q` retains its existing global detach/quit priority. It closes/discards the prompt, clears
  chord state, and is never sent to a PTY.
- If the agents overlay is open, it remains modal and no rename chord can start. A rename prompt
  cannot be opened over it. Conversely, once rename is open, overlay toggling is disabled until
  the prompt closes.
- A layout commit may arrive while the prompt is open. Keep its target ID; on Enter the coordinator
  is the authority. If the target disappeared or the revision became stale, the request is rejected
  and normal rejection status is shown after the prompt closes.

## Chrome and layout behavior

`Pane.title` and `Tab.title` live in the authoritative `SessionState` snapshot, not in local TUI
state. Pane and tab order remains the source of ordinal fallback numbering. A title mutation only
changes the title and revision; it never changes roots, pane hosting, PTY grids, leases, focus, or
membership.

- Tab bar label: custom title when present, otherwise `Tab #N`. Keep the current active-tab color
  treatment and clickable tab hit-testing.
- Pane border label: custom title when present, otherwise `Pane #N`, followed by the existing
  `host: … control: …` metadata.
- Apply a display-width-aware, single-line ellipsis helper at each render boundary. Never emit
  controls/newlines (the model already rejects controls). Tab labels are clipped to their allocated
  tab-bar width; reserve the active-tab padding and show an ellipsis when it fits. Pane labels are
  clipped to the block title's available width after reserving host/control text; if chrome is too
  narrow, host/control may clip normally but title rendering must not overflow adjacent borders.
- Titles in snapshots naturally survive a client detach/resume while the headless session lives.
  They are intentionally not written to finder records and cannot restore a dead session.

## Wire contract and compatibility

This changes both `LayoutState` descriptors and `LayoutRequest` wire shape. Bump
`PROTOCOL_VERSION` from **5 to 6**; mixed v5/v6 peers fail the existing version handshake instead
of silently ignoring titles/actions.

Use the following Prost fields. Tags are additive and must never be renumbered or reused.

```rust
pub struct PaneDescriptor {
    // existing tags 1..=4
    #[prost(string, optional, tag = "5")]
    pub title: Option<String>,
}

pub struct TabDescriptor {
    // existing tags 1..=2
    #[prost(string, optional, tag = "3")]
    pub title: Option<String>,
}

pub struct LayoutRequest {
    // existing tags 1..=8
    #[prost(message, optional, tag = "9")]
    pub rename_pane: Option<RenamePane>,
    #[prost(message, optional, tag = "10")]
    pub rename_tab: Option<RenameTab>,
}

pub struct RenamePane {
    #[prost(uint64, tag = "1")]
    pub pane_id: u64,
    #[prost(string, tag = "2")]
    pub title: String, // empty clears
}

pub struct RenameTab {
    #[prost(uint64, tag = "1")]
    pub tab_id: u64,
    #[prost(string, tag = "2")]
    pub title: String, // empty clears
}
```

`Option<String>` on descriptors preserves the semantic distinction between an absent custom title
and a present custom title. Requests use a required string because empty is the explicit clear
operation. The coordinator/layout normalization converts that empty string to `None` before
committing, so committed descriptors never carry `Some("")`.

Protocol validation must count exactly one of all eight layout actions; validate nonzero target IDs
and title byte length no greater than a new `MAX_LAYOUT_TITLE_BYTES = 128` before allocation/use.
The layout authority then applies the normative Unicode scalar-count (≤32), control-character, and
trim/empty normalization rule. Descriptor/snapshot validation applies the same title validation:
`None` is valid, `Some` must already be normalized, non-empty, ≤32 scalars, and control-free.
This two-layer split keeps hostile wire inputs bounded while one pure layout helper defines the
product rule.

No new reject enum is needed. Coordinator outcomes are:

| Case | Result |
|---|---|
| Unknown pane/tab ID | `LayoutRejectReason::UnknownId` |
| Sender not an admitted member | existing `NotHost` mapping for `LayoutError::NotMember` |
| Invalid title or malformed/multiple action request | `Malformed` |
| Request revision does not equal state revision | `Stale` |
| Valid rename | authoritative revision advances and `LayoutCommit` broadcasts title-bearing state |

Unlike delete/grid operations, rename performs no pane-host/tab-host authorization check. A pending
creation reservation should use the existing `ensure_no_reservation` structural-mutation gate so
rename ordering/revision behavior stays consistent with other layout requests.

## Testing

- Layout: normalization; empty/whitespace clear; 32-scalar boundary; overlong/control rejection;
  any admitted member can rename a remote-hosted pane/tab; unknown IDs and stale revisions reject;
  snapshot validation rejects `Some("")` and invalid titles; commits preserve titles.
- Protocol: v6 assertion; descriptor encode/decode preserves optional titles; rename action tags 9
  and 10 round-trip; empty clear is valid; zero IDs, over-128-byte titles, and mixed actions fail.
- Session: coordinator dispatches both actions, maps each rejection correctly, broadcasts commits,
  and a late join snapshot carries pane/tab titles.
- TUI: `Ctrl+P e` / `Ctrl+T e` target the expected IDs and expose `e` in chord recognition/footer;
  prefill; character and Backspace editing; Enter request; blank clear; invalid stays open; Esc
  cancel; no chord timeout; modal key/mouse/PTY suppression; Ctrl+Q priority; overlay mutual
  exclusion; title/fallback render and narrow-width ellipsis.

Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and
`cargo test` after the implementation tasks.

## Success criteria

1. Any member can share-rename the focused pane or current tab using lowercase `e` in the matching
   sticky chord, while `r` remains right split.
2. Rename is an accessible, non-expiring modal; Enter commits, Esc cancels, and no prompt keys
   leak to a PTY.
3. Blank confirmation restores exactly the ordinal pane/tab labels.
4. Titles converge through coordinator commits, late join snapshots, and local detach/resume.
5. Invalid, stale, or missing targets fail safely without partial local mutation.
6. v5 and v6 peers never interoperate silently; all tests and quality gates pass.
