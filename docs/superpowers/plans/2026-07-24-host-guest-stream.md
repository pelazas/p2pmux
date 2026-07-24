# Spike 2 Host/Guest Screen and Control Implementation Plan

> **For agentic workers:** REQUIRED: Use `superpowers:subagent-driven-development` (if subagents are available) or `superpowers:executing-plans` to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the existing authenticated Iroh `create`/`join` handshake into one fixed-grid, single-pane host terminal that streams canonical vt100 snapshots/deltas to a guest, renders remotely, and grants one epoch-checked input controller at a time without screen backlog blocking typing.

**Architecture:** The host TUI continues to own the PTY and canonical `vt100::Parser`. A new pure screen codec turns its fixed-size `Screen` into an opaque protocol `Snapshot` payload and computes protocol `Delta` payloads using vt100’s `state_formatted`/`state_diff`; the guest owns a separate parser that only applies those decoded payloads. After `Welcome`, each peer uses two independent QUIC bi-streams: a host-to-guest screen stream with a lossy one-value `watch` update source, and a bidirectional control stream with bounded high-priority control queues. A pure lease state machine, owned by the host TUI, serializes ownership and accepts PTY bytes only when the authenticated sender and lease epoch both match.

**Tech Stack:** Rust 2024; Tokio 1.x (`sync` feature); Iroh 1.0.3 QUIC streams; existing Prost protocol; portable-pty; vt100 0.16; ratatui/crossterm; macOS CI and deterministic localhost tests.

---

## Scope guard

- Stay on `spike1/local-terminal`. Do not create/switch branches, merge, or open a PR. Keep the existing worktree clean except for the files in this plan.
- Commit in this exact order: this plan, milestone 10, milestone 11, milestone 12, milestone 13. Push only `origin/spike1/local-terminal` after final verification.
- Preserve `p2pmux local` exactly: one local PTY, fixed startup grid, existing Ctrl-Q behavior, no networking requirement.
- Implement exactly one host-created pane with the constant opaque ID `b"default-pane"`. No tabs, splits, layout tree, presence roster, coordinator failover, admission cap, resize, session bootstrap tree, or pane registry belongs in Spike 2.
- The PTY grid is immutable. Snapshot payloads carry the host’s initial rows/columns; guest window resizing only crops/letterboxes the remote fixed grid. Do not add a `Resize` message or call `MasterPty::resize`.
- Do not change the existing protobuf schema or size limits. `Snapshot.screen` and `Delta.changes` remain opaque to `protocol`; their exact contents are private to `screen.rs`.
- Keep all interactive assertions manual. Automated tests must use `TestBackend`, a `/bin/sh` PTY only where needed, and Iroh `presets::Minimal` loopback endpoints with relay disabled; no test may require DNS, a relay, real terminal, or timing greater than the existing five-second network bound.
- The Iroh TLS connection identity is authoritative. Every post-Welcome envelope must claim the same sender peer ID as `Connection::remote_id()`; reject mismatched claimed IDs before dispatch. For `TakeControl`, require `requester_peer_id` to equal that authenticated peer as well.

## Locked post-Welcome stream contract

The current Join/Welcome stream remains a one-frame-per-direction handshake and both halves are finished after `Welcome`. Keep the authenticated Iroh `Connection` alive, then establish streams in this exact order so no stream needs a new wire variant or magic prelude:

```text
Host                              Guest
----                              -----
Join/Welcome bi-stream             Join/Welcome bi-stream
open screen bi-stream  ----------> accept screen bi-stream
                                   open control bi-stream
accept control bi-stream <-------- control bi-stream

screen SendStream -> guest RecvStream: Snapshot | Delta only
control guest SendStream -> host RecvStream: Input | TakeControl only
control host SendStream -> guest RecvStream: ControlLease only
```

`FrameReader` incrementally reconstructs exactly one existing length-delimited protocol frame from a `RecvStream`, retaining at most `MAX_FRAME_BYTES` for the in-progress frame; it does not use `read_to_end` for long-lived streams. `FrameWriter` encodes one validated `Envelope` then `write_all`s it without finishing until shutdown. A bad frame, wrong direction/body, wrong pane/host ID, or mismatched authenticated sender closes only that peer connection; it never blocks or terminates the PTY.

### Fixed screen payload format

`src/screen.rs` owns this version-1 binary wrapper, not `src/protocol.rs`:

```text
Snapshot.screen = [0x01 codec version][rows: u16 big-endian][cols: u16 big-endian][vt100 state_formatted bytes]
Delta.changes   = vt100 state_diff bytes
```

The host starts sequences at 1. A snapshot has `sequence = N`; a delta has `base_sequence = N - 1`, `sequence = N`. A guest replaces its parser on every snapshot using the encoded host dimensions, processes the state bytes, and records `N`; it only processes a delta when local sequence equals `base_sequence`. A mismatch marks the remote frame stale and leaves its last good render intact until a fresh snapshot arrives.

