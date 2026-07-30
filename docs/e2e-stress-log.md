# p2pmux end-to-end stress log

Hand-driven end-to-end stress testing of p2pmux: real `target/release/p2pmux` binaries on
real PTYs, driven with real keystrokes, asserted against the bytes they actually rendered
back. No unit tests, no mocks, no p2pmux-inside-tmux.

Harness: `scripts/e2e/driver.py` (Python `pty` + `pyte`), smoke test `scripts/e2e/smoke.py`.
Branch: `e2e-stress`.

## Standing caveats

- **Localhost only, for scenarios A–K.** Those peers all run on one Mac, exercising the direct
  transport and the local rendezvous join-code file, and nothing in them should be read as
  "P2P is tested". Scenarios **L**, **M**, **N** and **O** (addenda below) are the
  exception: they run over the public internet against DigitalOcean droplets, with latency,
  loss, and forced relay applied on the Linux side, and each asserts which path the session
  actually took. N removes this Mac from the session entirely; O puts it back in the middle of
  two droplets, behind its own router. Still uncovered anywhere: carrier-grade NAT, which needs
  a Mac on a phone hotspot and a pair of hands.
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

### Iteration 9 — A×B: a focused pane exits and is deleted under another peer (2026-07-29)

Script: `scripts/e2e/scenario_i_focused_exit.py`. **Three** peers: `host` owns pane 1,
`g1` owns pane 2 and is the one that exits and deletes it, `g2` holds pane 2 focused
throughout. ~35s per run (one lease-idle wait).

The nasty part is the deletion. A pane vanishing out from under a peer that has it
*selected* is the classic way to strand focus on an id that no longer exists: the peer
looks fine but every keystroke goes nowhere. So the decisive check is not "did it render"
but "can that peer still type afterwards".

Repro:
1. `g1` presses `Ctrl+P, n` → pane 2 is g1-hosted.
2. `g2` presses `Ctrl+P, →` and types `FOCUS<n>` — proves focus landed on pane 2.
3. Wait out the 30s control lease (see the harness note below).
4. `g1` types `exit` into its own pane 2, which dies while g2 still has it focused.
5. `g2` types `GHOST<n>` into the exited pane.
6. `g1` presses `Ctrl+P, x`, deleting the pane while g2 still has it focused.
7. `g2` types `RECOVER<n>`.

| Check | Result |
|---|---|
| All three peers see both panes with the right owners | PASS |
| g2 has g1's pane focused (its typing landed there) | PASS |
| **The peer holding the pane focused sees the exited notice** | PASS |
| Input from the focusing peer into the exited pane is rejected | PASS |
| The pane is gone on every peer | PASS |
| **No peer died when the focused pane was deleted** | PASS |
| **g2 can still type after its focused pane was deleted (focus not stranded)** | PASS |
| g2's recovered typing is visible to the other peers too | PASS |
| No panic on any peer | PASS |

**No bug found.** 4/4 runs clean, zero orphans. Focus recovers cleanly when the selected
pane is deleted by its owner — g2 keeps typing and the other peers see it.

Harness note (my error, not p2pmux): the first run failed two checks because `GHOST<n>`
came back as `command not found` — the shell was still alive, so the pane had never
exited. g2's hop-in in step 2 had taken pane 2's **control lease**, so `g1`'s `exit` into
its own pane was correctly refused. The scenario now waits out `lease.rs IDLE_AFTER` (30s)
before the exit. Focus and control are separate: g2 keeps the pane *focused* across that
wait without holding its lease.

### Iteration 10 — B×F: three peers racing structural edits under load (2026-07-29)

Script: `scripts/e2e/scenario_j_edit_storm.py`. Three peers at 140x40. **Final iteration.**

The layout is a revisioned tree and every structural request carries a `base_revision`
(`layout.rs:546`), so three peers splitting at the same instant means most requests race a
revision that moved under them. The coordinator must reject the losers with
`StaleRevision` rather than applying them twice or corrupting the tree. The invariant that
matters is **convergence** — peers disagreeing about the layout is the worst multiplayer
outcome short of a crash, since a guest deleting "pane 2" would hit a different pane than
the one it can see.

Repro:
1. Host starts `seq 1 4000` streaming, so the storm races real pane traffic.
2. Four rounds of: all three peers send `Ctrl+P` together, then `n` together.
3. Host echoes a marker; every peer must see it.
4. `g1` and `g2` send `Ctrl+T` then `n` at the same instant.

