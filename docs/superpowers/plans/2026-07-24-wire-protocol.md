# Tiny Wire Protocol Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Define and test the small, versioned, length-delimited `prost` contract needed to begin Spike 2: membership bootstrap, pane input/control, and canonical screen snapshot/delta updates.

**Architecture:** Keep the contract and pure framing helpers in `src/protocol.rs`; no module opens a connection, owns session state, or touches a PTY. A length-delimited `Envelope` carries the protocol version, an unauthenticated claimed sender ID, and exactly one body. The encoder validates before encoding; the decoder bounds the complete frame and declared payload before protobuf decoding, then validates version and fields.

**Tech Stack:** Rust 2024, `prost` 0.14, Cargo, rustfmt, Clippy, existing macOS CI; unit/integration tests only, with no runtime network dependency.

---

## Scope guard

- Stay on `spike1/local-terminal`. Do not create, switch to, or merge another branch.
- Make one plan-only commit first, then two implementation commits: milestone 6 (schema) and milestone 7 (framing/validation). Do not create a PR; finish by pushing `origin/spike1/local-terminal`.
- Preserve `p2pmux local`. Do not edit `src/cli.rs`, `src/main.rs`, `src/pty_host.rs`, `src/tui.rs`, `src/session.rs`, `src/ticket.rs`, `src/transport.rs`, or `README.md`.
- Add `prost`, but do **not** add Iroh, Tokio, async I/O, sockets, a stream reader, an Iroh ticket, or a session runtime. The later transport authenticates peer identity; all v1 ID fields are claims.
- Include only `Join`, `Welcome`, `Input`, `TakeControl`, `ControlLease`, `Snapshot`, and `Delta`. The envelope has these seven concrete oneof variants.
- Do not implement LayoutCommit, full SessionSnapshot/multi-tab trees, presence, disconnect grace, coordinator election, resync requests, resize, PTY/viewer queues, or transport behavior. Never define a Resize message: pane grids are immutable.
- Snapshot/Delta payloads are opaque bytes. Sequence metadata lets a future viewer identify a gap, but no delta format, resync behavior, or runtime policy belongs here.
- Size constants live in `src/protocol.rs` and are enforced by `decode_frame`. Tests must be deterministic and network-free.

## Planned file structure

| Path | Responsibility |
| --- | --- |
| `docs/superpowers/plans/2026-07-24-wire-protocol.md` | This plan, committed before source changes. |
| `Cargo.toml`, `Cargo.lock` | Add and lock only `prost`. |
| `src/protocol.rs` | Public protobuf schema, framing codec, validation, size limits, and errors. |
| `tests/module_surface.rs` | Keeps the module-surface check and proves `Envelope` is public. |
| `tests/protocol.rs` | Network-free schema, round-trip, malformed-frame, version, and size-limit tests. |
| `src/cli.rs`, `src/main.rs`, `src/pty_host.rs`, `src/tui.rs`, `src/session.rs`, `src/ticket.rs`, `src/transport.rs`, `README.md` | No change expected. |

## Locked v1 wire contract

Identifiers are opaque, non-empty byte strings, not Iroh types or strings:

```rust
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576; // prefix + envelope
pub const MAX_ENVELOPE_BYTES: usize = 1_048_560;
pub const MAX_PEER_ID_BYTES: usize = 64;
pub const MAX_SESSION_ID_BYTES: usize = 64;
pub const MAX_PANE_ID_BYTES: usize = 64;
pub const MAX_INPUT_BYTES: usize = 8 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
pub const MAX_DELTA_BYTES: usize = 64 * 1024;
```

`MAX_ENVELOPE_BYTES` leaves room for the (at-most-ten-byte) unsigned-varint prefix inside the complete frame cap.

```text
Envelope { 1: version, 2: sender_peer_id, 10: oneof body }
Join { 1: session_id, 2: peer_id }
Welcome { 1: session_id, 2: admitted_peer_id, 3: coordinator_peer_id }
Input { 1: pane_id, 2: lease_epoch, 3: data }
TakeControl { 1: pane_id, 2: requester_peer_id, 3: known_lease_epoch }
ControlLease { 1: pane_id, 2: controller_peer_id, 3: lease_epoch }
Snapshot { 1: pane_id, 2: host_peer_id, 3: sequence, 4: screen }
Delta { 1: pane_id, 2: host_peer_id, 3: base_sequence, 4: sequence, 5: changes }
```

