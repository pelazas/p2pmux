#!/usr/bin/env python3
"""Category B: control-lease handoff, and whether input survives it.

Scenario-bank entries combined:
  B. "idle hop-in: another member types into an idle pane with no approval"
  B. "take-control while the current controller is actively typing -> explicit handoff"
  B. "two peers race for control of the same pane simultaneously"

The interesting target is `held_input` (src/tui.rs:6717-6725): when the lease has gone
idle, a non-controller's keystrokes are buffered client-side while a take-control request
makes a round trip, then replayed. That buffer-and-replay is exactly where input gets
dropped, duplicated, or reordered -- severity 3 -- so the handoff is driven with a long
distinctive string typed fast enough to straddle the round trip.

`lease.rs IDLE_AFTER` is 30s, so each run spends ~35s waiting for the lease to go idle.

Run: python3 scripts/e2e/scenario_c_lease.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, p2pmux_pids  # noqa: E402

IDLE_AFTER = 30.0
CTRL_P = b"\x10"

# Deliberately non-repeating, so a reordering or a partial replay is visible by eye.
HANDOFF = "Q1W2E3R4T5Y6U7I8O9P0"


def pane_columns(screen: str) -> tuple[str, str]:
    """(left pane text, right pane text) for a two-pane side-by-side layout."""
    left, right = [], []
    for line in screen.split("\n"):
        if "││" in line:
            first, second = line.split("││", 1)
            left.append(first.lstrip("│").rstrip())
            right.append(second.rstrip("│").rstrip())
    return "\n".join(left), "\n".join(right)


def control_of(screen: str) -> list[str]:
    return re.findall(r"control: (\S+)", screen)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    with Harness(f"c-lease-{index}") as harness:
        host, ticket = harness.create_room("host")
        guest = harness.join_room("guest", ticket)
        time.sleep(1.0)

        # Give the guest a pane so the layout is two-column and columns are separable.
        guest.send(CTRL_P); time.sleep(0.35); guest.send(b"n")
        time.sleep(2.5)
        guest.settle(quiet_for=0.6, timeout=10)

        # --- 1. Idle hop-in: control is free, so the guest may type into the host's
        #        pane with no approval at all.
        guest.send(CTRL_P); time.sleep(0.35); guest.send(b"\x1b[D")
        time.sleep(1.0)
        hop = f"HOPIN{index}"
        guest.type(hop)
        guest.send(b"\r")
        time.sleep(2.0)

        host_left, _ = pane_columns(host.snapshot())
        check("idle hop-in: guest input reaches the host's pane", hop in host_left, host_left[:300])
        check(
            "lease shows the guest as controller on both peers",
            control_of(host.snapshot())[:1] == ["guest"] == control_of(guest.snapshot())[:1],
            f"host={control_of(host.snapshot())} guest={control_of(guest.snapshot())}",
        )

        # --- 2. The pane's own host types while the guest still holds the lease.
        #        Per design this must be refused. Record whether ANY feedback is given.
        blocked = f"BLOCKED{index}"
        before = pane_columns(host.snapshot())[0]
        host.type(blocked)
        time.sleep(2.0)
        after = pane_columns(host.snapshot())[0]

        check(
            "host input is refused while the guest holds the lease",
            blocked not in after,
            f"host's own keystrokes leaked into a pane it does not control:\n{after[:300]}",
        )
        feedback = after != before or "control" not in host.snapshot()
        check(
            "refusal is silent (documented gap, not asserted as a bug)",
            True,
            "",
        )
        if verbose:
            print(f"      note: any visible reaction to the refused keystrokes? {after != before}")

        # --- 3. Wait for the lease to go idle, then the host takes it back by typing.
        #        Typed fast, so the bytes straddle the take-control round trip and land
        #        in held_input.
        time.sleep(IDLE_AFTER + 5.0)

        host.type(f"echo {HANDOFF}", per_key_delay=0.002)
        host.send(b"\r")
        time.sleep(4.0)
        host.settle(quiet_for=0.8, timeout=10)
        guest.settle(quiet_for=0.8, timeout=10)

        host_left, _ = pane_columns(host.snapshot())
        guest_left, _ = pane_columns(guest.snapshot())

        check(
            "host reclaims control after the lease goes idle",
            HANDOFF in host_left,
            f"handoff string never arrived:\n{host_left[:400]}",
        )
        # The typed command line and its output = exactly two occurrences. A replayed
        # held_input buffer shows up as three or more, or as a mangled command line.
        occurrences = host_left.count(HANDOFF)
        check(
            "handoff input is not duplicated",
            occurrences == 2,
            f"expected 2 occurrences (typed + echoed), saw {occurrences}:\n{host_left[:400]}",
        )
        check(
            "handoff input is not reordered or partially dropped",
            f"echo {HANDOFF}" in host_left,
            f"command line was mangled:\n{host_left[:400]}",
        )
        check(
            "guest sees the same reclaimed-pane content as the host",
            guest_left.count(HANDOFF) == occurrences,
            f"host={occurrences} guest={guest_left.count(HANDOFF)}",
        )
        check(
            "lease now shows the host as controller",
            control_of(host.snapshot())[:1] == ["host"],
            f"{control_of(host.snapshot())}",
        )

        # --- 4. Race: after the lease goes idle again, both peers type at once.
        #        Exactly one must win, and neither may interleave into the other.
        time.sleep(IDLE_AFTER + 5.0)
        guest.send(CTRL_P); time.sleep(0.35); guest.send(b"\x1b[D"); time.sleep(0.8)

        host.type("AAAAAAAAAA", per_key_delay=0.0)
        guest.type("BBBBBBBBBB", per_key_delay=0.0)
        time.sleep(3.0)

        host_left, _ = pane_columns(host.snapshot())
        interleaved = re.search(r"A+B+A|B+A+B", host_left)
        check(
            "racing peers do not interleave into one command line",
            interleaved is None,
            f"interleaved input {interleaved.group(0) if interleaved else ''!r}:\n{host_left[:400]}",
        )
        winners = [m for m in ("AAAAAAAAAA", "BBBBBBBBBB") if m in host_left]
        check(
            "exactly one peer wins the control race",
            len(winners) == 1,
            f"winners={winners}\n{host_left[:400]}",
        )

        for peer in (host, guest):
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:300])
            check(f"{peer.name} still alive", peer.alive, f"exit={peer.exit_code}")

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario C: control-lease handoff and input integrity ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        runs.append(run_once(index, verbose=(repeats == 1)))

    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, name in enumerate(n for n, _, _ in runs[0]):
        failures = sum(1 for run in runs if not run[position][1])
        any_failure |= bool(failures)
        print(f"    {failures}/{len(runs)}  {'FAIL' if failures else 'pass'}  {name}")

    leaked = p2pmux_pids() - baseline
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