| Check | Result |
|---|---|
| **All three peers converge on the same panes after the split storm** | PASS |
| The pane count is sane, not doubled or corrupted | PASS |
| Every pane still has a real owner | PASS |
| No peer died during the storm | PASS |
| The session still streams to every peer after the storm | PASS |
| **All three peers converge on the same tab list** | PASS |
| No runaway memory from the rejected edits | PASS |
| No panic on any peer | PASS |

**No bug found.** 4/4 runs clean, zero orphans. Concurrency control is doing exactly its
job: 12 racing split requests (4 rounds × 3 peers) settled to 5 panes — 4 winners, 8
correctly rejected as stale — with all three peers landing on a byte-identical layout. Two
simultaneous tab creations produced exactly one new tab, not two.

Harness false-positive caught and fixed (mine, not p2pmux): the first run reported the
peers "diverging", but host was viewing Tab #1 while g1 had created Tab #2 and switched to
it. **Which tab a peer is viewing is per-peer UI state, exactly like focus** — comparing
whatever tab happened to be visible was comparing different tabs. The oracle now compares
panes while all peers are on the same tab, and compares the *tab list* separately, which
is meaningful regardless of selection.

## Final summary — loop stopped (2026-07-29)

Stopping per the loop's own rule: three consecutive iterations (8, 9, 10) found no new
bug, and no open bugs remain.

**Bugs found and fixed: 3.** All three were the *same defect shape* — p2pmux computed the
correct information and had no channel to carry it to the user:

| | Bug | Root cause |
|---|---|---|
| BUG-1 | Guests never told the coordinator died | `status` existed but no wire carried it under the node+client split |
| BUG-2 | Every CLI error printed as a Rust debug dump | `main` returned `Result`, so Rust printed Debug; detached node's failure went to `/dev/null` |
| BUG-3 | Refused joiner not told the room is full | Welcome sent before the limit check; then p2pmux rejected its own refusal frame (`request_id` 0) |

Three instances is a structural pattern worth a design pass, not three coincidences.

**What was verified as genuinely solid** (each measured, not assumed):
- Ownership enforcement is server-side on the *authenticated* peer id; guests cannot delete
  foreign panes, foreign tabs, or exited foreign panes. Proven with a positive control.
- Input rejection on exited panes is real — the bytes never reach the host's copy either.
- Control-lease handoff is lossless: a 20-char string typed at ~2ms/key across the
  take-control round trip arrived intact, in order, exactly once; races produce one winner.
- A peer SIGSTOPped through 3000 lines resyncs byte-identically with no duplicated tail.
- Killing the coordinator wedges nothing; both guests keep rendering and answer a resize.
- Scrollback is bounded (~123 MB first flood, then free) and retained only by the pane host.
- `less` replicates byte-identically including paging; guest resize crops and reconverges.
- Wheel routing is decided per pane state, correctly, both ways in one session.
- Concurrent structural edits converge across three peers under streaming load.

**Harness defects found: 8** — every one would have produced a false bug report. Worth
noting: five of the eleven total defects this loop surfaced were in the *test harness*, not
p2pmux. The recurring lesson was to read the raw PTY bytes before believing an oracle.

### What this harness structurally cannot reach — for a human, by hand

- **Two real machines over the real internet.** ~~Everything here is localhost.~~
  *Closed by scenarios L, N and O (addenda below), which run over the public internet and assert
  the path taken; forced relay is covered by `--force-relay`.* What remains open is NAT traversal
  from behind **carrier-grade** NAT — symmetric NAT and hairpinning — which a home router and a
  public-IP droplet cannot exercise.
- **Real latency, loss, and reordering.** No jitter, no packet loss, no MTU/path-MTU
  effects. The lease handoff and resync paths were only ever exercised at ~0ms RTT.
- **Sleep/wake and network change.** Close the laptop mid-session; switch Wi-Fi → cellular;
  change networks while a pane streams. Connection migration is untouched here.
- **The 30s idle lease with real people.** Two humans genuinely fighting over one pane,
  where "actively typing" is human-paced rather than scripted.
- **Long-lived sessions.** Nothing here ran longer than a few minutes. Leaks, revision
  counter growth, and scrollback behaviour over a working day are unknown.
- **Real workloads in shared panes.** vim with plugins, `htop`, `claude`, a full TUI IDE —
  tested only with `less`.