`Join` claims peer/session; `Welcome` names the admitted peer and coordinator. `Input.lease_epoch` lets the future host reject stale input. `TakeControl` asks for ownership; `ControlLease` states the sole controller. A snapshot has nonzero sequence. A delta requires `base_sequence > 0` and `sequence > base_sequence`; a future viewer applies it only when local sequence equals its base.

## Chunk 1: Branch baseline and milestone 6 schema

### Task 1: Commit the plan on the current branch

**Files:**

- Create: `docs/superpowers/plans/2026-07-24-wire-protocol.md`

- [ ] **Step 1: Confirm branch and clean pre-plan baseline.**

Run:

```bash
git branch --show-current
git status --short
git diff --check
git log --oneline -3
```

Expected: branch is `spike1/local-terminal`; status contains exactly `?? docs/superpowers/plans/2026-07-24-wire-protocol.md` and nothing else; diff-check is empty. The plan file is intentionally untracked at this point. Stop rather than cleaning, stashing, switching, or overwriting an unexpected change.

- [ ] **Step 2: Commit this plan only.**

Run:

```bash
git add docs/superpowers/plans/2026-07-24-wire-protocol.md
git commit -m "docs: plan tiny wire protocol"
```

Expected: documentation-only commit; no production/dependency files change.

### Task 2: Add the versioned protobuf schema (milestone 6)

**Files:**

- Modify: `Cargo.toml:7-12`
- Modify: `Cargo.lock`
- Modify: `src/protocol.rs:1-2`
- Modify: `tests/module_surface.rs:1-14`
- Create: `tests/protocol.rs`

- [ ] **Step 1: Write the failing public-contract test.**

Create `tests/protocol.rs` using this table-driven schema test. Import `prost::Message`; after constructing each envelope, encode it with `encode_to_vec()`, decode it with `Envelope::decode(wire.as_slice())`, and assert equality. This is schema-only protobuf serialization, not the public length-delimited codec from milestone 7:

```rust
use p2pmux::protocol::{
    envelope, ControlLease, Delta, Envelope, Input, Join, Snapshot, TakeControl, Welcome,
    PROTOCOL_VERSION,
};

fn envelope(body: envelope::Body) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        sender_peer_id: b"peer-a".to_vec(),
        body: Some(body),
    }
}

#[test]
fn envelope_exposes_each_v1_body() {
    let messages = [
        envelope(envelope::Body::Join(Join {
            session_id: b"session-a".to_vec(), peer_id: b"peer-a".to_vec(),
        })),
        envelope(envelope::Body::Welcome(Welcome {
            session_id: b"session-a".to_vec(), admitted_peer_id: b"peer-a".to_vec(),
            coordinator_peer_id: b"peer-host".to_vec(),
        })),
        envelope(envelope::Body::Input(Input {
            pane_id: b"pane-a".to_vec(), lease_epoch: u32::MAX as u64 + 1,
            data: b"ls\r".to_vec(),
        })),
        envelope(envelope::Body::TakeControl(TakeControl {
            pane_id: b"pane-a".to_vec(), requester_peer_id: b"peer-b".to_vec(),
            known_lease_epoch: u32::MAX as u64 + 2,
        })),
        envelope(envelope::Body::ControlLease(ControlLease {
            pane_id: b"pane-a".to_vec(), controller_peer_id: b"peer-b".to_vec(),
            lease_epoch: u32::MAX as u64 + 3,
        })),
        envelope(envelope::Body::Snapshot(Snapshot {
            pane_id: b"pane-a".to_vec(), host_peer_id: b"peer-host".to_vec(),
            sequence: u32::MAX as u64 + 4, screen: b"full screen".to_vec(),
        })),
        envelope(envelope::Body::Delta(Delta {
            pane_id: b"pane-a".to_vec(), host_peer_id: b"peer-host".to_vec(),
            base_sequence: u32::MAX as u64 + 4,
            sequence: u32::MAX as u64 + 5, changes: b"patch".to_vec(),
        })),
    ];

    let expected_body_shapes: [&[(u32, u8)]; 7] = [
        &[(1, 2), (2, 2)], &[(1, 2), (2, 2), (3, 2)],
        &[(1, 2), (2, 0), (3, 2)], &[(1, 2), (2, 2), (3, 0)],
        &[(1, 2), (2, 2), (3, 0)], &[(1, 2), (2, 2), (3, 0), (4, 2)],
        &[(1, 2), (2, 2), (3, 0), (4, 0), (5, 2)],
    ];
    for ((message, expected_body_field), expected_body_shape) in messages
        .into_iter()
        .zip(10..=16)
        .zip(expected_body_shapes)
    {
        let wire = message.encode_to_vec();
        let envelope_fields = parse_fields(&wire);
        assert_eq!(
            field_shape(&envelope_fields),
            vec![(1, 0), (2, 2), (expected_body_field, 2)],
        );
        assert_eq!(
            field_shape(&parse_fields(&envelope_fields[2].value)),
            expected_body_shape,
        );
        assert_eq!(Envelope::decode(wire.as_slice()).unwrap(), message);
    }
}
```

