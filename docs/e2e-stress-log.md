# p2pmux end-to-end stress log

Hand-driven end-to-end stress testing of p2pmux: real `target/release/p2pmux` binaries on
real PTYs, driven with real keystrokes, asserted against the bytes they actually rendered
back. No unit tests, no mocks, no p2pmux-inside-tmux.

Harness: `scripts/e2e/driver.py` (Python `pty` + `pyte`), smoke test `scripts/e2e/smoke.py`.
Branch: `e2e-stress`.

## Standing caveats

- **Localhost / direct path only.** Every peer runs on one Mac. This exercises the direct
  transport and the local rendezvous join-code file. It does **not** cover relay fallback,
  NAT traversal, real network latency, packet loss, reordering, or MTU effects. Nothing in
  this log should be read as "P2P is tested".
- Per instruction, the network interface is never touched (no `ifconfig`/`pfctl`). Transport
  failure is simulated at the process level (kill / SIGSTOP / SIGCONT).
- The user's own `target/debug/p2pmux` sessions run on this machine. The harness sandboxes
  `HOME` and only ever kills p2pmux pids that appeared *after* it started, so real sessions
  are never disturbed.

## Harness notes (read before extending)

- `p2pmux create` / `p2pmux join` fork a **detached background node** (`p2pmux __node`,
  own process group, see `src/cli.rs:417 launch_background_node`). It owns the PTYs and
  **outlives the foreground client**. Killing the peer you spawned does not kill it —
  `Harness.__exit__` reaps it by pid diff against a baseline taken at entry.
- p2pmux resolves its session store, config, and rendezvous dir from `$HOME`
  (`session_store.rs:181`, `config.rs:167`, `rendezvous.rs:65`). Each `Harness` gets a
  private temp `HOME`, so runs are isolated from each other and from the developer.
  Socket dir is `/tmp/p2pmux-<uid>` and is *not* HOME-derived, but names are random ids.
- The sandbox `HOME` has no saved display name, so `create`/`join` **must** be passed
  `--name <name>` or they will block prompting for one.
- Every read has a deadline (`wait_for` / `wait_until` / `settle`); every peer gets
  SIGTERM-then-SIGKILL on its process group. A hung peer cannot hang an iteration.

## Scenarios run

### Iteration 0 — harness bring-up (2026-07-28)

Building and smoke-testing the driver was the entire iteration, per the loop's rule.
No product scenario was run and no product bug was fixed.

| # | What | Result |
|---|------|--------|
| 0.1 | `p2pmux local` on a 100x30 PTY renders a real frame | PASS — shell prompt drawn |
| 0.2 | Typed `echo SMOKE-$((6*7))-OK`, read `SMOKE-42-OK` back off the PTY | PASS — keystrokes reach the child shell |
| 0.3 | RSS sampling for a peer plus its children | PASS — ~11.6 MiB |
| 0.4 | `resize(120x40)` mid-session delivers a real `SIGWINCH` | PASS — peer alive, screen changed |
| 0.5 | A `wait_for` on a string that never appears | PASS — `DeadlineExceeded` at 1.5s, no hang |
| 0.6 | `p2pmux create --name hostuser` prints a join code | PASS — e.g. `6HPC8V94PD` |
| 0.7 | Session store + rendezvous land in the sandbox `HOME` | PASS |
| 0.8 | The detached `__node` worker is actually forked | PASS — 2 new pids |
| 0.9 | Teardown reaps the detached node | PASS — `leaked=[]` |
| 0.10 | Scenario raising mid-flight still triggers full cleanup | PASS — `leaked=[]` |

Stability: smoke run 3x, 10/10 checks each time. Zero `target/release/p2pmux` processes
left behind afterwards.

Repro: `cargo build --release && python3 scripts/e2e/smoke.py`