- **The disconnect-grace features, once written.** No grace window, placeholder pruning, or
  coordinator failover exists in `src/` yet, so those bank entries were unimplemented
  rather than failing. They will need their own pass when built.
- **BUG-2's `.error` file** is written to a fixed path per session id; a human should sanity
  check behaviour if two nodes ever collide on one id, which this harness never forced.

## Coverage caveat on the loop's stop rule

Iterations 1-3 each found no bug, which technically fires the "three consecutive clean
iterations → stop" rule. Noting explicitly that this is **not** saturation of p2pmux:
those three iterations were all inside categories A and B (which the loop asked to be
prioritised, being the newest code). Categories C (room lifecycle), D (terminal fidelity),
E (mouse forwarding) and F (load and failure) have **not been touched at all**. The right
reading is "A and B look solid", so the loop continues into the untouched categories.

## Addendum — Scenario L: the first session that left this Mac (2026-07-29)

Script: `scripts/e2e/scenario_l_internet.py`, helper `scripts/e2e/remote.py`. This retires the
first standing caveat above for everything it covers: one peer is now a DigitalOcean droplet in
ams3 (`p2pmux-lab`, ssh alias in `~/.ssh/config`), joined over the public internet.

How a remote peer works: `ssh -tt` allocates a PTY on the droplet and propagates our window size,
so the remote binary draws a real TUI whose bytes come back down the local PTY that `Peer` already
pumps into pyte. `Peer` gained one optional field, `launcher`; every existing scenario is
untouched. Two things a remote peer does not inherit — the sandbox `HOME` (passed explicitly over
ssh) and pid reaping (killing the local ssh does not kill the remote node, so `remote.reap()` runs
in a `finally`).

The droplet doubles as the network-impairment lab. `tc netem` shapes latency and loss and
`iptables` forces a relay, both on Linux, so the standing rule that the Mac's own interface is
never touched still holds.

The eight checks: the Mac creates a session and mints a ticket; the droplet joins with it; the
droplet creates its own pane and that pane reaches the Mac's layout; keystrokes execute on the
droplet's shell; the resulting PTY frame renders back here; both peers survive.

| run | result |
|---|---|
| default path, ×2 | 8/8, 8/8 |
| `--delay-ms=150` (netem, one-way egress) | 8/8 |
| `--force-relay` | 8/8 |
| `--force-relay --delay-ms=300` | 8/8 |

**No product code changed.** The Linux build was clean on the first attempt; the only fix needed
was to this Mac's stale `target/release` binary, which predated the `ticket` subcommand.

**A wrong first instrument, worth recording.** `--force-relay` initially dropped *all* inbound UDP
above 1024 and the droplet exited 1. That is not a relay-fallback failure: the relay path arrives
on an ephemeral port too, so the rule killed the very fallback it was meant to test. Scoping the
rule to the peer Mac's public address — hole-punched datagrams from that host vanish, every relay
server stays reachable — is the precise instrument, and it passes. Any future "relay is broken"
result should be checked against this mistake first.

**What this does not prove.** The droplet has a public IP and no NAT, so a direct path here is the
easy case; carrier-grade NAT still needs a Mac on a phone hotspot, by hand.

**Update, same day — the runs now say which path they took.** The paragraph that stood here said
the default runs could not distinguish "it worked" from "it worked directly", because nothing in
the codebase reported a path. Spike 4A closed that (see below), and scenario L gained three
checks: both peers must report the same path kind, that kind must be `direct` by default and
`relayed` under `--force-relay`, and the reported RTT must be consistent with the ICMP baseline.
Re-run at 11 checks: default 11/11 with both peers reading `direct 55ms` against a 32ms ICMP
baseline, and `--force-relay` 11/11 with both reading `relayed`. Holepunching Mac→droplet is now
a measured fact rather than an inference.

## Addendum — Spike 4A: the connectivity badge (2026-07-29)

`direct 55ms`, right-aligned in the tab bar; `relayed 120ms ×3` when several peers are connected
and one of them is worse. Iroh 1.0.3 is multipath QUIC, so the facts were already on hand:
`Connection::paths()` exposes the selected path, `TransportAddr::{Ip, Relay}` is the bit itself,
and `Path::rtt()` is the number. `src/transport.rs` samples the selected path once a second per
connection and forgets a peer when its connection closes, so a stale `direct 20ms` can never
outlive the link it described.