The host retains a full snapshot beside every coalesced update. A peer screen writer sends a fresh snapshot initially, whenever its `watch::Receiver` skipped an intermediate state (`last_sent_sequence != update.base_sequence`), and as a 500 ms snapshot heartbeat while the peer is connected. Otherwise it sends the smaller delta. Thus a slow/dropped screen update is repaired by a snapshot without any Resync protocol message. If a computed payload exceeds its existing protocol cap, log the error, keep PTY processing, and retry on the next update/heartbeat; do not enqueue an oversize frame.

### Queue and priority design (done-when contract)

| Path | Mechanism and capacity | Producer behavior | Consumer behavior / guarantee |
| --- | --- | --- | --- |
| Host PTY → canonical screen | Existing PTY reader plus host TUI loop; bounded 64 chunks / ~4 ms drain | Always process PTY bytes into the canonical parser. | Never waits for a peer or network write. |
| Canonical screen → each peer | `tokio::sync::watch::Sender<ScreenFrame>`; one latest value, each peer has a receiver | `send_replace` is synchronous and overwrites stale screen state. A `ScreenFrame` holds `Arc<[u8]>` snapshot and delta bytes plus sequence metadata. | A slow screen writer receives the newest state and sends a snapshot after any gap. Screen updates are intentionally lossy/coalesced. |
| Guest typing/control → control stream | Two bounded `mpsc` queues: `take_control` capacity 16 and `input` capacity 256 envelopes; `GuestControlWriter` polls with `biased` priority for TakeControl before Input | The guest TUI does non-blocking `try_send`; when the input queue is full it retains already-encoded input in a local bounded pending deque and keeps draining it before reading more terminal events. Surface a compact `input congested` status instead of silently dropping bytes. | This writer owns only the control `SendStream`; it is never scheduled behind snapshots/deltas. |
| Host control stream → host TUI | Bounded `mpsc::channel<HostControlEvent>(256)` | The per-peer reader awaits capacity only behind other control requests, after authenticating each envelope. | The host TUI drains it before/after PTY chunks; screen writers never share this queue. |
| Host lease state → each peer | `watch::Sender<LeaseState>`; one latest value | Host lease transitions use `send_replace`. | Each control-state writer immediately sends the current `ControlLease`, then the latest transition; stale lease notices are harmless because epochs increase. |

There is deliberately no multiplexed “outbound message queue.” The control and screen paths use distinct QUIC streams, distinct writer tasks, and distinct channels. A screen writer may be blocked indefinitely in a fat `Snapshot` write; the guest’s control writer, host’s control reader, lease broadcaster, host TUI, and PTY reader remain runnable.

### Lease state machine

`src/lease.rs` owns no I/O and takes `Instant` as an argument so its tests need no sleeps. The first controller is the host peer at epoch 1. `IDLE_AFTER` is `Duration::from_secs(8)`; no separate “idle” message is sent.

| Event | Preconditions | Result |
| --- | --- | --- |
| `Input { sender, lease_epoch, data }` | `sender == controller`, `lease_epoch == epoch`, `data` nonempty | Return `Accept(data)` and set `last_activity = now`. |
| Input with any other sender or epoch | — | Return `RejectStaleInput`; do not write to PTY and do not change activity/epoch. |
| `TakeControl { sender, known_epoch }` while idle | `known_epoch == epoch`, `now - last_activity >= 8s` | Grant the sender, increment epoch, publish `ControlLease`. This is the invisible “hop in by typing” path. |
| Explicit `TakeControl` while active | `known_epoch == epoch` | Grant the sender, increment epoch, publish `ControlLease`. This is the visible handoff path; no confirmation/ACL exists in this trusted Spike. |
| TakeControl with a stale known epoch | — | Return `RejectStaleRequest`; retain lease. |
| Local host typing | Host has current lease → inject directly; otherwise use the same idle hop-in/explicit-take rules as a guest | Host does not bypass the lease. |

Guest UX is intentionally minimal: the footer names the controller and says `idle` or `typing`. If someone else is idle, the first ordinary encoded key queues a TakeControl and holds that key until the next matching `ControlLease`, then queues it as `Input` with the newly announced epoch. If someone is active, ordinary keys are not sent; `Ctrl-T` sends explicit TakeControl. `Ctrl-Q` remains a local p2pmux exit and is never forwarded. The host uses the same Ctrl-T binding when it is not controller. Only the controller’s normal key/paste input reaches the PTY.

## Planned file structure

