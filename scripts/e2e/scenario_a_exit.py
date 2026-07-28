#!/usr/bin/env python3
"""Category A x B: a pane's child exits while a guest is watching, then the guest
tries to type into the exited pane.

Combines two scenario-bank entries because that is where the bugs live:
  A. "a child process exits while a guest is watching it"
  A. "a guest sends input to an exited pane (must be rejected, not swallowed)"

Real host + real guest, each on its own PTY, asserting on the bytes each rendered.

Run: python3 scripts/e2e/scenario_a_exit.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import (  # noqa: E402
    DeadlineExceeded,
    Harness,
    diff_screens,
    p2pmux_pids,
    rss_kb,
)

HOST_EXIT_NOTICE = "exited — close with Ctrl+P, X"
GUEST_EXIT_NOTICE = "exited — input disabled; pane host can close with Ctrl+P, X"


def pane_body(screen: str) -> str:
    """Just the pane interior, without p2pmux chrome, for host/guest diffing."""
    rows = []
    for line in screen.split("\n"):
        if line.startswith("│") and line.endswith("│"):
            rows.append(line[1:-1].rstrip())
    return "\n".join(rows).rstrip("\n")


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    with Harness(f"a-exit-{index}") as harness:
        host, code = harness.create_room("host")
        guest = harness.join_room("guest", code)

        rss_start = rss_kb(host.pid) + rss_kb(guest.pid)

        # 1. Output produced by the host's child must reach the guest.
        marker = f"WATCHING-{index}"
        host.run_in_shell(f"echo {marker}")
        try:
            host.wait_for(marker, timeout=10)
            guest.wait_for(marker, timeout=10)
            check("guest sees host's pane output", True)
        except (DeadlineExceeded, AssertionError) as error:
            check("guest sees host's pane output", False, str(error)[:300])

        host.settle(quiet_for=0.5, timeout=6)
        guest.settle(quiet_for=0.5, timeout=6)
        body_host, body_guest = pane_body(host.snapshot()), pane_body(guest.snapshot())
        check(
            "host and guest render identical pane bodies",
            body_host == body_guest,
            "" if body_host == body_guest
            else "\n" + diff_screens("host", body_host, "guest", body_guest),
        )

        # 2. The child exits while the guest is watching.
        host.run_in_shell("exit")

        try:
            host.wait_for(re.escape(HOST_EXIT_NOTICE), timeout=12)
            check("host sees the exited notice", True)
        except (DeadlineExceeded, AssertionError) as error:
            check("host sees the exited notice", False, str(error)[:400])

        try:
            guest.wait_for(re.escape(GUEST_EXIT_NOTICE), timeout=12)
            check("guest sees the input-disabled exited notice", True)
        except (DeadlineExceeded, AssertionError) as error:
            check("guest sees the input-disabled exited notice", False, str(error)[:400])

        check("host still alive after child exit", host.alive, f"exit={host.exit_code}")
        check("guest still alive after child exit", guest.alive, f"exit={guest.exit_code}")

        # 3. The guest types into the exited pane. It must be rejected, not swallowed:
        #    nothing the guest typed may appear in the pane on either peer.
        guest.settle(quiet_for=0.5, timeout=6)
        before_guest = pane_body(guest.snapshot())
        before_host = pane_body(host.snapshot())

        forbidden = f"GHOSTKEY{index}"
        guest.type(forbidden)
        guest.send(b"\r")
        time.sleep(1.5)

        after_guest = pane_body(guest.snapshot())
        after_host = pane_body(host.snapshot())

        check(
            "guest keystrokes do not appear in the exited pane (guest view)",
            forbidden not in after_guest,
            "" if forbidden not in after_guest else f"leaked:\n{after_guest}",
        )
        check(
            "guest keystrokes do not reach the host's exited pane",
            forbidden not in after_host,
            "" if forbidden not in after_host else f"leaked:\n{after_host}",
        )
        check(
            "exited pane content is unchanged by rejected input (guest)",
            before_guest == after_guest,
            "" if before_guest == after_guest
            else "\n" + diff_screens("before", before_guest, "after", after_guest),
        )
        check(
            "exited pane content is unchanged by rejected input (host)",
            before_host == after_host,
            "" if before_host == after_host
            else "\n" + diff_screens("before", before_host, "after", after_host),
        )
        check(
            "guest still shows the exited notice after typing",
            GUEST_EXIT_NOTICE in guest.snapshot(),
            "",
        )

        # 4. Health: no panic text anywhere, no runaway memory.
        for peer in (host, guest):
            noise = [
                line
                for line in peer.raw_text().split("\n")
                if "panicked at" in line or "RUST_BACKTRACE" in line
            ]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:300])

        rss_end = rss_kb(host.pid) + rss_kb(guest.pid)
        growth = rss_end - rss_start
        check(
            "no runaway RSS growth",
            growth < 60_000,
            f"{rss_start} -> {rss_end} KiB (+{growth})",
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    all_runs: list[list[tuple[str, bool, str]]] = []

    print(f"Scenario A: child exits while guest watches + guest input rejected ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        all_runs.append(run_once(index, verbose=(repeats == 1)))

    names = [name for name, _, _ in all_runs[0]]
    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, name in enumerate(names):
        failures = sum(1 for run in all_runs if not run[position][1])
        if failures:
            any_failure = True
            print(f"    {failures}/{len(all_runs)}  FAIL  {name}")
        else:
            print(f"    0/{len(all_runs)}  pass  {name}")

    leaked = p2pmux_pids() - baseline
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
