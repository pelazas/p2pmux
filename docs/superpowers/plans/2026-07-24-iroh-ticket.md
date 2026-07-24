# Iroh Transport and Reusable Ticket Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a live Iroh 1.x host that prints one reusable join ticket, and make `p2pmux join <ticket>` complete the existing `Join` → `Welcome` exchange over an authenticated Iroh bi-stream.

**Architecture:** `transport` owns the Iroh 1.0.3 endpoint, ALPN, bounded connect/accept, and one-frame stream I/O. Iroh 1.0.3 uses `EndpointAddr`, not a built-in printable node-ticket API, so `ticket` owns a versioned wrapper around a serde-encoded EndpointAddr. The session ID is the 32-byte coordinator endpoint ID. `session` composes these pieces: create keeps the host live for repeated handshakes; join connects once, verifies Iroh-authenticated identity against protocol claims, prints success, and exits.

**Tech Stack:** Rust 2024, Tokio 1.x, Iroh 1.0.3, Prost 0.14, Serde/serde_json, base64 0.22, Clap, Cargo, macOS CI.

---

## Scope guard

- Stay on spike1/local-terminal. Do not create/switch branches, merge, or open a PR.
- Commit in order: this plan, #8 endpoint/ticket smoke, then #9 reusable create/join. Finish by pushing origin/spike1/local-terminal only.
- Preserve p2pmux local exactly. Tests never start an interactive TUI.
- Production endpoints use Iroh 1.0.3 `Endpoint::builder(presets::N0).alpns(...).bind().await`, giving standard Iroh address lookup and relay defaults. Before the production ticket is minted, create makes one bounded `Endpoint::online()` attempt so the current EndpointAddr normally includes relay discovery; on timeout it prints the current direct-address ticket plus an explicit localhost/LAN-only warning rather than blocking forever. Tests use presets::Minimal, RelayMode::Disabled, clear_ip_transports(), and 127.0.0.1:0; no test uses DNS, relays, Endpoint::online(), or Internet access.
- The ticket is reusable until its create process exits, after which its ephemeral endpoint is gone. Print the existing trust warning before create and join.
- Do not add identity persistence, member admission/cap, PTY streaming, Snapshot/Delta loop, LayoutCommit, pane/mux state, control/input forwarding, presence, or direct/relay UI. Existing protocol variants other than Join and Welcome stay unused.
- Create hosts handshakes until Ctrl-C. Join proves one handshake then closes. Bound every accept/connect/stream operation at five seconds.

## Planned file structure

| Path | Responsibility |
| --- | --- |
| docs/superpowers/plans/2026-07-24-iroh-ticket.md | This plan; first commit. |
| Cargo.toml, Cargo.lock | Tokio, Iroh, base64, Serde, serde_json. |
| src/transport.rs | Endpoint ownership, ALPN, bounded Iroh operations, one-frame I/O. |
| src/ticket.rs | Printable ticket mint/parse/validation. |
| src/session.rs | Host lifecycle and authenticated Join/Welcome orchestration. |
| src/cli.rs, src/main.rs | Async create/join and Ctrl-C lifecycle; local unchanged. |
| tests/module_surface.rs | Resource-owning type surface assertions. |
| tests/transport.rs | Localhost Endpoint connect/accept/bi-stream smoke. |
| tests/ticket.rs | Pure ticket contract tests. |
| tests/session_handshake.rs | Two joins with the same printed ticket. |
| tests/cli.rs | Non-interactive CLI regression tests. |
| README.md | Handshake-only manual instructions. |
| src/protocol.rs, src/pty_host.rs, src/tui.rs, docs/MVP_DESIGN.md, docs/SPIKE_PLAN.md, CI | No change expected. |

## Locked ticket and handshake contract

Iroh 1.0.3 has no printable NodeTicket. The p2pmux ticket is:

```text
p2pmux-v1:<base64url-no-pad compact UTF-8 JSON>
{"version":1,"session_id":[32 bytes],"endpoint_addr":{"id":"<iroh public-key hex>","addrs":[...]}}
```