Add a test-local `ParsedField { field_number: u32, wire_type: u8, value: Vec<u8> }` record, a `field_shape(&[ParsedField]) -> Vec<(u32, u8)>` projection, and a `parse_fields` helper that reads each protobuf key varint into ordered `ParsedField` values and skips/collects varint or length-delimited values. Make malformed input panic in this test helper. Use it first to verify the Envelope shape is exactly `[(1, 0), (2, 2), (expected_body_field, 2)]`; then parse that length-delimited body value and assert its exact field-number/wire-type sequence: Join `[(1, 2), (2, 2)]`, Welcome `[(1, 2), (2, 2), (3, 2)]`, Input `[(1, 2), (2, 0), (3, 2)]`, TakeControl `[(1, 2), (2, 2), (3, 0)]`, ControlLease `[(1, 2), (2, 2), (3, 0)]`, Snapshot `[(1, 2), (2, 2), (3, 0), (4, 2)]`, and Delta `[(1, 2), (2, 2), (3, 0), (4, 0), (5, 2)]`. This verifies all v1 field tags and wire types rather than matching tag bytes inside payload content. The values above make an accidental `uint32` declaration for every lease/sequence field fail the schema test. Keep these exact shape assertions paired with protobuf round trips so no listed tag or wire type can silently change.

Update `tests/module_surface.rs` to import `Envelope` and add:

```rust
let _: Option<Envelope> = None;
```

- [ ] **Step 2: Confirm the test fails.**

Run:

```bash
cargo test --test protocol envelope_exposes_each_v1_body
```

Expected: compile FAIL because `prost` and the public types are absent.

- [ ] **Step 3: Add only the protobuf dependency.**

Append to the existing dependency table in `Cargo.toml`; retain every current Spike 1 dependency/version:

```toml
prost = "0.14"
```

Do not add `prost-build`, a `.proto` file, a build script, Iroh, Tokio, or an error-helper crate. Rust derives are the complete small v1 schema.

- [ ] **Step 4: Replace the protocol marker with the exact protobuf types.**

Replace all of `src/protocol.rs` with constants above and this shape, preserving every tag:

```rust
//! Versioned, length-delimited messages for the future pane transport.

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Envelope {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub sender_peer_id: Vec<u8>,
    #[prost(oneof = "envelope::Body", tags = "10, 11, 12, 13, 14, 15, 16")]
    pub body: Option<envelope::Body>,
}

pub mod envelope {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Body {
        #[prost(message, tag = "10")]
        Join(super::Join),
        #[prost(message, tag = "11")]
        Welcome(super::Welcome),
        #[prost(message, tag = "12")]
        Input(super::Input),
        #[prost(message, tag = "13")]
        TakeControl(super::TakeControl),
        #[prost(message, tag = "14")]
        ControlLease(super::ControlLease),
        #[prost(message, tag = "15")]
        Snapshot(super::Snapshot),
        #[prost(message, tag = "16")]
        Delta(super::Delta),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Join {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub peer_id: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Welcome {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub admitted_peer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub coordinator_peer_id: Vec<u8>,
}
```