**One harness defect found and fixed (harness only, not p2pmux):** `settle()` treated an
all-blank screen as "settled", so it returned an empty frame ~0.55s in, before p2pmux had
drawn its first frame — silently breaking any assertion made against it. `settle()` now
takes `require_content=True` by default and `wait_ready()` waits for the first non-blank
frame. Confirmed the underlying render was always correct by dumping the raw PTY bytes
(`\x1b[?1049h` alt-screen enter, then the prompt).

### Iteration 1 — A×A: child exits under a watching guest, then guest types into it (2026-07-28)

Script: `scripts/e2e/scenario_a_exit.py`. Two real peers, 100x30 each.

Repro:
1. `p2pmux create --name host` → read the join code off the host status bar.
2. `p2pmux join <code> --name guest`, wait for `Pane #1` to render.
3. Host types `echo WATCHING-<n>` + Enter.
4. Host types `exit` + Enter — the pane's child shell dies while the guest watches.
5. Guest types `GHOSTKEY<n>` + Enter into the now-exited pane.

| Check | Result |
|---|---|
| Guest sees the host child's output | PASS |
| Host and guest render byte-identical pane bodies | PASS |
| Host footer shows `exited — close with Ctrl+P, X` | PASS |
| Guest footer shows `exited — input disabled; pane host can close with Ctrl+P, X` | PASS |
| Both peers survive the child exit | PASS |
| Guest keystrokes never appear in the exited pane (guest view) | PASS |
| Guest keystrokes never reach the host's exited pane | PASS |
| Exited pane content unchanged by the rejected input, both views | PASS |
| No panic text on either PTY | PASS |
| RSS across the scenario | PASS — 76.6 MB → 76.1 MB, no growth |

**No bug found.** 5/5 runs fully clean (1 + 4 repeats), zero orphans. Input rejection is
real, not cosmetic: `input_allowed()` (`src/tui.rs:4521`) gates on `!pane.exited`, and the
guest's bytes changed nothing on the *host's* copy of the pane either — so they were
dropped before the wire, not swallowed at the render layer.

Escalating: next scenario targets severity-2 ownership outcomes rather than happy path.

**Second harness fidelity fix (harness only):** pyte does not implement the alternate
screen buffer, so the multi-line TRUST WARNING that `create`/`join` print before the TUI
starts stayed on the grid and bled through wherever the TUI did not repaint — indis-
tinguishable from a garbled render, i.e. a false-positive generator for exactly the bug
class this loop hunts. `driver.AltScreen` now clears on `ESC [ ? 1049 h/l`.

### Iteration 2 — B×A: a guest tries to destroy what it does not own (2026-07-28)

Script: `scripts/e2e/scenario_b_ownership.py`. Two real peers. All severity-2 territory.

Repro:
1. Host creates, guest joins.
2. Guest presses `Ctrl+P, n` → guest-hosted pane; tab now holds one pane per owner.
3. Guest presses `Ctrl+P, ←` to focus the host's pane.
4. Guest presses `Ctrl+P, x` (delete a live foreign pane).
5. Guest presses `Ctrl+T, x` + Enter (delete a tab containing a foreign pane).
6. Host types `exit` — its pane is now an *exited* foreign pane.
7. Guest presses `Ctrl+P, x` again on the exited foreign pane.
8. **Positive control:** host presses `Ctrl+P, x` on its own exited pane.

| Check | Result |
|---|---|
| Both peers see two panes with the right owners | PASS |
| Guest cannot delete a live foreign pane | PASS |
| Guest cannot delete a tab containing a foreign pane | PASS |
| Tab survives the rejected tab delete | PASS |
| Host's pane reports exited | PASS |
| **Guest cannot delete an EXITED foreign pane** | PASS |
| **Positive control: pane host CAN delete its own exited pane** | PASS |
| No panic, both peers alive | PASS |

**No bug found.** 4/4 runs clean, zero orphans. Enforcement is server-side in
`layout.rs:747` (`LayoutError::NotPaneHost`), keyed on the *authenticated* peer id, so a
guest cannot forge it client-side. The positive control matters: it proves the rejections
are real authorization outcomes and not a chord that silently never fired.

