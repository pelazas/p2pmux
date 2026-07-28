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

## Open bugs

_None yet._

## Fixed

_None yet._

## Flaky

_None yet._