| Path | Responsibility |
| --- | --- |
| `docs/superpowers/plans/2026-07-24-host-guest-stream.md` | This implementation plan; first, documentation-only commit. |
| `Cargo.toml`, `Cargo.lock` | Enable Tokio’s `sync` feature; no new runtime/network crates. |
| `src/screen.rs` | Fixed-grid snapshot envelope codec, host sequence/delta producer, guest snapshot/delta applier, and pure screen errors. |
| `src/lease.rs` | Pure one-controller lease state machine and transition/rejection types. |
| `src/transport.rs` | Incremental long-lived framed stream reader/writer in addition to the retained one-frame handshake helpers. |
| `src/session.rs` | Handshake-to-two-stream upgrade, per-peer screen/control/lease tasks, authenticated post-Welcome dispatch, and graceful peer shutdown. |
| `src/tui.rs` | Reusable fixed-grid renderer/input encoding, host TUI integration, guest remote renderer/control UX, terminal cleanup, and non-interactive renderer tests. |
| `src/cli.rs` | `create` host PTY/TUI plus Iroh accept loop; `join` guest session/TUI lifecycle; trust warning remains unchanged. |
| `src/lib.rs` | Export the new `lease` and `screen` module boundaries. |
| `tests/screen.rs` | Pure snapshot/delta encoding, grid, sequence, and cap regression tests. |
| `tests/lease.rs` | Clock-controlled lease transition, stale-epoch, and input-rejection tests. |
| `tests/session_stream.rs` | Loopback post-Welcome screen/control integration and stream-direction/authentication tests. |
| `tests/queue_priority.rs` | Deterministic screen coalescing and control-priority/non-blocking tests with no Iroh timing dependency. |
| `tests/module_surface.rs`, `tests/cli.rs`, `tests/session_handshake.rs`, `tests/transport.rs`, `tests/protocol.rs`, `tests/pty_host.rs` | Extend only where needed; retain all existing contract coverage. |
| `README.md` | Replace handshake-only wording with Spike 2 dogfood commands and fixed-grid/control caveats. |

`src/protocol.rs`, `src/pty_host.rs`, `src/ticket.rs`, `docs/MVP_DESIGN.md`, `docs/SPIKE_PLAN.md`, and `.github/workflows/ci.yml` need no functional change. Do not add layout, resize, or new protobuf messages.

## Chunk 1: Baseline and milestone 10 — host PTY to Snapshot/Delta stream

### Task 1: Commit this plan only

**Files:**

- Create: `docs/superpowers/plans/2026-07-24-host-guest-stream.md`

- [ ] **Step 1: Confirm the continuation baseline.**

Run:

```bash
git branch --show-current
git status --short
git diff --check
git log --oneline -5
```

Expected: branch is `spike1/local-terminal`; status contains only `?? docs/superpowers/plans/2026-07-24-host-guest-stream.md`; diff check is silent. Stop if any other tracked/untracked work exists—do not clean, stash, reset, switch, or overwrite it.

- [ ] **Step 2: Commit the plan without source changes.**

```bash
git add docs/superpowers/plans/2026-07-24-host-guest-stream.md
git commit -m "docs: plan host guest stream"
```

Expected: documentation-only commit; do not push yet.

### Task 2: Add pure fixed-grid screen state, encoding, and sequencing

**Files:**

- Modify: `src/lib.rs:3-9`
- Create: `src/screen.rs`
- Create: `tests/screen.rs`

- [ ] **Step 1: Write failing, deterministic screen tests.**

Create `tests/screen.rs` around these public contracts (tests may use a 2×3 parser and compare every visible cell’s contents/colors/modifiers plus cursor/mode state):

```rust
pub const SCREEN_CODEC_VERSION: u8 = 1;

pub struct ScreenFrame {
    pub sequence: u64,
    pub base_sequence: u64,
    pub snapshot: Arc<[u8]>,
    pub delta: Arc<[u8]>,
}

pub struct HostScreen { /* canonical parser, previous Screen, next sequence */ }
impl HostScreen {
    pub fn new(rows: u16, cols: u16) -> Result<Self, ScreenError>;
    pub fn process_pty(&mut self, bytes: &[u8]) -> Result<ScreenFrame, ScreenError>;
    pub fn current_frame(&self) -> &ScreenFrame;
    pub fn screen(&self) -> &vt100::Screen;
}

pub struct GuestScreen { /* parser plus Option<u64> */ }
pub enum ApplyDelta { Applied, NeedsSnapshot }
impl GuestScreen {
    pub fn new() -> Self;
    pub fn apply_snapshot(&mut self, sequence: u64, payload: &[u8]) -> Result<(), ScreenError>;
    pub fn apply_delta(&mut self, base_sequence: u64, sequence: u64, payload: &[u8]) -> Result<ApplyDelta, ScreenError>;
    pub fn screen(&self) -> Option<&vt100::Screen>;
    pub fn sequence(&self) -> Option<u64>;
}
```

Cover all of the following before implementation:

1. Initial frame is a nonzero Snapshot at sequence 1; its payload has exact version/rows/cols header and reproduces an empty parser at the host dimensions.
2. `process_pty` after styled text creates a frame where a fresh guest snapshot renders exactly like the host; then a second change’s delta applied after the first snapshot produces the host’s final cells and input modes.
3. Applying a delta before any snapshot, with a wrong base sequence, or with `sequence <= base_sequence` returns `NeedsSnapshot`/an error without mutating the last good guest screen.
4. Applying a later fresh snapshot replaces rather than accumulates parser state, including alternate-screen/cursor/input modes.
5. Malformed/truncated header, unknown codec version, zero rows/columns, and snapshot/delta payloads above `MAX_SNAPSHOT_BYTES`/`MAX_DELTA_BYTES` are rejected deterministically.