session_id is exactly endpoint_addr.id.as_bytes(). A parser requires the literal prefix, decoded payload no larger than MAX_TICKET_PAYLOAD_BYTES (16 KiB), version 1, exactly 32 session-ID bytes, at least one EndpointAddr transport address, and endpoint/session byte equality. Errors describe only the failure class and never echo ticket input.

ALPN is b"p2pmux/1". Each join uses one Iroh bi-stream and exactly one existing framed Envelope in each direction. The client connects, opens a bi-stream, writes and finishes Join; the host accepts, accepts the bi-stream, reads one frame, validates Join, writes and finishes Welcome; the client reads and validates Welcome. Writes use encode_frame + SendStream::write_all + finish; reads use RecvStream::read_to_end(MAX_FRAME_BYTES) + decode_frame, rejecting empty, partial, oversized, trailing, or concatenated frames through the existing codec.

## Chunk 1: Baseline and milestone #8

### Task 1: Commit this plan

**Files:**

- Create: docs/superpowers/plans/2026-07-24-iroh-ticket.md

- [ ] **Step 1: Confirm the pre-plan baseline.**

Run:

```bash
git branch --show-current
git status --short
git diff --check
git log --oneline -4
```

Expected: spike1/local-terminal, only this untracked plan, and a silent diff check. Stop if any other change exists; do not clean, stash, switch, or overwrite it.

- [ ] **Step 2: Commit the plan only.**

```bash
git add docs/superpowers/plans/2026-07-24-iroh-ticket.md
git commit -m "docs: plan iroh transport and ticket"
```

Expected: documentation-only commit.

### Task 2: Add dependency and public-type foundations

**Files:**

- Modify: Cargo.toml:7-12
- Modify: Cargo.lock
- Modify: src/session.rs:1-2
- Modify: tests/module_surface.rs:1-20

- [ ] **Step 1: Write the failing public type test.**

Replace the current unit construction of Session, Transport, and JoinTicket with Option type assertions, retaining current CLI/PTY/TUI/protocol checks:

```rust
use p2pmux::{
    session::HostSession,
    ticket::JoinTicket,
    transport::Transport,
};
let _: Option<Transport> = None;
let _: Option<JoinTicket> = None;
let _: Option<HostSession> = None;
```

The temporary public HostSession is opaque with private fields; remove the old Session marker rather than retaining a second session API.

- [ ] **Step 2: Verify it fails.**

Run: cargo test --test module_surface

Expected: compile failure because HostSession is absent.

- [ ] **Step 3: Add exactly these direct dependencies.**

```toml
base64 = "0.22"
iroh = "1.0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "time"] }
```

Run cargo check to lock them. Do not add anyhow, random-ID crates, Iroh test-utils, or relay-server crates.

- [ ] **Step 4: Verify current behavior.**

```bash
cargo fmt --all
cargo test --test module_surface
cargo test --test protocol
cargo test --test pty_host
cargo test --test cli
```

Expected: all pass without runtime networking.

### Task 3: Test and implement raw Iroh endpoint smoke

**Files:**

- Modify: src/transport.rs:1-2
- Create: tests/transport.rs

- [ ] **Step 1: Write the failing direct-loopback test.**

Create tests/transport.rs using #[tokio::test(flavor = "multi_thread", worker_threads = 2)] and a five-second timeout. The helper must build endpoint instances with:

```rust
Endpoint::builder(presets::Minimal)
    .relay_mode(RelayMode::Disabled)
    .clear_ip_transports()
    .bind_addr((Ipv4Addr::LOCALHOST, 0))?
    .alpns(vec![ALPN.to_vec()])
    .bind()
    .await?
```

endpoint_connects_accepts_and_opens_a_bi_stream_on_localhost must wrap two endpoints with Transport::from_endpoint, spawn host.accept_connection(), assert accepted Connection::remote_id equals the client ID, call client.connect(host.endpoint_addr()), open_bi, write b"ping", finish, then accept_bi/read_to_end(4) and assert b"ping". Time out each network future; close both endpoints and await the task.

- [ ] **Step 2: Verify it fails.**

Run: cargo test --test transport endpoint_connects_accepts_and_opens_a_bi_stream_on_localhost

Expected: compile failure because transport APIs are absent.

