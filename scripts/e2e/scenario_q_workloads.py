#!/usr/bin/env python3
"""Category D: real full-screen programs in a shared pane, driven from the other peer.

docs/e2e-stress-log.md lists this as never touched -- terminal fidelity was proven with
`less` and nothing else. vim and top are what a pair of developers actually put in a
shared pane on day one, and they stress different things than a pager does: vim owns the
alternate screen *and* takes input that changes the buffer, and top repaints on its own
timer whether anyone is typing or not.

The interesting claim is not "vim renders". It is that a guest can drive an editor
running on someone else's Mac and both peers keep the same screen while it happens.

The control lease is load-bearing here. Whoever typed last holds the pane for 30s
(`src/lease.rs` IDLE_AFTER), so the peer that starts a program and the peer that drives
it cannot act back to back: each hand-off waits for the header to read `control: free`,
exactly as a second human would have to. A version of this scenario without those waits
reports four failures that are all the lease working as designed.

Run: python3 scripts/e2e/scenario_q_workloads.py [repeats]
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, mask_link_badge, orphans_after, p2pmux_pids  # noqa: E402

TEST_FILE = "/tmp/p2pmux-scenario-q.txt"
TYPED = "HELLO-FROM-GUEST-42"


def body(screen: str) -> str:
    """Pane content without the tab bar and footer, so two peers can be compared."""
    return "\n".join(mask_link_badge(screen).splitlines()[2:-2])


def wait_free(peer, timeout: float = 45.0) -> bool:
    """Block until the focused pane's header says the control lease is free."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if "control: free" in peer.snapshot():
            return True
        time.sleep(0.5)
    return False


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    with Harness(f"q-workloads-{index}") as harness:
        host, ticket = harness.create_room("host")
        guest = harness.join_room("guest", ticket)

        # ------------------------------------------------------------------ vim
        # `-u NONE -N` so the run does not depend on whatever vimrc the machine has.
        host.run_in_shell(f"vim -u NONE -N {TEST_FILE}")
        time.sleep(3.0)
        check(
            "vim opened in the host's pane",
            "p2pmux-scenario-q" in host.snapshot() or "0L, 0B" in host.snapshot(),
            host.snapshot()[:300],
        )
        try:
            guest.wait_for("p2pmux-scenario-q", timeout=15)
            check("vim's alternate screen reached the guest", True)
        except Exception as error:
            check("vim's alternate screen reached the guest", False, str(error)[:200])

        check("the host's pane frees up after it stops typing", wait_free(guest))

        guest.send(b"i")
        time.sleep(1.5)
        guest.send(TYPED.encode())
        try:
            host.wait_for(TYPED, timeout=20)
            check("guest's insert-mode text reached vim on the host's Mac", True)
        except Exception as error:
            check("guest's insert-mode text reached vim on the host's Mac", False, str(error)[:200])
        try:
            guest.wait_for(TYPED, timeout=20)
            check("the guest sees what it typed", True)
        except Exception as error:
            check("the guest sees what it typed", False, str(error)[:200])

        host_body = body(host.settle(quiet_for=1.2, timeout=20))
        guest_body = body(guest.settle(quiet_for=1.2, timeout=20))
        check(
            "vim renders identically on both peers",
            host_body == guest_body,
            next(
                (f"{a!r} != {b!r}" for a, b in zip(host_body.splitlines(), guest_body.splitlines()) if a != b),
                "row count differs",
            ),
        )

        # `:q!` from the guest, so vim's command line is exercised remotely too.
        guest.send(b"\x1b")
        time.sleep(1.0)
        guest.send(b":q!\r")
        time.sleep(3.0)
        check(
            "vim exited and the alternate screen restored on both",
            TYPED not in host.snapshot() and TYPED not in guest.snapshot(),
            f"host tail: {host.snapshot()[-200:]!r}",
        )

        # ------------------------------------------------------------------ top
        check("the guest's pane frees up before the host reclaims it", wait_free(host))
        host.run_in_shell("top -l 0 -n 5")
        time.sleep(5.0)
        check(
            "top is drawing in the host's pane",
            "PID" in host.snapshot() or "Processes" in host.snapshot(),
            host.snapshot()[:300],
        )
        try:
            guest.wait_for(r"(Processes|PID)", timeout=20)
            check("top's live redraw streams to the guest", True)
        except Exception as error:
            check("top's live redraw streams to the guest", False, str(error)[:200])

        # A program repainting on its own timer must not drift the two views apart.
        # Row *lengths* rather than contents: top's clock and CPU numbers legitimately
        # differ between two snapshots taken a moment apart, and asserting equality
        # there would fail on a correct implementation.
        time.sleep(4.0)
        a = body(host.settle(quiet_for=1.5, timeout=25))
        b = body(guest.settle(quiet_for=1.5, timeout=25))
        check(
            "both peers hold the same grid while top repaints",
            [len(line) for line in a.splitlines()] == [len(line) for line in b.splitlines()],
            f"host {len(a.splitlines())} rows, guest {len(b.splitlines())} rows",
        )

        host.send(b"q")
        time.sleep(2.0)
        check("host peer still alive", host.alive)
        check("guest peer still alive", guest.alive)
        check(
            "no panic on either peer",
            "panicked" not in host.raw_text() and "panicked" not in guest.raw_text(),
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    print(f"Scenario Q: vim and top in a shared pane, driven by the other peer ({repeats}x)")
    baseline = p2pmux_pids()
    runs = []
    for index in range(repeats):
        print(f"\n  run {index + 1}/{repeats}")
        runs.append(run_once(index, verbose=(repeats == 1)))

    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, (name, _, _) in enumerate(runs[0]):
        failures = sum(1 for run in runs if position < len(run) and not run[position][1])
        any_failure |= bool(failures)
        print(f"    {failures}/{len(runs)}  {'FAIL' if failures else 'pass'}  {name}")

    leaked = orphans_after(baseline)
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