Add `Input`, `TakeControl`, `ControlLease`, `Snapshot`, and `Delta` exactly as field/tagged in **Locked v1 wire contract**, using `bytes = "vec"` for IDs/payloads and `uint64` for epochs/sequences. Do not add a framing API, authentication, runtime policy, or transport behavior in this task.

- [ ] **Step 5: Verify schema and preserve Spike 1.**

Run:

```bash
cargo fmt --all
cargo test --test protocol envelope_exposes_each_v1_body
cargo test --test module_surface
cargo test --test cli
cargo test --test pty_host
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all exit 0; no test opens a network connection or terminal UI.

- [ ] **Step 6: Commit milestone 6.**

Run:

```bash
git add Cargo.toml Cargo.lock src/protocol.rs tests/module_surface.rs tests/protocol.rs
git commit -m "feat: define tiny wire protocol"
```

Expected: schema-only commit with seven typed variants; no codec, Iroh, or session behavior.

## Chunk 2: Milestone 7 framing and decode-time safety

### Task 3: Add strict framing, validation, and codec tests (milestone 7)

**Files:**

- Modify: `src/protocol.rs`
- Modify: `tests/protocol.rs`

- [ ] **Step 1: Write failing round-trip and rejection tests.**

Append a `sample_envelopes() -> Vec<Envelope>` helper that returns the seven valid values from Task 2 and add:

```rust
use prost::Message;
use p2pmux::protocol::{
    decode_frame, encode_frame, ProtocolError, MAX_ENVELOPE_BYTES, MAX_INPUT_BYTES,
    MAX_SNAPSHOT_BYTES, MAX_DELTA_BYTES, MAX_FRAME_BYTES,
};

#[test]
fn framed_envelopes_round_trip_all_v1_bodies() {
    for original in sample_envelopes() {
        let frame = encode_frame(&original).expect("valid envelope encodes");
        assert_eq!(decode_frame(&frame).expect("valid frame decodes"), original);
    }
}

#[test]
fn decoder_rejects_unsupported_version() {
    let mut wrong = sample_envelopes().remove(0);
    wrong.version = PROTOCOL_VERSION + 1;
    let mut frame = Vec::new();
    wrong.encode_length_delimited(&mut frame).unwrap();

    assert!(matches!(
        decode_frame(&frame),
        Err(ProtocolError::UnsupportedVersion(v)) if v == PROTOCOL_VERSION + 1
    ));
}

