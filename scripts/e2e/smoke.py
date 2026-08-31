#!/usr/bin/env python3
"""Iteration 0 smoke test: prove the harness itself works against one real p2pmux.

This validates the machinery every later scenario depends on:
  1. a p2pmux process starts on a real PTY and renders bytes we can read back;
  2. keystrokes we send actually reach the child shell and change the screen;
  3. deadlines fire instead of hanging when something never happens;
  4. terminal resize is delivered;
  5. RSS sampling reports something sane;
  6. teardown leaves zero orphan p2pmux processes -- including the detached
     `__node` worker that `p2pmux create` forks behind the foreground client.

Run: python3 scripts/e2e/smoke.py
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import (  # noqa: E402
    DeadlineExceeded,
    Harness,
    p2pmux_pids,
    orphans_after,
    rss_kb,
    session_store_dirs_for,
)

RESULTS: list[tuple[str, bool, str]] = []


def check(name: str, passed: bool, detail: str = "") -> None:
    RESULTS.append((name, passed, detail))
    print(f"  {'PASS' if passed else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))


def smoke_local() -> None:
    """`p2pmux local`: one shell, typed command, real output read back."""
    print("\n[1] p2pmux local -- render, type, resize, RSS")
    with Harness("smoke-local") as harness:
        peer = harness.spawn("solo", ["local"], cols=100, rows=30)

        screen = peer.settle(quiet_for=0.5, timeout=8)
        check("renders something on the PTY", len(screen.strip()) > 0, f"{len(screen)} chars")

        # Type a command with a marker only the real child shell could produce.
        peer.run_in_shell("echo SMOKE-$((6*7))-OK")
        try:
            peer.wait_for(r"SMOKE-42-OK", timeout=10)
            check("keystrokes reach the child shell", True, "saw SMOKE-42-OK")
        except (DeadlineExceeded, AssertionError) as error:
            check("keystrokes reach the child shell", False, str(error)[:400])

        rss = rss_kb(peer.pid)
        check("RSS sampling works", rss > 1000, f"{rss} KiB incl. children")

        before = peer.snapshot()
        peer.resize(120, 40)
        time.sleep(0.8)
        after = peer.settle(quiet_for=0.4, timeout=5)
        check("survives a resize", peer.alive, f"changed={before != after}")

        # A deadline must fire rather than hang forever.
        start = time.monotonic()
        try:
            peer.wait_for(r"THIS-STRING-WILL-NEVER-APPEAR", timeout=1.5)
            check("deadlines fire", False, "no exception raised")
        except DeadlineExceeded:
            elapsed = time.monotonic() - start
            check("deadlines fire", 1.4 < elapsed < 4.0, f"{elapsed:.2f}s")

        peer.key("ctrl_q")
        time.sleep(0.5)
        # Ctrl+Q asks detach-or-kill; `d` is detach.
        peer.send(b"d")
        time.sleep(1.0)


def smoke_create_and_cleanup() -> None:
    """`p2pmux create`: publishes a ticket, and teardown reaps the detached node."""
    print("\n[2] p2pmux create -- ticket, sandbox isolation, orphan reaping")
    baseline = p2pmux_pids()

    with Harness("smoke-create") as harness:
        host = harness.spawn(
            "host",
            # The session name has to be pinned: `create` picks a random world city
            # otherwise, and the record lookup below has nothing to match on.
            ["create", "--name", "hostuser", "--session-name", "hostuser"],
            cols=100,
            rows=30,
        )
        screen = host.settle(quiet_for=0.6, timeout=15)

        try:
            ticket = harness.wait_for_ticket("hostuser", timeout=15)
            check("create publishes a ticket", ticket.startswith("p2pmux-v"), ticket[:24])
        except AssertionError:
            check("create publishes a ticket", False, f"screen:\n{screen[:600]}")

        # Sandboxed HOME really is where state landed.
        stores = session_store_dirs_for(harness.home)
        check(
            "state stays in the sandbox HOME",
            any(store.exists() for store in stores),
            f"stores={[str(store) for store in stores]}",
        )

        spawned = p2pmux_pids() - baseline
        check("background node was forked", len(spawned) >= 2, f"{len(spawned)} new pids")

    time.sleep(0.5)
    leaked = orphans_after(baseline)
    check("teardown leaves no orphans", not leaked, f"leaked={sorted(leaked)}")


def smoke_raises_still_cleans() -> None:
    """A scenario that blows up mid-flight must still leave nothing behind."""
    print("\n[3] exception inside the harness -- cleanup still runs")
    baseline = p2pmux_pids()
    try:
        with Harness("smoke-raise") as harness:
            peer = harness.spawn("boom", ["local"])
            peer.settle(quiet_for=0.4, timeout=8)
            raise RuntimeError("simulated scenario failure")
    except RuntimeError:
        pass
    time.sleep(0.5)
    leaked = orphans_after(baseline)
    check("cleanup runs on exception", not leaked, f"leaked={sorted(leaked)}")


def main() -> int:
    print("p2pmux e2e harness smoke test")
    smoke_local()
    smoke_create_and_cleanup()
    smoke_raises_still_cleans()

    failures = [name for name, ok, _ in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(failures)}/{len(RESULTS)} checks passed")
    if failures:
        print("failed: " + ", ".join(failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