- [ ] **Step 2: Run the tests to prove the contract is absent.**

Run:

```bash
cargo test --test screen
```

Expected: compile failure because `screen` is not exported and the APIs do not exist.

- [ ] **Step 3: Implement the codec without leaking it into `protocol`.**

Create `src/screen.rs` with a manual `ScreenError` (`Display` + `Error`) covering malformed wrapper, invalid dimensions, over-cap payload, and invalid sequence. `HostScreen::new` makes the canonical `vt100::Parser`, clones its initial `Screen`, and materializes sequence 1’s full snapshot. `process_pty` processes bytes, increments the sequence with checked arithmetic, calls `screen.state_formatted()` for the complete wrapper and `screen.state_diff(&previous)` for the delta, validates size limits before allocating `Arc<[u8]>`, then clones the new screen as `previous`.

The full snapshot wrapper is exactly one version byte, two big-endian `u16`s, then `state_formatted`; it must be large enough to reset a fresh guest parser, not merely `contents()`. `GuestScreen::apply_snapshot` parses the wrapper first, creates a new parser at host dimensions with no scrollback, processes the state data only after validation, then atomically replaces parser/sequence. `apply_delta` validates sequencing before `process`, never resizes a parser, and never uses terminal text as a screen format.

Export `pub mod screen;` from `src/lib.rs`. Do not add serialization dependencies, change protocol validation, touch PTY I/O, or create a network task in this step.

- [ ] **Step 4: Verify the pure screen path.**

Run:

```bash
cargo fmt --all
cargo test --test screen
cargo test --test protocol
cargo test --test pty_host
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all exit 0; all screen tests are time-free.

### Task 3: Add incremental framed stream I/O and host screen fan-out

**Files:**

- Modify: `Cargo.toml:14`
- Modify: `Cargo.lock`
- Modify: `src/transport.rs:1-165`
- Modify: `src/session.rs:1-246`
- Create: `tests/session_stream.rs`

- [ ] **Step 1: Write failing long-lived stream tests.**

In `tests/session_stream.rs`, use the existing relay-disabled loopback transport helper. Add tests that:

1. Write three `ControlLease` frames through one `FrameWriter`, read them one by one through one `FrameReader`, and prove the writer does not finish after the first frame.
2. Split a valid encoded frame over multiple chunks and concatenate two frames in one chunk through a testable byte-buffer helper; prove exactly one frame is yielded per call and the remaining bytes are retained. Reject an oversize prefix before a large allocation and reject EOF with an incomplete frame.
3. Establish a real post-Welcome peer with a seeded `watch::Receiver<ScreenFrame>`. Assert its first screen message is Snapshot sequence 1 and its next non-skipped update is Delta with the expected base/sequence; the peer must reject a screen message whose host ID or pane ID differs from the default host/pane.

Keep the tests under the existing five-second bounds and use only tiny screen payloads.

- [ ] **Step 2: Confirm the new stream tests fail.**

Run:

```bash
cargo test --test session_stream
```

Expected: compile failure because long-lived framed helpers and post-Welcome host serving are absent.

- [ ] **Step 3: Add the transport boundary for a long-lived framed stream.**

Enable Tokio `sync` only by changing its existing features to include `"sync"`; do not add `io-util`, futures, bytes, or a new protocol crate.

In `src/transport.rs`, retain `read_frame`/`write_frame` for the finished handshake stream and add:

```rust
pub struct FrameReader { /* RecvStream + bounded pending bytes */ }
pub struct FrameWriter { /* SendStream */ }

impl Transport {
    pub async fn open_framed_bi(&self, connection: &Connection) -> Result<(FrameWriter, FrameReader), TransportError>;
    pub async fn accept_framed_bi(&self, connection: &Connection) -> Result<(FrameWriter, FrameReader), TransportError>;
}
impl FrameReader {
    pub async fn read_next(&mut self) -> Result<Option<Envelope>, TransportError>;
}
impl FrameWriter {
    pub async fn write_next(&mut self, envelope: &Envelope) -> Result<(), TransportError>;
    pub fn finish(self) -> Result<(), TransportError>;
}
```

`FrameReader` uses bounded `RecvStream::read_chunk` reads and a private parser that recognizes the protobuf varint prefix, waits for only the declared payload, hands the exact frame to existing `decode_frame`, then drains only that frame from pending bytes. Before appending, enforce the `MAX_FRAME_BYTES` hard ceiling on prefix + declared payload; EOF returns `Ok(None)` only with an empty pending buffer and otherwise a new `TransportError::TruncatedStreamFrame`. `FrameWriter::write_next` calls existing `encode_frame` and timeouts only its own stream write; it never calls `finish` per frame. Extend manual error variants/sources accordingly.

- [ ] **Step 4: Upgrade the host handshake into per-peer screen service.**

Keep `HostSession::create`, ticket behavior, and existing `accept_one_join` test surface. Add private/default-pane constants in `session.rs` and these public orchestration types:

```rust
pub const DEFAULT_PANE_ID: &[u8] = b"default-pane";