#[test]
fn decoder_rejects_oversize_declared_and_decoded_payloads() {
    let declared_only = encode_varint((MAX_ENVELOPE_BYTES + 1) as u64);
    assert!(matches!(
        decode_frame(&declared_only),
        Err(ProtocolError::FrameTooLarge { .. })
    ));

    let oversized_input = envelope(envelope::Body::Input(Input {
        pane_id: b"pane-a".to_vec(), lease_epoch: 1,
        data: vec![0; MAX_INPUT_BYTES + 1],
    }));
    let mut input_frame = Vec::new();
    oversized_input.encode_length_delimited(&mut input_frame).unwrap();
    assert!(matches!(
        decode_frame(&input_frame),
        Err(ProtocolError::FieldTooLarge { field: "input.data", .. })
    ));

    let oversized_snapshot = envelope(envelope::Body::Snapshot(Snapshot {
        pane_id: b"pane-a".to_vec(), host_peer_id: b"peer-host".to_vec(),
        sequence: 1, screen: vec![0; MAX_SNAPSHOT_BYTES + 1],
    }));
    let mut snapshot_frame = Vec::new();
    oversized_snapshot.encode_length_delimited(&mut snapshot_frame).unwrap();
    assert!(matches!(
        decode_frame(&snapshot_frame),
        Err(ProtocolError::FieldTooLarge { field: "snapshot.screen", .. })
    ));
}
```

Implement test-local `encode_varint(u64) -> Vec<u8>`; do not use `to_le_bytes()`. Add focused decoder tests for missing body, malformed/truncated varint, a non-oversize truncated frame, a frame with trailing bytes, an overflowing tenth-byte varint, an empty identifier, zero lease epoch, `Delta.sequence <= base_sequence`, and a valid Delta with exactly `MAX_DELTA_BYTES` changes. Add `assert!(matches!(decode_frame(&vec![0; MAX_FRAME_BYTES + 1]), Err(ProtocolError::FrameTooLarge { .. })))` to distinguish an over-complete frame from an over-declared payload. Add a table-driven `encode_frame_rejects_invalid_envelopes` test over invalid version/body/identifier/lease/sequence and each one-byte-over field cap. These must fail because codec APIs and validation do not yet exist.

- [ ] **Step 2: Confirm codec tests fail.**

Run:

```bash
cargo test --test protocol
```

Expected: compile FAIL because `encode_frame`, `decode_frame`, `ProtocolError`, and validation are absent.

- [ ] **Step 3: Add the public codec/error surface.**

In `src/protocol.rs`, import `prost::Message`, add manual `Display` and `std::error::Error` implementations, and expose:

```rust
#[derive(Debug)]
pub enum ProtocolError {
    FrameTooLarge { limit: usize, actual: usize },
    MalformedLengthPrefix,
    TruncatedFrame { declared: usize, available: usize },
    TrailingFrameBytes { declared: usize, actual: usize },
    Encode(prost::EncodeError),
    Decode(prost::DecodeError),
    UnsupportedVersion(u32),
    MissingBody,
    EmptyField(&'static str),
    FieldTooLarge { field: &'static str, limit: usize, actual: usize },
    InvalidLeaseEpoch(&'static str),
    InvalidScreenSequence(&'static str),
}

pub fn encode_frame(envelope: &Envelope) -> Result<Vec<u8>, ProtocolError>;
pub fn decode_frame(frame: &[u8]) -> Result<Envelope, ProtocolError>;
```

`ProtocolError::Encode` and `ProtocolError::Decode` expose their prost causes through `source()`. Map the fallible `encode_length_delimited` result to `ProtocolError::Encode`; tests match variants/fields, never display strings.

- [ ] **Step 4: Implement strict length-delimited framing before decode.**

Implement a private `decode_length_prefix(frame: &[u8]) -> Result<(usize, usize), ProtocolError>` that reads an unsigned protobuf varint one byte at a time, accepts at most ten bytes, checks overflow before `usize` conversion, and allocates nothing. Unterminated/overflowed prefixes return `MalformedLengthPrefix`.

`decode_frame` must do this, in order:

1. Reject `frame.len() > MAX_FRAME_BYTES` with `FrameTooLarge`.
2. Parse the prefix; if declared payload exceeds `MAX_ENVELOPE_BYTES`, return `FrameTooLarge` immediately even when truncated.
3. Use checked addition for prefix plus declared length; reject short frames as `TruncatedFrame` and surplus bytes as `TrailingFrameBytes`. One call accepts exactly one frame.
4. Decode only the bounded payload using `Envelope::decode`, then call `validate_envelope`.

`encode_frame` first validates, then checks `encoded_len() <= MAX_ENVELOPE_BYTES`, reserves only checked prefix-plus-payload capacity, calls `encode_length_delimited`, maps its error to `ProtocolError::Encode`, and confirms result length is within `MAX_FRAME_BYTES`. Never `unwrap` in production code. `MAX_FRAME_BYTES` is the hard pre-decode allocation ceiling: individual field caps are semantic checks immediately after bounded protobuf decoding, so no untrusted frame can allocate more than 1 MiB in this protocol layer. Do not add a custom protobuf field scanner, `Read`, `AsyncRead`, stream buffering, channels, or transport queues; a later transport adapter will collect a bounded complete frame and call this API.

- [ ] **Step 5: Implement explicit shared validation.**

Add private `validate_id(field, bytes, limit)` and `validate_envelope`. It checks version, non-empty/capped `sender_peer_id`, a present body, and explicit arms for every body—no wildcard arm.

| Body | Required validation |
| --- | --- |
| `Join` | Non-empty/capped `session_id` and `peer_id`. |
| `Welcome` | Non-empty/capped `session_id`, `admitted_peer_id`, and `coordinator_peer_id`. |
| `Input` | Non-empty/capped `pane_id`; nonzero `lease_epoch`; `data.len() <= MAX_INPUT_BYTES` (empty data allowed). |
| `TakeControl` | Non-empty/capped `pane_id` and `requester_peer_id`; zero `known_lease_epoch` is valid (“no lease observed”). |
| `ControlLease` | Non-empty/capped `pane_id` and `controller_peer_id`; nonzero `lease_epoch`. |
| `Snapshot` | Non-empty/capped `pane_id` and `host_peer_id`; nonzero sequence; `screen.len() <= MAX_SNAPSHOT_BYTES` (empty screen allowed). |
| `Delta` | Non-empty/capped `pane_id` and `host_peer_id`; nonzero base; `sequence > base_sequence`; `changes.len() <= MAX_DELTA_BYTES` (empty changes allowed). |

An `Envelope` always exposes exactly one body after protobuf oneof decoding; protobuf's standard repeated-oneof behavior is last-value-wins. Treat that normal decoder behavior as the v1 compatibility rule rather than adding a custom raw-wire scanner. Do not require sender/embedded-ID equality: the sender is unauthenticated and forwarding/routing semantics do not exist yet. Do not interpret leases, apply deltas, or create a screen parser.

- [ ] **Step 6: Run the full network-free regression suite.**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test protocol
cargo test --test module_surface
cargo test --test cli
cargo test --test pty_host
cargo test --all-features
cargo check --all-targets --all-features
git diff --check
```

Expected: every command exits 0. Protocol tests prove seven-body round trips and encoder-side rejection of invalid version/body/ID/epoch/sequence/size values. Decoder tests reject bad version, empty/missing fields, oversized complete/declared/input/snapshot/delta frames, a non-oversize truncated frame, trailing bytes after one frame, and malformed short/overflowing tenth-byte varints before any network is used. Boundary tests accept exactly `MAX_INPUT_BYTES`, `MAX_SNAPSHOT_BYTES`, and `MAX_DELTA_BYTES`, then reject one byte over each cap.

- [ ] **Step 7: Commit milestone 7.**

Run:

```bash
git add src/protocol.rs tests/protocol.rs
git commit -m "feat: validate wire protocol frames"
```

Expected: framing/validation-only commit; no Iroh, sockets, PTY/TUI, layout, presence, resize, session-runtime, or CLI changes.

## Chunk 3: Final verification and push only

### Task 4: Verify scope and the existing local terminal, then push

**Files:**

- Modify: none

- [ ] **Step 1: Inspect history, scope, and CI-equivalent checks.**

Run:

```bash
git branch --show-current
git status --short
git log --oneline -3
git diff --check HEAD~3..HEAD
git diff --name-only HEAD~3..HEAD
git diff HEAD~3..HEAD -- Cargo.toml Cargo.lock src/protocol.rs
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo check --all-targets --all-features
```

Expected: branch is `spike1/local-terminal`; status is empty; newest commits are `feat: validate wire protocol frames`, `feat: define tiny wire protocol`, and `docs: plan tiny wire protocol`. Changed paths are only this plan, `Cargo.toml`, `Cargo.lock`, `src/protocol.rs`, `tests/module_surface.rs`, and `tests/protocol.rs`. Inspect the final content diff and explicitly confirm it adds only `prost`, protobuf definitions, pure codec/validation, and tests—no Iroh, Tokio, async I/O, socket, transport, session, PTY, TUI, CLI, layout, presence, or Resize code.

- [ ] **Step 2: Smoke-test Spike 1 manually without changing it.**

Run:

```bash
cargo run -- local
```

Inside it, (1) run `vim`, type briefly, then `:q!`; verify alternate-screen, input, and return, (2) run `top`, observe redraws, then `q`; verify refresh/exit, (3) run an installed Claude-like terminal UI if available and verify input/exit, and (4) Ctrl-Q p2pmux; verify the host terminal returns with a visible cursor. Expected: the shell behaves as the existing Spike 1 acceptance bar requires. Any automated-check or manual-check failure blocks the push: record it and stop without broadening this protocol milestone with local-terminal changes.

- [ ] **Step 3: Push the existing branch; do not create a PR.**

Run:

```bash
git push -u origin spike1/local-terminal
git rev-parse HEAD
git ls-remote --heads origin spike1/local-terminal
```

Expected: all three commits are pushed to `origin/spike1/local-terminal`; the SHA from `git ls-remote` matches local `HEAD`. Stop here; do not invoke `gh pr create`.