Two failures in the first run were **my oracles, not p2pmux**:
- `Pane #N` is a display *ordinal* (`src/tui.rs:571`), not a stable id — deleting the first
  pane renumbers the survivor to `#1`, which an id-keyed oracle reports as a phantom
  failure. The oracle now keys on owner + count.
- The scenario had the guest type into the host's pane before the host typed `exit`. That
  grabs the pane's control lease, so the host's own `exit` was then correctly refused.
  Reordered so ownership is tested without lease contamination.

### Discovery — silent input discard when another peer holds the lease

Found while diagnosing the above; **not yet classified as a bug**, recorded for iteration 3.

`src/tui.rs:6715-6726` handles typing into a pane you do not control:

```
if you are the controller            -> send
else if controller is empty (free)   -> send, implicitly grabbing control
else if lease idle >= IDLE_AFTER     -> buffer into held_input, request take-control
else                                 -> (no branch: bytes are silently discarded)
```

So while another peer actively holds control, your keystrokes vanish with **no feedback of
any kind** — no bell, no footer notice. Compare the exited-pane path, which does explain
itself (`exited — input disabled; pane host can close with Ctrl+P, X`). The pane header
does show `control: <peer>`, which is the only indication. Confirmed by hand: after the
guest typed once into the host's idle pane, the host typed `exit` into *its own* pane and
nothing whatsoever happened.

