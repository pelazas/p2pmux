# Option Focus and Directional Pane Splits Implementation Plan

**Goal:** Add Option/Alt-arrow pane focus and directional `Ctrl+P` splits while
making split placement authoritative through the layout protocol.

**Architecture:** Store a first/second placement enum in the layout reservation
and apply it when `pane_ready` builds the split. Carry that value through the
protocol and runtime; TUI key handling only chooses it. Preserve all current
grid sizing, focus, and split-depth rules.

**Spec:** `docs/superpowers/specs/2026-07-25-option-focus-directional-splits-design.md`

## File map

| File | Responsibility |
|---|---|
| `src/layout.rs` | Placement enum, reservation state, child order |
| `src/protocol.rs` | CreatePane field, validation, protocol v3 |
| `src/session.rs` | Map protocol placement into layout reservation |
| `src/tui.rs` | Alt focus, directional intents, footer help |
| `tests/*` | Layout, protocol, session, and wire compatibility coverage |
| `README.md` | User-facing keybinding and terminal caveat |

## Tasks

1. Add this design and plan documentation.
2. Add `NewPanePosition` to the layout model; default missing placement to
   `Second`; test both child orders and depth rejection.
3. Add a `CreatePane` protobuf placement field, reject unknown values, bump the
   protocol to v3, and map it through session coordination.
4. Add exact Alt-arrow focus in normal and pane modes and `r/l/d/u` pane-chord
   intents, retaining sticky mode and existing `n` behaviour.
5. Update normal and pane footer segments and their rendering tests.
6. Document the bindings and Option-arrow terminal/mux caveat in the README.

## Verification

Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
Confirm six focused commits on `ux/option-focus-directional-splits` and no
remote push or PR.