pub struct HostPaneChannels {
    pub pane_id: Vec<u8>,
    pub host_peer_id: Vec<u8>,
    pub screen_rx: watch::Receiver<ScreenFrame>,
    pub lease_rx: watch::Receiver<LeaseState>,
    pub control_tx: mpsc::Sender<HostControlEvent>,
}

pub enum HostControlEvent {
    Input { peer_id: Vec<u8>, input: Input },
    TakeControl { peer_id: Vec<u8>, request: TakeControl },
}

impl HostSession {
    pub async fn serve_peer(&self, incoming: Incoming, pane: HostPaneChannels) -> Result<(), SessionError>;
}
```

Factor the existing Join/Welcome identity validation into a helper that returns the authenticated `Connection` and peer ID. `serve_peer` performs the locked stream ordering: send/finish Welcome, open a screen framed bi-stream, wait for the guest’s control framed bi-stream, then run three peer-local tasks under a cancellation/select boundary:

- screen writer: sends initial Snapshot, then Delta only when contiguous, fresh Snapshot after a watched gap, and a Snapshot heartbeat every 500 ms;
- control reader: accepts only Input/TakeControl from its authenticated remote ID and expected default pane, verifies protocol sender/requester claims, then awaits the bounded host control channel;
- lease writer: sends current ControlLease immediately and each latest watched transition, validating host/pane IDs on construction.

Do not let a peer screen send failure reach the host PTY/TUI. On any peer task failure, close that one connection and drop its local watchers; preserve the accepting host and all other peers.

- [ ] **Step 5: Verify milestone 10 host streaming.**

Run:

```bash
cargo fmt --all -- --check
cargo test --test screen
cargo test --test session_stream
cargo test --test session_handshake
cargo test --test transport
cargo test --test protocol
cargo test --test pty_host
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all pass; post-Welcome screen delivery is loopback-only and no test launches crossterm.

- [ ] **Step 6: Commit milestone 10.**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/screen.rs src/transport.rs src/session.rs tests/screen.rs tests/session_stream.rs
git commit -m "feat: stream host screen snapshots and deltas"
```

Expected: one host pipeline commit. It streams canonical screen state but does not yet present a guest TUI or inject control input.

## Chunk 2: Milestone 11 — guest remote rendering

### Task 4: Build the guest session receiver and fixed-grid remote TUI

**Files:**

- Modify: `src/session.rs`
- Modify: `src/tui.rs`
- Modify: `src/cli.rs`
- Modify: `tests/session_stream.rs`
- Modify: `tests/cli.rs`

- [ ] **Step 1: Write failing guest receive/render tests.**

Add to `tests/session_stream.rs` an end-to-end loopback test in which the host feeds snapshot then delta frames and `join_pane` yields guest events in order. Assert the guest reader rejects a Delta with a wrong base by forwarding `GuestEvent::ScreenGap` rather than poisoning the receiver, and that the next Snapshot event is delivered for recovery.

In `src/tui.rs`’s existing `#[cfg(test)]` module, add a `TestBackend` test that applies a remote snapshot/delta to `GuestScreen`, renders `VtScreen`, and proves host dimensions are fixed: a larger guest viewport leaves trailing blank cells and a smaller one crops upper-left. Add a footer test that shows `controller: <short peer> idle|typing` without altering any remote cell. Existing local renderer/input tests must remain unchanged.

In `tests/cli.rs`, update only static help/output assertions needed for `join`’s changed, live terminal-sharing description; do not process-test an interactive `join`.

- [ ] **Step 2: Confirm tests fail.**

Run:

```bash
cargo test --test session_stream
cargo test tui::tests
cargo test --test cli
```

Expected: guest session events/runtime and remote TUI entry point are absent.

- [ ] **Step 3: Add the guest post-Welcome runtime.**

In `src/session.rs`, replace the one-shot `join_once` implementation internally with a reusable authenticated handshake helper; retain `join_once` as the existing smoke wrapper that performs handshake and closes immediately so `tests/session_handshake.rs` stays meaningful. Add:

```rust
pub enum GuestEvent {
    ScreenSnapshot(Snapshot),
    ScreenDelta(Delta),
    ScreenGap { expected_base: Option<u64>, received_base: u64 },
    Lease(ControlLease),
    Disconnected,
}

pub struct GuestPane {
    pub pane_id: Vec<u8>,
    pub host_peer_id: Vec<u8>,
    pub events: mpsc::Receiver<GuestEvent>,
    pub controls: GuestControlSender,
    // private cancellation/connection/task ownership
}

pub async fn join_pane(transport: Transport, ticket: JoinTicket) -> Result<GuestPane, SessionError>;
impl GuestPane { pub async fn shutdown(self); }
```

After authenticated Welcome, `join_pane` accepts the host-opened screen stream, opens the guest control stream, and starts independent tasks: the screen reader validates sender/pane/host/direction and emits Snapshot/Delta or ScreenGap; the control reader accepts only host `ControlLease`; and the control writer is added in milestone 12 but its receiver/channel is constructed now. Event delivery is a bounded 128-message channel; screen events use `try_send` and coalesce into a pending fresh-snapshot marker when full, whereas Lease and Disconnected are delivered reliably. Dropping/shutting down `GuestPane` cancels tasks, finishes streams, closes the connection, then closes the endpoint.

