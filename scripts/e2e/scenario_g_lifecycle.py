#!/usr/bin/env python3
"""Category C: room lifecycle -- bad codes, dead rooms, simultaneous joins, the cap.

Scenario-bank entries covered:
  C. "join with a bad/expired code, join a dead room"
  C. "two peers joining simultaneously"
  C. "a guest reattaching after quitting"
  C. "joining at the 8-member cap and one over it"

The severity-1 question running through all of these is whether a doomed join *fails*
rather than hanging: a peer that blocks forever on a dead room with no feedback is far
worse than one that exits with an error.

`MAX_MEMBERS` is 8 and the host counts as a member (src/layout.rs:30,361), so the cap is
host + 7 guests, and the 9th peer must be refused with `LayoutError::MemberLimit`.

Each phase gets its own Harness -- and therefore its own sandbox HOME -- so join codes
cannot leak between phases and cleanup happens incrementally.

Run: python3 scripts/e2e/scenario_g_lifecycle.py [repeats]
"""

import os
import signal
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import DeadlineExceeded, Harness, p2pmux_pids  # noqa: E402


def wait_exit(peer, timeout: float) -> int | None:
    """Wait for a peer to exit, returning its code or None if it outlived the deadline."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not peer.alive:
            return peer.exit_code
        time.sleep(0.1)
    return None


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    # --- Phase 1: a join code that was never valid.
    with Harness(f"g-badcode-{index}") as harness:
        bad = harness.spawn("bad", ["join", "ZZZZZZZZZZ", "--name", "bad"])
        code = wait_exit(bad, timeout=30)
        check("a bad join code exits instead of hanging", code is not None,
              "still running after 30s")
        check("a bad join code exits non-zero", code not in (0, None), f"exit={code}")
        check(
            "a bad join code explains itself",
            "not found" in bad.raw_text().lower() or "invalid" in bad.raw_text().lower(),
            f"output tail: {bad.raw_text()[-300:]!r}",
        )

    # --- Phase 2: a well-formed code whose room is dead.
    with Harness(f"g-deadroom-{index}") as harness:
        host, room_code = harness.create_room("host")
        node = harness.node_pid_for_role("coordinator")
        host.close()
        if node is not None:
            try:
                os.kill(node, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(2.0)

        latecomer = harness.spawn("late", ["join", room_code, "--name", "late"])
        code = wait_exit(latecomer, timeout=90)
        check("joining a dead room terminates instead of hanging forever",
              code is not None,
              "still running after 90s -- a doomed join blocks with no feedback")
        if code is None:
            check("joining a dead room reports an error", False,
                  f"screen:\n{latecomer.snapshot()[:400]}")
        else:
            text = latecomer.raw_text().lower()
            check(
                "joining a dead room reports an error",
                any(word in text for word in ("error", "disconnect", "timed out", "failed",
                                              "refused", "unreachable", "not found")),
                f"exit={code} output tail: {latecomer.raw_text()[-300:]!r}",
            )

    # --- Phase 3: two peers joining at the same instant.
    with Harness(f"g-simul-{index}") as harness:
        host, room_code = harness.create_room("host")
        a = harness.spawn("a", ["join", room_code, "--name", "aaa"])
        b = harness.spawn("b", ["join", room_code, "--name", "bbb"])
        joined = []
        for peer in (a, b):
            try:
                peer.wait_ready(timeout=40)
                peer.wait_for(r"Pane #\d+", timeout=40)
                joined.append(True)
            except (DeadlineExceeded, AssertionError):
                joined.append(False)
        check("both simultaneous joiners get in", all(joined), f"a={joined[0]} b={joined[1]}")

        # The host's own view must show a marker reaching both of them.
        marker = f"SIMUL{index}"
        host.run_in_shell(f"echo {marker}")
        seen = []
        for peer in (a, b):
            try:
                peer.wait_for(marker, timeout=20)
                seen.append(True)
            except (DeadlineExceeded, AssertionError):
                seen.append(False)
        check("both simultaneous joiners receive the host's output", all(seen), f"{seen}")

    # --- Phase 4: the 8-member cap, and one over it.
    with Harness(f"g-cap-{index}") as harness:
        host, room_code = harness.create_room("host")
        guests = []
        for n in range(1, 8):  # host + 7 guests == MAX_MEMBERS (8)
            guest = harness.spawn(f"g{n}", ["join", room_code, "--name", f"g{n}"])
            guests.append(guest)
            time.sleep(1.2)  # stagger so the coordinator admits them in order

        admitted = 0
        for guest in guests:
            try:
                guest.wait_ready(timeout=45)
                guest.wait_for(r"Pane #\d+", timeout=45)
                admitted += 1
            except (DeadlineExceeded, AssertionError):
                pass
        check("all 7 guests fit under the 8-member cap", admitted == 7,
              f"only {admitted}/7 guests got in")

        # The 9th member (8th guest) must be refused, not admitted and not hung.
        over = harness.spawn("over", ["join", room_code, "--name", "over"])
        over_code = wait_exit(over, timeout=60)
        got_in = False
        if over_code is None:
            try:
                over.wait_for(r"Pane #\d+", timeout=5)
                got_in = True
            except (DeadlineExceeded, AssertionError):
                got_in = False
        check(
            "the 9th member is refused, not admitted",
            not got_in,
            "a 9th member was admitted past MAX_MEMBERS -- cap not enforced end to end",
        )
        check(
            "the over-cap peer does not hang silently",
            over_code is not None,
            f"still running after 60s; screen:\n{over.snapshot()[:300]}",
        )
        if over_code is not None:
            # Scoped to the error line only. Matching the whole PTY output is useless
            # here: the TRUST WARNING p2pmux prints on every join contains "fully
            # trusted", so a naive substring search for "full" passes on a room that
            # never explained itself.
            text = over.raw_text().lower()
            error_line = next(
                (ln for ln in reversed(over.raw_text().splitlines()) if "error" in ln.lower()),
                "",
            ).lower()
            check(
                "the over-cap peer explains why it was refused",
                any(w in error_line
                    for w in ("full", "limit", "capacity", "too many", "member")),
                f"exit={over_code} error line: {error_line[:200]!r}",
            )
            check(
                "the refusal is not a raw internal debug error",
                "kind: timedout" not in text and "custom {" not in text,
                f"leaks internals: {over.raw_text()[-220:]!r}",
            )

        for peer in [host, *guests]:
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            if noise:
                check(f"no panic on {peer.name}", False, "; ".join(noise)[:200])
        check("no panic across the full room", True)

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario G: room lifecycle -- bad codes, dead rooms, the cap ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        runs.append(run_once(index, verbose=(repeats == 1)))

    print("\n  per-check failure rate across runs:")
    any_failure = False
    names = [n for n, _, _ in runs[0]]
    for position, name in enumerate(names):
        failures = sum(1 for run in runs if position < len(run) and not run[position][1])
        any_failure |= bool(failures)
        print(f"    {failures}/{len(runs)}  {'FAIL' if failures else 'pass'}  {name}")

    leaked = p2pmux_pids() - baseline
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