Refusing is per design (`lease.rs`, `IDLE_AFTER = 30s`; the bank calls for "explicit
handoff required"). The open question is the missing feedback, and — more interesting —
whether `held_input` replay after the handoff is lossless. That is iteration 3.

### Iteration 3 — B: control-lease handoff and input integrity (2026-07-29)

Script: `scripts/e2e/scenario_c_lease.py`. Two real peers. Severity-3 target: the
`held_input` buffer-and-replay path in `src/tui.rs:6717-6725`, which is where a handoff
would drop, duplicate, or reorder keystrokes. ~85s per run (two 30s idle waits).

Repro:
1. Host creates, guest joins, guest makes its own pane (`Ctrl+P, n`).
2. Guest focuses the host's pane (`Ctrl+P, ←`) and types `HOPIN<n>` — control is free,
   so this is the no-approval idle hop-in.
3. Host immediately types `BLOCKED<n>` into its *own* pane while the guest holds the lease.
4. Wait 35s for the lease to go idle. Host types `echo Q1W2E3R4T5Y6U7I8O9P0` at ~2ms/key,
   fast enough that the bytes straddle the take-control round trip and land in `held_input`.
5. Wait 35s again, then host and guest type `AAAAAAAAAA` / `BBBBBBBBBB` simultaneously.

| Check | Result |
|---|---|
| Idle hop-in: guest input reaches the host's pane with no approval | PASS |
| Both peers agree the guest is now controller | PASS |
| Host's own input is refused while the guest holds the lease | PASS |
| Host reclaims control once the lease goes idle | PASS |
| **Handoff input is not duplicated** (exactly 2 occurrences: typed + echoed) | PASS |
| **Handoff input is not reordered or partially dropped** | PASS |
| Guest renders the same reclaimed-pane content as the host | PASS |
| **Racing peers do not interleave into one command line** | PASS |
| **Exactly one peer wins the control race** | PASS |
| No panic, both peers alive | PASS |

**No bug found.** 4/4 runs clean, zero orphans. `held_input` replay is lossless: the
20-character non-repeating handoff string arrived intact, in order, exactly once, and the
simultaneous race produced no interleaving.

Confirms the iteration-2 discovery by measurement: the harness explicitly checked for any
visible reaction to the refused keystrokes and saw **none** (`visible reaction? False`).
Filed under Cosmetic below rather than Open bugs — the refusal itself is correct per
design, only the feedback is missing.

### Iteration 4 — F×D: typing fast into a 10k-line flood with a guest watching (2026-07-29)

Script: `scripts/e2e/scenario_d_load.py`. Two real peers, guest watching throughout.

Repro:
1. Host creates, guest joins.
2. Host runs `seq 1 10000`.
3. **150 ms later, while the flood is still streaming**, host types
   `echo TYPED-DURING-STREAM-Q1W2E3R4T5` at ~1ms/key. The shell cannot read it until the
   burst ends, so the keystrokes sit in the PTY input queue across the entire flood.
4. Let both peers settle, then run a second identical `seq 1 10000`.

| Check | Result |
|---|---|
| Input typed during the flood still executes | PASS |
| The queued command is not mangled by the flood | PASS |
| The queued command is not duplicated | PASS |
| The flood reached its last line (10000) | PASS |
| Host and guest render identical pane bodies after the flood | PASS |
| A second identical flood does not grow memory again | PASS |
| No panic, both peers alive | PASS |

**No bug found.** 3/3 runs clean, zero orphans.

**Memory characterisation** (measured over four consecutive floods, not a bug):

| stage | host | guest | total |
|---|---|---|---|
| baseline | 39.8 MB | 37.9 MB | 77.6 MB |
| after flood 1 | 162.7 MB | 38.4 MB | 201.1 MB |
| after flood 2 | 163.3 MB | 38.4 MB | 201.7 MB |
| after flood 3 | 163.4 MB | 38.4 MB | 201.8 MB |
| after flood 4 | 163.4 MB | 38.4 MB | 201.8 MB |

Two things worth knowing:
- Scrollback memory is **bounded, not leaking**. The first flood costs ~123 MB on the pane
  host as the scrollback buffer reaches capacity; every later flood is free (+0.6 MB total
  across three more). The scenario now asserts this boundedness invariant instead of an
  arbitrary absolute ceiling.
- **Only the pane host retains scrollback.** The watching guest grew 0.5 MB for the same
  10k lines, consistent with the on-demand-scrollback design.
- Open question for a human, not pursued here: ~123 MB per flooded pane is chunky. If that
  cost is *per pane* rather than shared, a busy 8-member session with several flooded panes
  could get expensive. Worth a deliberate multi-pane measurement.

### Iteration 5 — F×C×B: stalled-viewer resync, then the coordinator killed (2026-07-29)

Script: `scripts/e2e/scenario_e_failure.py`. **Three** real peers (host + g1 + g2).

Note on what "killing the host" means: `create`/`join` fork a detached `__node` that owns
the PTYs; the foreground peer is only a renderer. Killing the peer is a *detach*; killing
its node is the real disconnect. This scenario SIGKILLs the coordinator's **node**.

Repro:
1. Host creates, g1 and g2 join; all three confirm a shared baseline marker.
2. `SIGSTOP` g2, then host runs `seq 1 3000`.
3. Assert g1 keeps up to line 3000 while g2 is frozen (slow-viewer path).
4. `SIGCONT` g2, assert it converges on the host's exact pane body.
5. `SIGKILL` the coordinator's node with both guests attached.
6. Probe each guest's liveness by resizing it and requiring a redraw at the new width.

| Check | Result |
|---|---|
| All three peers share a baseline | PASS |
| A healthy guest keeps up while another peer is stalled | PASS |
| The stalled peer renders nothing while stopped | PASS |
| **The stalled peer resyncs to identical content after SIGCONT** | PASS |
| **The resumed peer did not replay a duplicated tail** | PASS |
| Both guests survive the coordinator being killed | PASS |
| **Both guests' render loops stay responsive** (resize → redraw) | PASS |
| No panic on any peer | PASS |
| **Guests are told the coordinator disconnected** | **FAIL — 3/3 runs** |

The resync path is genuinely good: a peer stopped through 3000 lines of output converges
byte-for-byte on resume with no duplicated tail, and killing the coordinator wedges
nothing — both guests keep rendering and answer a resize.

Deliberately *not* tested here: the B-category disconnect-grace items (placeholders, grace
expiry, coordinator failover, old coordinator rejoining). Reading the source, **none of
that is implemented yet** — there is no grace window, no placeholder pruning and no
failover anywhere in `src/`, consistent with the README calling them "later work". So
there is no hidden 5-minute timer to shrink for tests, and no configurable-grace flag was
needed. Those bank entries are unimplemented features, not failing behaviour.

### Iteration 6 — E×D: mouse forwarding and fidelity across mismatched sizes (2026-07-29)

Script: `scripts/e2e/scenario_f_mouse.py`. Host at **120x40**, guest at **64x20** — the
pane grid is host-owned, so a much smaller guest must track the stream without corrupting
it. Mouse input is sent as real SGR (1006) reports, the encoding the client enables.

The interesting part is that p2pmux must choose between two incompatible wheel behaviours
*per pane state*: scroll its own scrollback, or forward the wheel to a child that scrolls
itself. Get it backwards and either the user cannot scroll back, or a full-screen app never
sees the wheel. Both are exercised on the **same pane, same peer**, so the decision cannot
be made once at startup.

Repro:
1. Host creates at 120x40, guest joins at 64x20.
2. Host runs `echo '日本語 ABC 🙂🎉 café ĄŻ'`.
3. Host runs `seq 1 200`, then wheel-up ×5 at (20,10), then wheel-down ×8.
4. Host runs `printf '\033[?1000h\033[?1006h'` so the child now reports mouse.
5. Wheel-up ×3 again on that same pane.

| Check | Result |
|---|---|
| Unicode/wide/emoji reaches a much smaller guest | PASS |
| Wide characters are not mangled on the host | PASS |
| Wheel scrolls p2pmux's own scrollback when the child does not report mouse | PASS |
| Wheel-up moves backwards through history, not forwards | PASS |
| **Wheel-down returns exactly to the live bottom** (byte-identical screen) | PASS |
| **Wheel is forwarded once the child reports mouse** | PASS |
| **p2pmux scrollback does NOT move while the child owns the wheel** | PASS |
| No panic, both peers alive | PASS |

**No p2pmux bug found.** 4/4 runs clean, zero orphans. Wheel-up moves back exactly 3 lines
per notch and wheel-down clamps to a byte-identical live bottom.

Two harness defects found and fixed (mine, not p2pmux's):
- **pyte crashes on emoji.** `Screen.display` does `char[0]` on every cell and raises
  `IndexError` on the empty continuation cell a wide character leaves behind
  (`pyte/screens.py:241`). Since these scenarios stream CJK and emoji on purpose, the
  harness now renders rows itself, wide-char aware, via `Peer._render_row`.
- **Wrong mouse oracle.** The check looked for `[<64;` in the pane. The pane's own vt100
  parser consumes the leading `ESC [ <` as an escape sequence, so a forwarded report lands
  on screen as `64;19;6M`. Confirmed forwarding was working all along by reading the raw
  PTY bytes before changing anything.

### Iteration 7 — C: room lifecycle, bad codes, dead rooms, the 8-member cap (2026-07-29)

Script: `scripts/e2e/scenario_g_lifecycle.py`. Up to **9 real peers** (18 processes, since
each peer is a client plus its detached node). Each phase gets its own Harness, so join
codes cannot leak between phases and cleanup is incremental.

`MAX_MEMBERS` is 8 and the host counts as a member (`src/layout.rs:30,361`), so the cap is
host + 7 guests and the 9th must be refused.

Repro:
1. `p2pmux join ZZZZZZZZZZ` — a code that was never valid.
2. Create a room, SIGKILL its coordinator node, then join with the still-published code.
3. Two peers spawned back to back against the same code.
4. Host + 7 guests admitted, then an 8th guest attempts to join.

| Check | Result |
|---|---|
| A bad join code exits instead of hanging | PASS |
| A bad join code exits non-zero | PASS |
| A bad join code explains itself | PASS |
| **Joining a dead room terminates instead of hanging forever** | PASS |
| Joining a dead room reports an error | PASS |
| Both simultaneous joiners get in | PASS |
| Both simultaneous joiners receive the host's output | PASS |
| **All 7 guests fit under the 8-member cap** | PASS |
| **The 9th member is refused, not admitted** | PASS (refused in 0.2s) |
| The over-cap peer does not hang silently | PASS |
| The refusal is not a raw internal debug error | PASS *(after the fix below)* |
| **The over-cap peer explains why it was refused** | **FAIL — 3/3, see BUG-3** |
| No panic across the full room | PASS |

The cap itself is enforced correctly end to end, and nothing hangs: every doomed join
exits rather than blocking, which was the severity-1 question behind this whole category.

One harness false-pass caught and fixed: the "explains why" check originally searched the
whole PTY output for `"full"`, which matches **"fully trusted"** in the TRUST WARNING that
p2pmux prints on every join. It now scopes the search to the error line only.

### Iteration 8 — D×C: full-screen TUI, mid-stream resize, paste, reattach (2026-07-29)

Script: `scripts/e2e/scenario_h_fidelity.py`. Two peers at 100x28.

A full-screen TUI is the hardest thing to replicate — `less` drives the alternate screen,
absolute cursor moves and full repaints — so any divergence shows immediately. The nasty
part is resizing the *guest's* whole terminal while that TUI is live: the pane grid is
host-owned, so the guest must crop without corrupting the stream or disturbing the host.

Repro:
1. Host runs `seq 1 500 | less`; compare host and guest pane bodies.
2. Host presses space (page down); compare again.
3. Resize the guest to 70x20, wait, resize back to 100x28.
4. Host quits `less`, presses `Ctrl+P, n` to split.
5. Bracketed paste of 6 `echo PASTE<n>-<k>` lines wrapped in `ESC[200~ … ESC[201~`.
6. Guest presses `Ctrl+Q`, then a new peer runs `p2pmux attach <session-name>`.

| Check | Result |
|---|---|
| **Host and guest render the full-screen TUI identically** | PASS |
| Paging inside the TUI stays in sync on the guest | PASS |
| Guest survives a mid-stream terminal resize | PASS |
| **Host is undisturbed by the guest's resize** | PASS |
| **Guest reconverges on the host's view after resizing back** | PASS |
| A split is replicated to the guest | PASS |
| **Every line of a multi-line paste arrives** | PASS |
| **Pasted lines are not reordered** | PASS |
| Guest client exits on Ctrl+Q | PASS |
| **Guest can reattach after quitting** | PASS |
| **The reattached guest sees the pasted history, not a blank pane** | PASS |
| No panic on any peer | PASS |

**No bug found.** 4/4 runs clean, zero orphans.

#### BUG-3 revisited — cheaper fix attempted, measured, and rejected

With no new bug to fix, this iteration went back at BUG-3 rather than deferring twice.

**Sharper root cause than the original plan.** The coordinator sends `Welcome` *first*
(`session.rs:1774`) and only afterwards calls `admit_with_display_name` (`session.rs:2270`),
where `MemberLimit` fires. So an over-cap peer was being told it was **admitted** and then
dropped — the refusal was not just unexplained, it was contradicted.

**Landed (server-side, no wire change).** `LayoutCoordinator::is_full()` plus a capacity
check *before* the handshake welcomes anyone, and `SessionError::SessionFull` ("this
session is full (8 members)"). The join connection now closes carrying that reason instead
of the empty `b""` it used before. A peer is no longer welcomed and then discarded.

**Measured, and it was not enough.** The refused joiner still reports
`transport error: Iroh stream read failed: read error: connection lost` — its stream read
fails before the connection-level close reason is surfaced, so the reason never reaches
the user. **BUG-3 stays open**, now with evidence that the close-reason shortcut does not
work and the explicit refusal *frame* in the original plan really is required.

Regression: full suite 385 tests 0 failures; scenario G 2/2 apart from the known BUG-3
check; the cap still admits exactly 7 guests and refuses the 9th.

## Coverage caveat on the loop's stop rule

Iterations 1-3 each found no bug, which technically fires the "three consecutive clean
iterations → stop" rule. Noting explicitly that this is **not** saturation of p2pmux:
those three iterations were all inside categories A and B (which the loop asked to be
prioritised, being the newest code). Categories C (room lifecycle), D (terminal fidelity),
E (mouse forwarding) and F (load and failure) have **not been touched at all**. The right
reading is "A and B look solid", so the loop continues into the untouched categories.

## Open bugs

### BUG-3 (severity 5) — a refused joiner is not told the room is full

**Symptom.** The 9th member is correctly refused, but is told
`Error: transport error: Iroh stream read failed: read error: connection lost`. Truthful
and no longer misleading, but it never says the room is full. Reproduced 3/3.

**Root cause.** The coordinator computes the right reason — `LayoutError::MemberLimit`
(`layout.rs:362`), which `reject_reason` maps to `LayoutRejectReason::Limit`
(`session.rs:982`) — but that mapping serves *layout requests* made by admitted members.
The join handshake has no such path: a peer refused at `add_member` simply has its
connection dropped, so the joiner only observes a transport-level close.

**Status after iteration 8.** The cheaper, non-protocol fix was attempted and measured:
the coordinator now refuses a full session *before* welcoming (previously it welcomed the
peer and then dropped it) and closes carrying the reason. That did **not** surface to the
user — the joiner's stream read fails with "connection lost" before the close reason is
read — so the explicit refusal frame below is genuinely required. Still open.

**Escape hatch taken — this needs a protocol change, so it is not fixed here.** Making it
right means the coordinator sends a structured refusal on the control stream *before*
closing, and the joiner surfaces it. That touches the join handshake and the wire format,
which is more than one iteration should quietly change under a stress-test loop.

**Plan for a human.**
1. Add a terminal `LayoutControlEvent::Refused { reason }` (or reuse `LayoutReject` with a
   handshake-scoped request id) emitted by the coordinator's join path on `MemberLimit`.
2. Send it before `disconnect_or_remove` drops the connection.
3. In `join_layout_with_display_name` (`session.rs:2634`), translate a received refusal
   into a typed `SessionError::Refused(reason)` instead of surfacing the read failure.
4. Map that to a plain sentence in `cli.rs`, e.g. `this session is full (8 members)`.

Worth doing beyond the cap: the same channel would carry any future refusal reason
(banned peer, version mismatch) rather than every one of them looking like a network fault.

## Fixed

### BUG-2 (severity 5) — every CLI error printed as a Rust debug dump, hiding the real cause

**Symptom.** A user joining a full room was told:

```
Error: Custom { kind: TimedOut, error: "background node did not become ready" }
```

Two failures in one line. It is a raw `Debug` dump of Rust internals, and it points at a
*local startup problem* when the real cause was a remote refusal — actively misleading.

**Root cause, two layers.**
1. `main` returned `Result<(), Box<dyn Error>>`, and Rust prints that error with **Debug**,
   not Display — so every CLI error surfaced as a struct dump.
2. `create`/`join` fork a detached node whose stderr is `/dev/null` (`cli.rs:440`). When it
   failed to start, its reason went nowhere, and `launch_background_node` could only report
   that the socket never appeared — a timeout message standing in for the real error.

**Fix.**
- `src/main.rs` prints `Error: {error}` via Display and returns `ExitCode::FAILURE`, so
  every error in the binary shows the message it actually carries.
- The node writes its startup failure to `<session-id>.error` beside its bootstrap file;
  `launch_background_node` polls for it and surfaces it verbatim as `NodeStartupError`,
  falling back to the old timeout only when the node left no reason. Stale files are
  removed before spawn.

**Verified.** `p2pmux join ZZZZZZZZZZ` now prints `Error: join code was not found on this
Mac`. The over-cap joiner reports a real transport error instead of a debug struct, 3/3
runs. Regression: scenario A 2/2, full suite 385 tests, 0 failures.

Same class of defect as BUG-1: the software computed the right information and had no wire
to carry it to the user.

### BUG-1 (severity 4) — every status message is invisible in the default run mode

**Symptom.** Kill a session's coordinator and the guests are never told. 20+ seconds later
each guest still renders `Pane #1 host: host control: host` over stale content, with the
ordinary hint bar in the footer. A user cannot distinguish "the host is idle" from "the
host's machine is gone". Reproduced **3/3 runs** — deterministic, not flaky.

**Root cause — not a render bug, a missing wire.** `SharedLayoutRuntime` keeps a
`status: String` (`src/tui.rs:4338`) and sets it correctly on exactly the events you would
want: `layout coordinator disconnected` (`tui.rs:5168`), `pane {id} disconnected; retrying`
(`tui.rs:5032`), `pane spawn failed` (`tui.rs:5654`), `pane registration failed`
(`tui.rs:5673`), `pane {id} has no usable host address` (`tui.rs:5255`), and more.

But `status` is only ever *read* at `tui.rs:4725/4807/4837`, inside `SharedLayoutRuntime`'s
own drawing code — which is the **legacy foreground path**. Since `create`/`join` default
to the node+client split (`cli.rs:150`, `cli.rs:249`), the runtime now executes headless
inside the node and the client does the rendering. The string `status` does not appear
anywhere in `src/node.rs`, `src/client.rs`, or `src/protocol.rs`, and `NodeMessage`
(`local_ipc.rs:67`) has no status variant. `node_snapshot()` (`tui.rs:4531`) does not carry
it either.

So in the mode every real user runs, the entire status channel is dead code. This is wider
than the disconnect case: *every* one of those operator-facing error messages is computed
and then dropped on the floor.

**Fixed** in commit below. One-line root cause: *the node computes `status` but no wire
carried it to the client, so in the default node+client mode the whole status channel was
dead code.* The fix adds the missing propagation rather than papering over it at the
render layer:

- `NodeMessage::Status { message }` added to `src/local_ipc.rs`.
- `SharedLayoutRuntime::status()` exposed (`src/tui.rs`) and surfaced on the node wrapper.
- Published from the node's existing change-detection loop in `queue_updates`
  (`src/node.rs`), in the same shape as `Layout`/`Leases`/`Rosters`, so it is sent only
  when it actually changes; `reset_for_snapshot` clears it so a reattaching client
  re-receives it.
- `src/client.rs` renders it through the `footer_notice` slot it already had. An empty
  message retracts the notice instead of flashing a blank one.

**Verified.** Same repro, 3/3 runs now pass. Measured latency to the user after the
coordinator is SIGKILLed:

| notice | appears at |
|---|---|
| `pane 1 disconnected; retrying` | ~34.6s |
| `layout coordinator disconnected` | ~47.6s |

Before the fix: never, at any elapsed time.

Regression check: scenario A (2/2 clean) and the full `cargo test --release` suite
(385 tests, 0 failures).

**Follow-up, not fixed here (separate concern).** ~35s to first notice is the transport's
dead-peer detection, not the propagation this fixed. Whether that idle timeout should be
shorter — or whether the UI should show "no packets from host for Ns" sooner — is a design
call worth a human's judgement.

## Cosmetic / by-design gaps (not bugs)

- **Silent input discard while another peer holds a pane's lease.** `src/tui.rs:6715-6726`
  has no `else` branch, so keystrokes typed into a pane someone else actively controls are
  dropped with no bell, no footer notice, and no other reaction (measured, iteration 3).
  The only indication is the `control: <peer>` badge in the pane header. Refusing is
  correct per `lease.rs` (`IDLE_AFTER = 30s`); the gap is purely the missing feedback, and
  it is inconsistent with the exited-pane path which does explain itself. Severity 5.

## Flaky

_None yet._