- [ ] **Step 4: Refactor the TUI renderer into local and remote modes.**

Keep `run_local` public and behavior-identical. Make `VtScreen` and its screen render helper reusable within `tui.rs`; add a private footer renderer and a public synchronous entry point:

```rust
pub fn run_guest(pane: GuestPane) -> Result<(), Box<dyn Error>>;
```

`run_guest` uses the same raw-mode/alternate-screen/RAII cleanup as local mode but owns no `PtyHost`. It drains `GuestEvent`s every frame, applies snapshot/delta through `GuestScreen`, keeps the last good remote screen on ScreenGap, draws the fixed host grid plus the one-line lease footer, and ignores `Event::Resize`. It intercepts Ctrl-Q locally. Until milestone 12, it displays spectator state and deliberately does not forward keys/paste.

- [ ] **Step 5: Wire the interactive guest lifecycle without blocking Tokio.**

In `cli.rs`, keep trust-warning printing and ticket parsing exactly as today. `join` must call `join_pane`, pass its `GuestPane` into `tokio::task::spawn_blocking(|| tui::run_guest(...))`, then call `GuestPane::shutdown`/await its task cleanup after Ctrl-Q, PTY host disconnect, or terminal error. Do not run crossterm work on a Tokio worker thread. Update the success text so it no longer says terminal sharing is unimplemented.

The host `create` path remains handshake-only until milestone 12’s host TUI integration; it must still accept legacy `join_once` smoke joins.

- [ ] **Step 6: Verify milestone 11.**

Run:

```bash
cargo fmt --all -- --check
cargo test tui::tests
cargo test --test session_stream
cargo test --test session_handshake
cargo test --test cli
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
```

Expected: all pass with no interactive terminal. The guest render test proves snapshot/delta parsing and fixed-grid cropping/letterboxing.

- [ ] **Step 7: Commit milestone 11.**

```bash
git add src/session.rs src/tui.rs src/cli.rs tests/session_stream.rs tests/cli.rs
git commit -m "feat: render remote host screen"
```

Expected: one guest-rendering commit. It introduces no input injection, lease transition, multi-pane layout, or resize behavior.

## Chunk 3: Milestone 12 — control lease and input injection

### Task 5: Add and integrate the epoch-checked lease state machine

**Files:**

- Modify: `src/lib.rs`
- Create: `src/lease.rs`
- Create: `tests/lease.rs`
- Modify: `src/session.rs`
- Modify: `src/tui.rs`
- Modify: `src/cli.rs`
- Modify: `tests/session_stream.rs`

- [ ] **Step 1: Write clock-controlled lease tests first.**

Create `tests/lease.rs` using fixed peer IDs and a fixed `Instant`. Define the public API in the test imports as:

```rust
pub const IDLE_AFTER: Duration = Duration::from_secs(8);
pub struct LeaseState { pub controller_peer_id: Vec<u8>, pub epoch: u64, pub last_activity: Instant }
pub enum LeaseDecision { AcceptInput(Vec<u8>), Publish(LeaseState), RejectStaleInput, RejectStaleRequest }
pub struct LeaseManager;
impl LeaseManager {
    pub fn new(initial_controller: Vec<u8>, now: Instant) -> Self;
    pub fn state(&self) -> &LeaseState;
    pub fn input(&mut self, sender: &[u8], epoch: u64, data: Vec<u8>, now: Instant) -> LeaseDecision;
    pub fn take_control(&mut self, sender: Vec<u8>, known_epoch: u64, now: Instant) -> LeaseDecision;
}
```

Cover host initial epoch 1; matching controller input refreshes activity; wrong peer and stale/future epoch are rejected and do not change activity; a request at 7.999 seconds and exactly 8 seconds both grant because taking control is authorized in this trusted Spike but only the latter is classified as idle hop-in by the caller; stale known epoch is rejected; every grant increments monotonically and a later stale Input from the displaced controller is rejected. Include an overflow test with epoch `u64::MAX` that returns a typed error rather than wrapping.

In `tests/session_stream.rs`, add a loopback test sending a valid `Input`, stale-epoch Input, wrong-claim `Input`, idle hop-in `TakeControl`, and active explicit `TakeControl`; assert only matching current-epoch bytes arrive on the host TUI control channel and every accepted transition appears at the guest as a `ControlLease` with a newer epoch.

- [ ] **Step 2: Confirm the lease tests fail.**

Run:

```bash
cargo test --test lease
cargo test --test session_stream
```

Expected: compile failure because `lease` and control stream dispatch are absent/incomplete.

- [ ] **Step 3: Implement the pure lease module.**

Implement `src/lease.rs` exactly as the state table specifies. Keep “idle” a derived `state.is_idle_at(now)` predicate; do not create timers, tasks, global locks, user approval, or extra protocol messages. Use a typed `LeaseError::EpochExhausted` for non-wrapping epoch arithmetic. Export `pub mod lease;` from `src/lib.rs`.

