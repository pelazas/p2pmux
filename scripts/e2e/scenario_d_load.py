#!/usr/bin/env python3
"""Category F x D: type fast while a flood of output streams to a watching guest.

Scenario-bank entries combined:
  D. "cat a large file; emit 10k lines fast"
  F. "type fast while output streams; look for dropped or reordered keystrokes,
      half-drawn frames"

The host floods its pane with 10k lines and, *while that is still streaming*, types a
long distinctive command. The shell cannot read it until the flood finishes, so the
keystrokes sit in the PTY input queue across the whole burst -- if p2pmux drops,
truncates or reorders input under output pressure, the command comes back mangled.

The guest is watching the whole time, so the final rendered pane must match the host's
byte for byte: a flood is the most likely way to produce a half-drawn or duplicated frame.

Run: python3 scripts/e2e/scenario_d_load.py [repeats]
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, diff_screens, orphans_after, p2pmux_pids, rss_kb  # noqa: E402

LINES = 10000
TYPED = "TYPED-DURING-STREAM-Q1W2E3R4T5"


def pane_body(screen: str) -> str:
    return "\n".join(
        line[1:-1].rstrip()
        for line in screen.split("\n")
        if line.startswith("│") and line.endswith("│")
    ).rstrip("\n")


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    with Harness(f"d-load-{index}") as harness:
        host, ticket = harness.create_room("host")
        guest = harness.join_room("guest", ticket)
        time.sleep(1.0)

        rss_before = rss_kb(host.pid) + rss_kb(guest.pid)

        # Start the flood, then type into it while it is still going.
        host.type(f"seq 1 {LINES}", per_key_delay=0.005)
        host.send(b"\r")
        time.sleep(0.15)  # let the burst get going before typing into it

        host.type(f"echo {TYPED}", per_key_delay=0.001)
        host.send(b"\r")

        # The flood plus the queued command must both finish.
        try:
            host.wait_for(TYPED, timeout=45)
            arrived = True
        except AssertionError:
            arrived = False
        check("input typed during the flood still executes", arrived, pane_body(host.snapshot())[-400:])

        host.settle(quiet_for=1.0, timeout=25)
        guest.settle(quiet_for=1.0, timeout=25)

        body_host = pane_body(host.snapshot())
        body_guest = pane_body(guest.snapshot())

        check(
            "the queued command is not mangled by the flood",
            f"echo {TYPED}" in body_host,
            f"command line corrupted:\n{body_host[-500:]}",
        )
        check(
            "the queued command is not duplicated",
            body_host.count(TYPED) == 2,
            f"expected 2 occurrences (typed + echoed), saw {body_host.count(TYPED)}:\n{body_host[-500:]}",
        )
        check(
            f"the flood reached its last line ({LINES})",
            str(LINES) in body_host,
            f"tail of pane:\n{body_host[-400:]}",
        )
        check(
            "host and guest render identical pane bodies after the flood",
            body_host == body_guest,
            "\n" + diff_screens("host", body_host, "guest", body_guest),
        )

        # A flood is the classic way to leak. The first flood legitimately costs a lot
        # (~120 MB on the pane host: that is the scrollback buffer reaching capacity),
        # so an absolute ceiling would just be a magic number. The invariant that
        # actually matters is that it is BOUNDED -- a second identical flood must be
        # nearly free. Growing linearly per flood would mean unbounded scrollback.
        rss_first = rss_kb(host.pid) + rss_kb(guest.pid)
        host.type(f"seq 1 {LINES}")
        host.send(b"\r")
        host.wait_for(str(LINES), timeout=45)
        host.settle(quiet_for=1.0, timeout=25)
        guest.settle(quiet_for=1.0, timeout=25)
        time.sleep(1.0)
        rss_second = rss_kb(host.pid) + rss_kb(guest.pid)

        check(
            "a second identical flood does not grow memory again (scrollback is bounded)",
            rss_second - rss_first < 20_000,
            f"flood1={rss_first} flood2={rss_second} KiB (+{rss_second - rss_first})",
        )
        if verbose:
            print(
                f"      RSS baseline={rss_before} after-flood-1={rss_first} "
                f"after-flood-2={rss_second} KiB"
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
    print(f"Scenario D: typing during a 10k-line flood with a guest watching ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        runs.append(run_once(index, verbose=(repeats == 1)))

    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, name in enumerate(n for n, _, _ in runs[0]):
        failures = sum(1 for run in runs if not run[position][1])
        any_failure |= bool(failures)
        print(f"    {failures}/{len(runs)}  {'FAIL' if failures else 'pass'}  {name}")

    leaked = orphans_after(baseline)
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