Three decisions worth keeping:

- **Worst path, not an average.** One peer stuck on a relay is the fact worth surfacing;
  averaging would hide it behind everyone else's healthy direct links.
- **RTT rounded up to 5ms.** The value is republished to the client whenever it changes, so raw
  millisecond jitter would have pushed an IPC message every sample for a digit nobody reads.
- **Tab bar, not footer.** The footer is transient — chords, copy feedback and status notices all
  take it over — and connectivity is exactly the fact you want visible at the moment something
  else has gone wrong. It stands down rather than overwrite a tab label.

Accepted connections must be handed to `Transport::observe` explicitly (five sites in
`session.rs`), because they are produced by awaiting an `Incoming` rather than by the transport
itself; only `connect()` can self-register. A missed site shows up as a peer with no badge.

## Addendum — Scenario M: locking the door on a live cross-machine session (2026-07-29)

Script: `scripts/e2e/scenario_m_lock.py`. Mac hosts, a droplet peer joins over the internet, and a
*second* droplet peer with its own sandbox HOME plays the stranger — remote on purpose, since a
locked door has to hold against someone arriving over the real network rather than over loopback.
14/14.

Two different locks turned out to be in play, and both are now covered:

- **Pane lock** (`Ctrl+P` then `k`) already existed and refuses input from anyone but the pane's
  host — but had only ever run on loopback, where a refusal and a slow network look identical.
  Scenario M now locks the Mac's pane, has the droplet type into it, and asserts the bytes never
  execute; then unlocks and asserts they do.
- **Session lock** (`Ctrl+P` then `Shift+L`) is new. The coordinator refuses peers it has never
  admitted, and says why. It governs the door only: peers already inside keep working, and a peer
  admitted once stays on the admitted roster so a transient reconnect is not exiled.

**Two real bugs, both invisible on localhost.**

1. *The refusal lost a race with the connection close.* `finish()` marks a stream complete but does
   not wait for its bytes to leave, and the caller closes the connection the moment the refusal
   returns an error. On loopback the reject frame usually escaped first; over a ~50ms path it
   usually did not, and the rejected peer printed `connection lost` — indistinguishable from a
   crashed host. Fixed by `drain_refusal`, a bounded wait for the joiner to hang up, which also
   repairs the pre-existing room-full refusal that had the same race.
2. *`Shift+L` could never fire.* `is_chord_command` rejected any key carrying a modifier, and SHIFT
   is how an uppercase letter is typed rather than a modifier the user chose to add. Every unit
   test passed while the feature was unreachable from a real keyboard. The filter now allows SHIFT
   for uppercase chord letters only, with a regression test.

**A third bug, found by an existing scenario.** Scenario F compares whole rendered screens to
check that wheel-down returns to the live bottom. The new badge carries a *live* RTT, so an
unmasked comparison now fails whenever the path jitters by 5ms between two snapshots. Two fixes:
`driver.mask_link_badge()` blanks the badge before any whole-screen comparison (same class of
oracle bug as keying on `Pane #N`, which is a display ordinal), and the badge itself stopped
printing `direct 0ms` for a sub-millisecond loopback path — it reads `<1ms`, because `0ms` looks
like a broken measurement rather than a very fast link.

## Addendum — Scenario K: when a watching peer is told an agent finished (2026-07-29)

Script: `scripts/e2e/scenario_k_agent_notify.py`. Added after the loop stopped, to cover a
user-reported bug the loop never reached: the agent-completion notification fired repeatedly
while an agent was still working.

The root cause was that "working" meant "this pane printed something in the last 2 seconds"
(`agent_detect.rs` `WORKING_WINDOW`), so every pause longer than that — waiting on a model
response, running a tool that streams nothing — was reported as a completion. A second defect
amplified it: the unread-pane set was both the overlay star and the sound's dedup key, so
focusing a pane to check on it silently re-armed the sound.

Two peers. A fake agent runs in the host's pane; the guest creates its own pane so it is *not*
viewing the agent, since a focused pane is suppressed by design and the scenario would
otherwise pass without testing anything.

Repro:
1. Host creates, guest joins, guest presses `Ctrl+P, n` so it is focused elsewhere.
2. Host runs a fake `claude` that prints, pauses 6s, prints, pauses 6s, prints, pauses 6s,
   then rings `BEL` and stays alive.
