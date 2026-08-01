#!/usr/bin/env python3
"""Category F x C x B: a stalled viewer resyncing, then the coordinator killed outright.

Scenario-bank entries combined:
  F. "SIGSTOP a peer, let output accumulate, SIGCONT and check it resyncs rather than
      replaying garbage"
  F. "slow-viewer path: one peer stalled while others keep producing output"
  F. "kill a peer's process outright"
  C. "host quitting while guests are attached"
  B. "coordinator disconnects"

Three real peers this time, so a stalled viewer can be distinguished from a healthy one.

Note on what "killing the host" means here: `create`/`join` fork a detached `__node`
worker that owns the PTYs; the foreground peer is only a renderer. Killing the peer is a
detach, killing its node is the real disconnect. This scenario SIGKILLs the coordinator's
*node*, which is the genuine "host is gone" case.

Run: python3 scripts/e2e/scenario_e_failure.py [repeats]
"""

import signal
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import DeadlineExceeded, Harness, diff_screens, p2pmux_pids, orphans_after  # noqa: E402

LINES = 3000


def pane_body(screen: str) -> str:
    return "\n".join(
        line[1:-1].rstrip()
        for line in screen.split("\n")
        if line.startswith("│") and line.endswith("│")
    ).rstrip("\n")


def still_responsive(peer, timeout: float = 10.0) -> bool:
    """Liveness probe: a resize must produce a redraw at the new width.

    Stronger than "the process is alive" -- it proves the render loop is still turning
    rather than wedged on a dead peer, which is the severity-1 failure this looks for.
    """
    if not peer.alive:
        return False
    target = 80 if peer.cols != 80 else 96
    before = peer.snapshot()
    peer.resize(target, peer.rows)
    try:
        peer.wait_until(
            lambda screen: screen != before and any(
                len(line) > target - 12 for line in screen.split("\n")
            ),
            timeout=timeout,
            what=f"redraw at {target} columns",
        )
        return True
    except (DeadlineExceeded, AssertionError):
        return False


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    with Harness(f"e-fail-{index}") as harness:
        host, ticket = harness.create_room("host")
        g1 = harness.join_room("g1", ticket)
        g2 = harness.join_room("g2", ticket)
        time.sleep(1.5)

        marker = f"BASE{index}"
        host.run_in_shell(f"echo {marker}")
        try:
            for peer in (host, g1, g2):
                peer.wait_for(marker, timeout=12)
            check("all three peers share a baseline", True)
        except (DeadlineExceeded, AssertionError) as error:
            check("all three peers share a baseline", False, str(error)[:300])

        # --- 1. Stall g2, then flood. g1 must keep up while g2 is frozen.
        g2.settle(quiet_for=0.5, timeout=8)
        frozen_before = g2.snapshot()
        g2.signal(signal.SIGSTOP)
        time.sleep(0.5)

        host.type(f"seq 1 {LINES}")
        host.send(b"\r")
        try:
            host.wait_for(str(LINES), timeout=45)
            g1.wait_for(str(LINES), timeout=45)
            check("a healthy guest keeps up while another peer is stalled", True)
        except (DeadlineExceeded, AssertionError) as error:
            check("a healthy guest keeps up while another peer is stalled", False, str(error)[:300])

        check(
            "the stalled peer renders nothing while stopped",
            g2.snapshot() == frozen_before,
            "stalled peer's screen changed while SIGSTOPped",
        )

        # --- 2. Resume it. It must converge on the same content, not replay garbage.
        g2.signal(signal.SIGCONT)
        host.settle(quiet_for=1.0, timeout=25)
        g1.settle(quiet_for=1.0, timeout=25)

        body_host = pane_body(host.settle(quiet_for=1.0, timeout=25))
        try:
            g2.wait_until(
                lambda screen: pane_body(screen) == body_host,
                timeout=30,
                what="resync to the host's pane content",
            )
            resynced = True
        except (DeadlineExceeded, AssertionError):
            resynced = False

        body_g2 = pane_body(g2.snapshot())
        check(
            "the stalled peer resyncs to identical content after SIGCONT",
            resynced,
            "\n" + diff_screens("host", body_host, "g2", body_g2),
        )
        check(
            "the resumed peer did not replay a duplicated tail",
            body_g2.count(str(LINES)) == body_host.count(str(LINES)),
            f"host={body_host.count(str(LINES))} g2={body_g2.count(str(LINES))}",
        )

        # --- 3. Kill the coordinator's node outright, with both guests attached.
        coordinator_node = harness.node_pid_for_role("coordinator")
        check("found the coordinator node to kill", coordinator_node is not None,
              f"nodes={harness.node_pids()}")
        if coordinator_node is not None:
            import os
            os.kill(coordinator_node, signal.SIGKILL)
        time.sleep(4.0)

        # The guests are now looking at a dead session. They must be TOLD: the pane
        # header still advertises a live host, so with no notice a user cannot tell
        # the difference between "idle" and "the host vanished".
        # Deadline is 60s because detecting a dead peer is a transport-level timeout:
        # measured by hand, `pane N disconnected; retrying` lands at ~35s and
        # `layout coordinator disconnected` at ~48s. That latency is a separate concern
        # from whether the message reaches the user at all, which is what this asserts.
        told = []
        for peer in (g1, g2):
            try:
                peer.wait_for(r"(?i)disconnect", timeout=60)
                told.append(True)
            except (DeadlineExceeded, AssertionError):
                told.append(False)
        check(
            "guests are told the coordinator disconnected",
            all(told),
            f"g1={told[0]} g2={told[1]}; no disconnect notice within 60s. "
            f"g1 footer={g1.snapshot().split(chr(10))[-1][:90]!r}",
        )

        # Severity 1 is the whole point here: guests must not hang, wedge, or panic.
        for peer in (g1, g2):
            check(f"{peer.name} survives the coordinator being killed", peer.alive,
                  f"exit={peer.exit_code}")
            check(f"{peer.name} render loop is still responsive", still_responsive(peer),
                  "no redraw after resize -- peer is wedged")

        for peer in (host, g1, g2):
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:300])

        if verbose:
            print("      g1 tail after coordinator kill:")
            for line in g1.snapshot().split("\n")[:3]:
                print(f"        {line[:100]}")

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario E: stalled-viewer resync, then coordinator killed ({repeats}x)")
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
