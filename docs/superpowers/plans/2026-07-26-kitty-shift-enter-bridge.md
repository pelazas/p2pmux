# Kitty Shift+Enter bridge (per-pane)

Keep p2pmux as a PTY host. Add a minimal per-pane Kitty keyboard bridge so agents get Shift+Enter newlines.

## Decisions (locked)

1. Respond to `CSI ? u` with current virtual-pane flags (`CSI ? 0 u` initially; after push of flag 1, `CSI ? 1 u`). Track/support only flag `1` (`DISAMBIGUATE_ESCAPE_CODES`); mask unsupported flags. `active()` means flag 1 is enabled.
2. Replicate `kitty_keyboard_active` on snapshot/delta as additive protobuf bools without bumping `PROTOCOL_VERSION` (stays 4). Absent fields decode as false → LF fallback.
3. Query replies are written only by the local PTY owner, never via leases/peer input. Encode: Shift+Enter → `ESC[13;2u` when active, else `LF`. Plain Enter stays `\r`.

## Tasks (one commit each)

### Task 1: Extend tracker into query bridge
- Files: `src/kitty_keyboard.rs`
- Add supported-flags bit `1`; parse `CSI ? u`; handle `=` optional `;mode`; mask unsupported flags; `take_query_reply()`; `active()` = flag 1 enabled.
- Tests: initial query `\x1b[?0u`; after `\x1b[>1u` query `\x1b[?1u`; split query; unsupported flags inactive; push/pop still works.
- Commit: `Add kitty keyboard query replies`

### Task 2: Reply to queries from local PTYs
- Files: `src/screen.rs`, `src/tui.rs`, `tests/screen.rs`
- HostScreen drains pending reply after `process_pty`. `run_local`, `SharedLocalPane::drain`, `run_host` write replies to that PtyHost only. Outer enhancement unchanged.
- Tests: host observes `CSI ? u`, exposes `\x1b[?0u`, inactive until push.
- Commit: `Reply to kitty queries from local PTYs`

### Task 3: Propagate mode in screen frames
- Files: `src/screen.rs`, `src/protocol.rs`, `src/session.rs`, `tests/screen.rs`, `tests/protocol.rs`, `tests/session_stream.rs`
- Add `kitty_keyboard_active` to `ScreenFrame` and Snapshot/Delta (new tags). Keep PROTOCOL_VERSION 4. GuestScreen stores received flag.
- Tests: flag survives snapshot/delta and clears on pop; protocol round-trip true; stream carries true after `\x1b[>1u`.
- Commit: `Propagate kitty mode in screen frames`

### Task 4: Use mode for remote key forwarding
- Files: `src/tui.rs`
- Replace remote `encode_key(..., false)` with pane’s `kitty_keyboard_active()`. Apply flag from snapshot/delta in remote drain and `run_guest`.
- Tests: remote active → CSI-u; inactive → LF; Enter always CR.
- Commit: `Use kitty mode for remote key forwarding`

### Task 5: End-to-end PTY regression
- Files: `src/tui.rs` and/or integration test, `tests/fixtures/agent_kitty_keyboard_probe.js`
- Probe: emit `CSI ? u`, expect `\x1b[?0u`, push `\x1b[>1u`, succeed only on `CSI 13;2u`. Keep existing LF probe.
- Final gate: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`
- Commit: `Cover kitty Shift-Enter bridge end to end`

## Constraints
- Minimal bridge only; no full emulator rewrite
- No competing global encodings
- Extend KittyKeyboardTracker; surgical diffs
- Commit after each task succeeds (tests for that task green)