- [ ] **Step 3: Implement the exact endpoint boundary.**

```rust
pub const ALPN: &[u8] = b"p2pmux/1";
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Transport { endpoint: Endpoint }

impl Transport {
    pub async fn bind() -> Result<Self, TransportError>;
    pub fn from_endpoint(endpoint: Endpoint) -> Self;
    pub fn endpoint_id(&self) -> EndpointId;
    pub fn endpoint_addr(&self) -> EndpointAddr;
    pub async fn wait_until_online(&self) -> bool;
    pub async fn accept_incoming(&self) -> Result<Incoming, TransportError>;
    pub async fn connect(&self, remote: EndpointAddr) -> Result<Connection, TransportError>;
    pub async fn accept_connection(&self) -> Result<Connection, TransportError>;
    pub async fn close(&self);
}
```

bind uses Endpoint::builder(presets::N0).alpns(vec![ALPN.to_vec()]).bind().await. wait_until_online returns whether timeout(HANDSHAKE_TIMEOUT, endpoint.online()) completed; it must not turn an offline localhost host into an error. accept_incoming timeouts only endpoint.accept() and maps None to a closed-endpoint error; accept_connection is its convenience wrapper that also timeouts awaiting Incoming. connect timeouts Endpoint::connect(remote, ALPN). Use manual Display/Error TransportError with sources; no Iroh Router. Keep the endpoint private; from_endpoint is the deterministic advanced/test constructor.

- [ ] **Step 4: Verify the #8 transport smoke.**

```bash
cargo fmt --all
cargo test --test transport
cargo test --test module_surface
cargo test --test protocol
cargo test --test pty_host
cargo test --test cli
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all pass; only bounded loopback networking occurs.

### Task 4: Test and implement reusable ticket mint/parse

**Files:**

- Modify: src/ticket.rs:1-2
- Create: tests/ticket.rs

- [ ] **Step 1: Write failing ticket tests.**

Build deterministic non-network EndpointAddr from SecretKey::from_bytes(&[7; 32]).public() plus 127.0.0.1:4242. Test mint → to_string → parse equality, prefix, endpoint address, and session ID. Parse the same string twice and assert equality.

Add table-driven rejection cases: empty input, wrong prefix, malformed base64, invalid JSON, version 2, 31-byte session ID, endpoint/session mismatch, empty addresses, and decoded payload one byte over the cap. Assert error variants/classes and assert error display never contains the original ticket.

- [ ] **Step 2: Verify failure.**

Run: cargo test --test ticket

Expected: compile failure because mint/parser/accessors/error/cap are absent.

- [ ] **Step 3: Implement the locked ticket API.**

```rust
pub const TICKET_PREFIX: &str = "p2pmux-v1:";
pub const TICKET_VERSION: u8 = 1;
pub const MAX_TICKET_PAYLOAD_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinTicket {
    session_id: [u8; 32],
    endpoint_addr: EndpointAddr,
}
impl JoinTicket {
    pub fn mint(endpoint_addr: EndpointAddr) -> Result<Self, TicketError>;
    pub fn session_id(&self) -> &[u8; 32];
    pub fn endpoint_addr(&self) -> &EndpointAddr;
}
impl Display for JoinTicket { /* URL_SAFE_NO_PAD JSON + prefix */ }
impl FromStr for JoinTicket { type Err = TicketError; /* strict validation */ }
```

Use private serde TicketPayload { version: u8, session_id: Vec<u8>, endpoint_addr: EndpointAddr }. Mint rejects empty addresses and copies endpoint_addr.id.as_bytes. Display uses serde_json::to_vec and base64::engine::general_purpose::URL_SAFE_NO_PAD. Parser preflights size and validates every locked condition. Add no signature, encryption, expiry, nonce, or alternative invite code.

- [ ] **Step 4: Verify and commit #8.**

```bash
cargo fmt --all
cargo test --test ticket
cargo test --test transport
cargo test --test module_surface
cargo test --test protocol
cargo test --test pty_host
cargo test --test cli
cargo clippy --all-targets --all-features -- -D warnings
git add Cargo.toml Cargo.lock src/transport.rs src/ticket.rs src/session.rs tests/module_surface.rs tests/transport.rs tests/ticket.rs
git commit -m "feat: add iroh endpoint and ticket smoke"
```

Expected: one #8 commit with raw endpoint proof and reusable ticket parsing, but no Join/Welcome runtime.

## Chunk 2: Milestone #9 reusable create/join

### Task 5: Write the repeated-ticket Join/Welcome integration test

**Files:**

- Create: tests/session_handshake.rs
- Modify: src/session.rs
- Modify: src/transport.rs

- [ ] **Step 1: Add the failing authoritative smoke test.**

Use the direct-only endpoint helper from Task 3. Test the_same_ticket_admits_two_joiners_in_separate_handshakes:

1. Construct HostSession::from_transport(host_transport); save host.ticket().to_string() once.
2. Parse that text independently twice and assert equal tickets.
3. Keep the original host for final shutdown, clone HostSession into a host task, and have that task call accept_one_join() twice and return both receipts.
4. Construct two different localhost client transports and call join_once(client, ticket) sequentially.
5. Assert session IDs equal ticket session; host admitted IDs equal each client endpoint ID; both join receipts name the host endpoint as coordinator; admitted IDs differ.
6. Close all endpoints and await the host task.

Place every task and connection operation under the five-second timeout. This proves one printable ticket is reusable and the actual existing frames handshake without human interaction.

- [ ] **Step 2: Verify failure.**

Run: cargo test --test session_handshake the_same_ticket_admits_two_joiners_in_separate_handshakes

Expected: compile failure because framed helpers, session lifecycle, receipt, and join function are absent.

### Task 6: Implement framed transport and authenticated session

**Files:**

- Modify: src/transport.rs
- Modify: src/session.rs

- [ ] **Step 1: Add bounded single-frame helpers to Transport.**

```rust
pub async fn open_bi(&self, connection: &Connection)
    -> Result<(SendStream, RecvStream), TransportError>;