3. Sample the announcement count after each pause, then after the bell, then after settling.

| Check | Result |
|---|---|
| The agent's output reaches the watching guest | PASS |
| **A mid-task pause is not announced as a completion** | PASS |
| **The bell announces the completion** | PASS — 0.0s after the bell |
| **The completion is announced exactly once** | PASS |
| **Focusing the finished pane does not re-announce it** | PASS |
| The peer viewing the pane is not notified | PASS |
| The fake agent is classified as Claude Code | PASS |
| No panic on either peer | PASS |

2/2 runs clean, zero orphans.

**Two harness notes worth keeping** (both mine, not p2pmux's):

- **A fake agent cannot just be a script named `claude`.** Detection matches the process's own
  name, so a shebang script is reported as its interpreter and never classified. Copying
  `/bin/sh` to `claude` does not work either — macOS SIGKILLs a copy of a signed platform
  binary (`Killed: 9`). `/bin/sh -c 'exec -a claude /bin/sh <script>'` runs the real, signed
  binary under the right `argv[0]`.
- **The first version of the oracle sampled too late.** The bell followed the last step with no
  pause between them, so the "mid-task" sample already included the bell's announcement and
  reported a false failure. The agent now pauses before ringing.

**The oracle was verified to be able to fail**, rather than assumed. Running the *fixed* binary
with `quiet_seconds = 5` against 8s pauses — the shape of the original bug, pauses longer than
the window — produced announcements of 1, 2, 3 at the three sample points: one spurious ring
per pause, exactly as reported. At the default 20s window the same scenario reports 0, 0, 0.

Note for anyone re-running this against an older build: the count comes from an
`agent_completion` line in the UI debug log that was added by the fix, so a pre-fix binary
reads zero for every sample. That comparison measures the absence of the instrument, not the
behaviour, and cannot be used as an A/B.

## Addendum — Scenario N: a session this Mac is not a member of (2026-07-30)

Script: `scripts/e2e/scenario_n_two_remotes.py`. Lab: `scripts/e2e/provision_droplets.sh`.

Scenario L still had this Mac on one end of the wire, and only the droplet's pane ever streamed.
That leaves the product's actual claim untested: *two* people on *two* machines, each hosting
their own terminals, each able to take control of the other's. Scenario N removes the Mac from
the session entirely — it holds two ssh reins and nothing else — and runs the whole session
between a droplet in **nyc3** and a droplet in **fra1**.

**The lab is disposable, and the tag is the contract.** `provision_droplets.sh create` stands up
two `s-4vcpu-8gb` Ubuntu 24.04 droplets, imports a throwaway ed25519 key, and tags every resource
`p2pmux-itest`; `destroy` removes exactly what carries that tag, so the pre-existing `p2pmux-lab`
and `mybotvm` droplets and the developer's own ssh keys are never in scope. Source ships as a
tarball and is built on the nyc3 box, then the binary is copied to fra1 rather than built twice —
same image, same architecture, and the release build is most of the ~10 minutes. Coordinates land
in `~/.cache/p2pmux/itest/droplets.json`, which `remote.py` reads, so no scenario needs a
hand-edited `~/.ssh/config` entry for a host that will not exist in an hour.

| run | result |
|---|---|
| first run, pre-fix | **16/20** — the four cross-control checks failed |
| default path, post-fix | 27/27, both peers reading `direct` |
| `--force-relay` | 27/27, both peers reading `relayed` |
| `--delay-ms=200` | 27/27, still `direct` |

**One real product bug, and no localhost scenario could ever have found it.** Typing into a free
remote pane claims it, and the pane host bumps the lease epoch the moment it accepts that first
byte. Everything typed during the round trip that follows still carried the *old* epoch, so the
host rejected it as stale and the characters were gone — silently, with no redraw and no error.
On loopback that window is under a millisecond, which is why all ten localhost iterations passed.
Between nyc3 and fra1 it is 85ms and ate the next eight characters:
`echo N-CROSS-A2B-$(hostname)` reached the far shell as `eOSS-A2B-$(hostname)`, and `/bin/sh` ran
the wreckage. That is the shape of every failure in the pre-fix run above.

The fix treats claim-by-typing as what it is — a control request in flight — and holds the rest
of the burst in the existing `held_input` buffer until the new lease lands, exactly as an
explicit take-control request already did. Three copies of that rule (the shared layout, and the
guest loop's key and paste arms) had drifted apart; they now share one `remote_input_decision`
helper, which is also what made the case unit-testable.

`--delay-ms=200` exists for exactly this class of defect: it is the cheap way to check that a
race fixed at 85ms stays fixed at 300ms rather than merely moving.

## Addendum — Scenario O: three members, two continents, one Mac behind a router (2026-07-30)

Script: `scripts/e2e/scenario_o_three_peers.py`. Coordinator in nyc3, **this Mac** in the middle,
a third member in fra1. **20/20.**

Scenario N proved two droplets can share a session, but droplets are the easy case: public IPs,
no NAT, nothing in the way. The machine p2pmux actually ships to is a Mac behind a consumer
router, and the thing that machine's owner will want to believe is that somebody else can drive a
terminal *on it*. So this scenario joins from here and hands the Mac's pane to fra1.

| Check | Result |
|---|---|
| A NAT'd Mac and two droplets land in one room | PASS |
| **The coordinator reports both peers, not just the last one** | PASS — `direct 140ms ×2` |
| Each of the three machines hosts a pane of its own | PASS |
| All three peers render all three panes | PASS |
| **fra1 drives this Mac's terminal: the whole line arrives** | PASS |
| **…and the process runs on this Mac, as this user** | PASS — `O-ONTO-MAC-<mac hostname>` |
| …and the third member sees it happen | PASS |
| **This Mac drives fra1's terminal, and it runs in fra1** | PASS — `O-FROM-MAC-p2pmux-itest-b` |
| No peer died | PASS |

The badge is the interesting number: `direct 140ms ×2` means this Mac holepunched to **both**
droplets from behind its router, and no leg of a three-way session fell back to a relay. The
`×2` also retires a smaller doubt — the tab bar reports the worst of several peers rather than
whichever one connected last.

**Still not proven: carrier-grade NAT.** A home router doing endpoint-independent NAT is the
common case, not the hard one. A Mac on a phone hotspot remains the outstanding test, and it
needs a pair of hands.

## Open bugs

_None._

## Fixed

### BUG-3 (severity 5) — a refused joiner was not told the room is full

**Symptom.** The 9th member was correctly refused, but was told
`Error: transport error: Iroh stream read failed: read error: connection lost`. Truthful,
yet it never said the room was full and read like a network fault. Reproduced 3/3.

**Root cause, three layers — each one hid the next.**
1. The coordinator sent `Welcome` *first* (`session.rs:1774`) and only afterwards called
   `admit_with_display_name` where `MemberLimit` fires. The over-cap peer was told it was
   **admitted** and then dropped: the refusal was not merely unexplained, it was
   contradicted. *(Fixed in iteration 8 via `LayoutCoordinator::is_full()`.)*
2. Nothing was ever sent to explain the refusal, and closing the connection with a reason
   did not help — measured in iteration 8, the joiner's stream read fails before a
   connection-level close reason surfaces.
3. Once a refusal frame *was* sent, it still never arrived. Instrumenting the send showed
   p2pmux rejecting **its own outbound frame**:
   `Err(Transport(Protocol(InvalidLayout("layout_reject.request_id"))))` —
   `validate_nonzero` (`protocol.rs:824`) forbids a zero `request_id`, and a handshake
   refusal has no request to name.

**Fix.** The coordinator now sends an explicit refusal before dropping the connection,
carried by the **existing** `LayoutReject` body (tag 22) with the existing
`LayoutRejectReason::Limit` — so **no wire-format change**, only a new use of a message
that was already there:
- `HostSession::refuse_join` writes `LayoutReject` with a non-zero sentinel request id.
- `join_handshake_with_display_name` translates a received `LayoutReject` into
  `SessionError::SessionFull` (or `JoinRefused`) instead of `InvalidWelcome`.
- The join path waits for the refused peer to close before closing itself, so QUIC cannot
  discard the frame as unacknowledged stream data.
- That error rides the BUG-2 plumbing (node error file → CLI Display) to the user.

**Verified.** `Error: this session is full (8 members)`, 3/3 runs. Scenario G is fully
green for the first time. Regression: scenario A 2/2, full suite 385 tests 0 failures; the
cap still admits exactly 7 guests.

**Worth noting:** two iterations of "this needs a protocol change" turned out to be wrong —
the blocker was a self-inflicted validation failure on an existing message. Only
instrumenting the actual send, rather than reasoning about the design, found it.

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
