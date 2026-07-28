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

## Coverage caveat on the loop's stop rule

Iterations 1-3 each found no bug, which technically fires the "three consecutive clean
iterations → stop" rule. Noting explicitly that this is **not** saturation of p2pmux:
those three iterations were all inside categories A and B (which the loop asked to be
prioritised, being the newest code). Categories C (room lifecycle), D (terminal fidelity),
E (mouse forwarding) and F (load and failure) have **not been touched at all**. The right
reading is "A and B look solid", so the loop continues into the untouched categories.

## Open bugs

_None._

## Cosmetic / by-design gaps (not bugs)

- **Silent input discard while another peer holds a pane's lease.** `src/tui.rs:6715-6726`
  has no `else` branch, so keystrokes typed into a pane someone else actively controls are
  dropped with no bell, no footer notice, and no other reaction (measured, iteration 3).
  The only indication is the `control: <peer>` badge in the pane header. Refusing is
  correct per `lease.rs` (`IDLE_AFTER = 30s`); the gap is purely the missing feedback, and
  it is inconsistent with the exited-pane path which does explain itself. Severity 5.

## Fixed

_None yet._

## Flaky

_None yet._