pub async fn accept_bi(&self, connection: &Connection)
    -> Result<(SendStream, RecvStream), TransportError>;
pub async fn write_frame(&self, send: &mut SendStream, envelope: &Envelope)
    -> Result<(), TransportError>;
pub async fn read_frame(&self, recv: &mut RecvStream)
    -> Result<Envelope, TransportError>;
```

Timeout each Iroh operation. write_frame must encode exactly one existing frame, write_all it, then finish. read_frame must timeout read_to_end(MAX_FRAME_BYTES), then decode_frame. Extend TransportError for stream/protocol failures without ticket text. Do not add an incremental or long-lived reader.

- [ ] **Step 2: Implement this session API.**

```rust
#[derive(Clone)]
pub struct HostSession {
    transport: Transport,
    ticket: JoinTicket,
    address_ready: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinReceipt {
    pub session_id: Vec<u8>,
    pub admitted_peer_id: Vec<u8>,
    pub coordinator_peer_id: Vec<u8>,
}

impl HostSession {
    pub async fn create() -> Result<Self, SessionError>;
    pub fn from_transport(transport: Transport) -> Result<Self, SessionError>;
    pub fn ticket(&self) -> &JoinTicket;
    pub fn address_ready(&self) -> bool;
    pub async fn accept_incoming(&self) -> Result<Incoming, SessionError>;
    pub async fn handle_incoming(&self, incoming: Incoming) -> Result<JoinReceipt, SessionError>;
    pub async fn accept_one_join(&self) -> Result<JoinReceipt, SessionError>;
    pub async fn close(&self);
}
pub async fn join_once(transport: Transport, ticket: JoinTicket)
    -> Result<JoinReceipt, SessionError>;
```

create calls Transport::bind, awaits wait_until_online, then mints one ticket from the resulting current endpoint_addr and stores that bool in address_ready. from_transport remains the deterministic test constructor, mints immediately from its supplied direct endpoint, and sets address_ready to true because its exact loopback address is known.

accept_incoming delegates to Transport::accept_incoming without awaiting the handshake. handle_incoming awaits that owned Incoming under the transport timeout, records connection.remote_id, accepts the stream, and permits only Join. accept_one_join is a small test/convenience composition of accept_incoming then handle_incoming. Before Welcome require:

```rust
join.session_id.as_slice() == self.ticket.session_id()
envelope.sender_peer_id.as_slice() == connection.remote_id().as_bytes()
join.peer_id.as_slice() == connection.remote_id().as_bytes()
```

Welcome’s envelope sender/coordinator is host endpoint ID; admitted peer is authenticated remote ID. Join first requires connection.remote_id == ticket.endpoint_addr().id, sends Join using the client endpoint ID for envelope sender and peer, then requires Welcome envelope sender/coordinator equal ticket endpoint ID, session equal ticket session, and admitted peer equal client ID. Close connection and endpoint on success and error. This is the required authentication bridge from protocol claims to Iroh TLS identity.

- [ ] **Step 3: Verify handshaking.**

```bash
cargo fmt --all
cargo test --test session_handshake the_same_ticket_admits_two_joiners_in_separate_handshakes
```

Expected: PASS within five seconds without public network/TUI. Diagnose failures; do not merely enlarge timeouts.

### Task 7: Wire CLI, docs, commit, and push

**Files:**

- Modify: src/cli.rs:1-58
- Modify: src/main.rs:1-3
- Modify: tests/cli.rs:1-58
- Modify: README.md:38-47

- [ ] **Step 1: Write failing CLI regressions.**

Keep missing-ticket and help tests. Replace scaffold tests with: help describes a reusable shared-session ticket; p2pmux join not-a-ticket exits nonzero, prints the trust warning, reports invalid ticket format, and does not echo not-a-ticket. Do not process-test create because the correct process remains alive; Task 5 is the deterministic create/join evidence.

- [ ] **Step 2: Verify failure.**

Run: cargo test --test cli

Expected: FAIL because scaffolding accepts arbitrary tickets and says unimplemented.

- [ ] **Step 3: Implement async CLI dispatch.**

Make main #[tokio::main] and await async cli::parse_and_run/private run. Preserve Command::Local => crate::tui::run_local() exactly.

Create prints/flushes the unchanged trust warning, calls HostSession::create().await, and prints a label and the ticket plus “Waiting for join handshakes; press Ctrl-C to end this live session.” If create's bounded online wait timed out, print one additional warning that the ticket contains only currently discovered direct addresses, so localhost/LAN is supported but public reachability is not yet confirmed. Use tokio::select! over ctrl_c and host.accept_incoming(). For every accepted Incoming, clone HostSession into a tokio::task::JoinSet task that owns the Incoming and runs handle_incoming(incoming); this makes the task 'static and keeps the accept loop available while a peer stalls during handshake. Log only failure class to stderr; on Ctrl-C abort/drain tasks and await host.close.

Join prints/flushes warning, parses ticket before binding, calls join_once(Transport::bind().await?, parsed).await, then prints a short coordinator-success message stating terminal sharing is not yet implemented. Never echo complete ticket; parse/handshake errors are nonzero.

- [ ] **Step 4: Update README.**

Replace scaffolding-only text with:

```text
Terminal 1: cargo run -- create
Terminal 2: cargo run -- join '<printed p2pmux-v1:... ticket>'
```

State reuse is valid while terminal 1 lives and this proves encrypted Iroh Join/Welcome only—no terminal mirroring or attached join process. Retain local-mode and trust-warning text.

- [ ] **Step 5: Verify, commit #9, and push only.**

```bash
cargo fmt --all -- --check
cargo test --test ticket
cargo test --test transport
cargo test --test session_handshake
cargo test --test cli
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
cargo run -- --help
cargo run -- join not-a-ticket
git add src/transport.rs src/session.rs src/cli.rs src/main.rs tests/session_handshake.rs tests/cli.rs README.md
git commit -m "feat: handshake reusable iroh tickets"
git branch --show-current
git status --short
git log --oneline -3
git diff --check HEAD~3..HEAD
git push origin spike1/local-terminal
```

Expected: every Cargo command passes. The malformed join is the sole intentional nonzero result and never echoes its ticket. Before push, branch is spike1/local-terminal, status clean, log contains plan/#8/#9, and diff check is silent. Do not run gh pr create, merge, or switch branches.