- [ ] **Step 4: Connect authenticated control stream messages to lease decisions and PTY writes.**

In `session.rs`, complete the guest control writer with `GuestControlSender` methods:

```rust
impl GuestControlSender {
    pub fn try_take_control(&self, known_lease_epoch: u64) -> Result<(), ControlQueueError>;
    pub fn try_input(&self, lease_epoch: u64, data: Vec<u8>) -> Result<(), ControlQueueError>;
}
```

It constructs existing `Envelope::Input`/`Envelope::TakeControl` using the authenticated local peer ID and default pane ID; it never trusts caller-supplied peer/pane IDs. The host control reader continues validating claimed sender/requester ID before it forwards its `HostControlEvent`.

In `tui.rs`, add `run_host(host: HostPaneRuntime) -> Result<(), Box<dyn Error>>`. It is the `run_local` loop generalized to own `PtyHost`, `HostScreen`, `LeaseManager`, the screen `watch::Sender`, lease `watch::Sender`, and `HostControlEvent` receiver. Process remote control events before local keys and PTY chunks. On accepted Input, write the original data bytes to `PtyHost`; on a published lease, `send_replace` the updated state. On rejected input/request, do not write, do not publish, and do not log secrets/input contents. Each processed PTY chunk creates a `HostScreen` frame and immediately `send_replace`s it to the screen watcher without awaiting any peer.

Refactor `run_local` to call the same internal terminal loop with a local-only runtime adapter so behavior remains verified. Host local key/paste bytes participate in the same LeaseManager: controller input goes directly to PTY; idle noncontroller typing queues a local hop-in and then writes after its lease transition; Ctrl-T is explicit TakeControl when another controller is active.

In `run_guest`, maintain current `ControlLease`, classify idle from the footer’s monotonic last observed activity approximation only for UX, queue first normal key for hop-in when not controller and idle, send Ctrl-T for active explicit takeover, and flush held input only once its exact newly-announced epoch controls. Construct all subsequent `Input` with that epoch. Continue to use local Ctrl-Q and existing key/paste encoders.

- [ ] **Step 5: Wire `create` to host PTY/TUI and live peer service.**

In `cli.rs`, after `HostSession::create` prints the existing warning/ticket, allocate exactly one `HostPaneRuntime` using the host endpoint ID and `DEFAULT_PANE_ID`, spawn the async accept loop that calls `HostSession::serve_peer(incoming, channels.subscribe())` per peer, and run `tui::run_host` inside `spawn_blocking`. The host starts as controller. Ctrl-Q stops the TUI, cancels/drains all peer tasks, closes the Iroh endpoint, and restores the terminal; Ctrl-C continues to end the session before the TUI starts or when terminal setup fails. Do not create a second shell per joiner.

The join command now offers full control behavior. It must still print the same trust warning before making the connection; neither errors nor status may echo the ticket or terminal input.

- [ ] **Step 6: Verify milestone 12.**

Run:

```bash
cargo fmt --all -- --check
cargo test --test lease
cargo test --test session_stream
cargo test tui::tests
cargo test --test pty_host
cargo test --test session_handshake
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
```

Expected: all pass. In particular, the stale-epoch test proves bytes do not reach the PTY-facing control channel, and existing `local` tests remain green.

- [ ] **Step 7: Commit milestone 12.**

```bash
git add src/lib.rs src/lease.rs src/session.rs src/tui.rs src/cli.rs tests/lease.rs tests/session_stream.rs
git commit -m "feat: add pane control lease"
```

Expected: one control/injection commit; no presence, layouts, admission cap, or resize implementation.

## Chunk 4: Milestone 13 — prove priority, resync, and dogfood behavior

### Task 6: Make queue separation executable, document it, and perform final checks

**Files:**

- Create: `tests/queue_priority.rs`
- Modify: `src/session.rs`
- Modify: `src/tui.rs`
- Modify: `README.md:45-64`
- Modify: `tests/session_stream.rs`

- [ ] **Step 1: Write deterministic queue and resync regressions.**

Create `tests/queue_priority.rs` without Iroh or sleeps. Extract only the small testable queue selection/coalescing helpers required to prove these cases:

1. Publish screen sequence 2, 3, and 4 into a one-slot screen watch while its consumer is deliberately held; when released, the peer emission planner chooses Snapshot sequence 4 (not Delta) because base 3 is not the last successfully sent sequence 1.
2. Given a contiguous update, planner chooses Delta; given a 500 ms heartbeat deadline, it chooses Snapshot at the current sequence. Verify either result contains the same host snapshot bytes and no source waits for a receiver.
3. Hold a fake screen writer behind a oneshot barrier after it begins a fat snapshot. Enqueue Input and TakeControl to distinct bounded queues; prove `GuestControlWriter::next` returns TakeControl first, then Input, without waiting for the screen barrier. This is the automated Spike 2 done-when test.
4. Fill the input queue to capacity; prove `try_input` returns a typed `Full` outcome and preserves the caller-owned pending bytes rather than dropping/reordering them. Then drain capacity and prove FIFO Input ordering after the one high-priority TakeControl.

Extend `tests/session_stream.rs` with an integration-level slow-viewer test: pause the host screen send task after the first frame, publish multiple `ScreenFrame`s, send a current-epoch Input over the independent control stream, and assert the host control receiver obtains it before releasing the screen task. After release, assert guest receives a fresh Snapshot and renders the newest screen. Use deterministic barriers/oneshots plus existing five-second outer test timeout, never arbitrary sleeps.

- [ ] **Step 2: Confirm the priority tests fail.**

Run:

```bash
cargo test --test queue_priority
cargo test --test session_stream slow_screen_does_not_delay_current_epoch_input
```

Expected: FAIL until the helpers expose the planned queue behavior and the real peer service uses separate streams/tasks.

- [ ] **Step 3: Finish the concrete non-blocking/resync mechanics.**

In `session.rs`, make the screen emission choice a small pure `ScreenSendPlan` helper used by the peer screen task and test it directly. Make `GuestControlWriter` read the 16-entry TakeControl receiver first with `try_recv`, then use `tokio::select! { biased; ... }` for normal waiting; it must never hold a screen sender/receiver or `FrameWriter`. Keep the 256-entry host control channel and verify all sender error paths close only the affected peer.

In `tui.rs`, keep guest pending input bounded and status-only; on `ScreenGap`, do not blank or mutate the good parser. The next host snapshot heartbeat restores state. A remote disconnection exits the guest loop cleanly with the guard restoring the terminal; a screen error never feeds raw terminal escapes directly to stdout.

- [ ] **Step 4: Update README for actual Spike 2 dogfooding.**

Replace only the handshake-only wording with concise instructions:

```text
Terminal 1: cargo run -- create
Terminal 2: cargo run -- join '<printed p2pmux-v1:... ticket>'
```

Document that create starts one fixed-size local shell and its host view; join renders that remote pane; host and guest can control one controller at a time; idle is about eight seconds and typing from another guest hops in, while active control requires Ctrl-T. State that snapshots/deltas may be coalesced for slow viewers and recover via snapshot, and that outer-terminal resizing crops/letterboxes the fixed host grid. Retain the trust warning and explicitly say there are no tabs/splits/multiple panes yet.

- [ ] **Step 5: Run full automated and manual acceptance.**

Run automated checks:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test screen
cargo test --test lease
cargo test --test queue_priority
cargo test --test session_stream
cargo test --test session_handshake
cargo test --all-features
cargo check --all-targets --all-features
git diff --check
```

Expected: every command exits 0 and `git diff --check` is silent.

Then perform this macOS interactive localhost acceptance check, recording only pass/fail and symptoms (never ticket or typed shell data):

1. Run `cargo run -- create`, acknowledge the warning, and copy its printed ticket.
2. In a second terminal run `cargo run -- join '<ticket>'`; verify the guest gets a remote fixed-grid screen with controller footer.
3. Type in the host shell; verify host and guest redraw. Let it sit about eight seconds, type one printable guest key, and verify it appears once after controller changes to guest.
4. While guest types repeatedly, verify host ordinary typing is withheld, then press host Ctrl-T and verify visible control handoff; verify guest stale keystrokes no longer reach the host shell.
5. Trigger a large redraw in the host (for example `printf` many lines or a local full-screen app), continue typing from current controller, and verify typing remains responsive while the guest catches up via a full snapshot.
6. Resize either outer terminal; verify neither changes the host PTY grid and the guest crops/letterboxes as documented. Press Ctrl-Q in both sessions and verify terminal cleanup.

- [ ] **Step 6: Commit milestone 13.**

```bash
git add src/session.rs src/tui.rs tests/queue_priority.rs tests/session_stream.rs README.md
git commit -m "test: prove screen queue priority"
```

Expected: one priority/resync/documentation commit. No production code outside the established Spike 2 path and no PR metadata.

### Task 7: Final scope audit and push-only handoff

**Files:**

- Modify: none

- [ ] **Step 1: Confirm intended commit shape and clean state.**

Run:

```bash
git branch --show-current
git status --short
git log --oneline -5
git diff --check HEAD~5..HEAD
git diff --name-only HEAD~5..HEAD
```

Expected: branch is `spike1/local-terminal`; status is clean; the five commits are the plan plus the four milestone commits; diff check is silent. Inspect the changed-file list and stop if it includes a layout tree, resize protocol, coordinator/failover logic, or unrelated dependency/workflow change.

- [ ] **Step 2: Re-run CI-equivalent validation immediately before push.**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo check --all-targets --all-features
```

Expected: all exit 0. If any test is flaky, diagnose the synchronization/ownership issue; do not extend timeouts or mask it with retries.

- [ ] **Step 3: Push the existing branch only.**

Run:

```bash
git push origin spike1/local-terminal
```

Expected: remote branch updates successfully. Do not invoke `gh`, create/open a PR, merge, rebase, or switch branches.
