#!/usr/bin/env python3
"""Category A x B: a pane exits, then is deleted, while ANOTHER peer has it focused.

Scenario-bank entries combined:
  A. "exit while the pane is the selected/focused one on another peer"
  A. "the host deletes an exited pane" (with a third peer watching that exact pane)
  B. three-peer semantics: the deleter, the focuser, and an uninvolved bystander

The nasty part is the deletion. A pane vanishing out from under a peer that currently has
it *selected* is the classic way to strand focus on an id that no longer exists: the peer
looks fine but every keystroke goes nowhere. So the decisive check is not "did it render"
but "can that peer still type somewhere afterwards" -- a stuck pane is severity 4, and a
panic on the dangling id would be severity 1.

Roles: `host` owns pane 1, `g1` owns pane 2 and is the one that exits and deletes it,
`g2` is the peer holding pane 2 focused throughout.

Run: python3 scripts/e2e/scenario_i_focused_exit.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import DeadlineExceeded, Harness, p2pmux_pids  # noqa: E402

CTRL_P = b"\x10"
IDLE_AFTER = 30.0  # lease.rs IDLE_AFTER
PANE_HEADER = re.compile(r"Pane #(\d+) host: (\S+)")

GUEST_EXIT_NOTICE = "exited — input disabled; pane host can close with Ctrl+P, X"


def owners(screen: str) -> list[str]:
    return [host for _, host in PANE_HEADER.findall(screen)]


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    with Harness(f"i-focus-{index}") as harness:
        host, ticket = harness.create_room("host", cols=110, rows=30)
        g1 = harness.join_room("g1", ticket, cols=110, rows=30)
        g2 = harness.join_room("g2", ticket, cols=110, rows=30)
        time.sleep(1.5)

        # g1 creates its own pane, so pane 2 is g1-hosted.
        chord(g1, CTRL_P, b"n", settle=3.0)
        for peer in (host, g1, g2):
            peer.settle(quiet_for=0.8, timeout=12)
        check(
            "all three peers see both panes with the right owners",
            owners(host.snapshot()) == ["host", "g1"] == owners(g2.snapshot()),
            f"host={owners(host.snapshot())} g1={owners(g1.snapshot())} g2={owners(g2.snapshot())}",
        )

        # g2 focuses g1's pane (the right-hand one) and proves focus landed there by
        # typing into it -- control is free, so this is a legitimate idle hop-in.
        chord(g2, CTRL_P, b"\x1b[C", settle=1.2)
        focus_marker = f"FOCUS{index}"
        g2.type(focus_marker)
        g2.send(b"\r")
        time.sleep(2.0)

        def right_column(screen: str) -> str:
            return "\n".join(
                line.split("││", 1)[1] for line in screen.split("\n") if "││" in line
            )

        check(
            "g2 has g1's pane focused (its typing landed there)",
            focus_marker in right_column(g1.snapshot()),
            f"g1's own view of pane 2:\n{right_column(g1.snapshot())[:300]}",
        )

        # g2's hop-in above grabbed pane 2's control lease. Until that lease goes idle,
        # even g1 -- the pane's own host -- cannot type into it, so an `exit` sent now
        # would be silently discarded and the pane would never die. Wait the lease out
        # (lease.rs IDLE_AFTER = 30s) so the exit actually happens. g2 keeps the pane
        # FOCUSED throughout: focus is per-peer UI state and is not the control lease.
        time.sleep(IDLE_AFTER + 4.0)

        # --- A: the pane's child exits while g2 has it selected.
        g1.type("exit")
        g1.send(b"\r")
        time.sleep(3.0)
        for peer in (host, g1, g2):
            peer.settle(quiet_for=0.8, timeout=12)

        try:
            g2.wait_for(re.escape(GUEST_EXIT_NOTICE), timeout=15)
            notice_ok = True
        except (DeadlineExceeded, AssertionError):
            notice_ok = False
        check(
            "the peer holding the pane focused sees the exited notice",
            notice_ok,
            f"g2 footer: {g2.snapshot().split(chr(10))[-1][:110]!r}",
        )
        check("no peer died on the exit", all(p.alive for p in (host, g1, g2)),
              f"host={host.alive} g1={g1.alive} g2={g2.alive}")

        # g2's input to the exited foreign pane must still be rejected.
        ghost = f"GHOST{index}"
        before_right = right_column(g2.snapshot())
        g2.type(ghost)
        g2.send(b"\r")
        time.sleep(1.5)
        check(
            "input from the focusing peer into the exited pane is rejected",
            ghost not in right_column(g2.snapshot()) and ghost not in right_column(g1.snapshot()),
            f"leaked into the exited pane:\n{right_column(g2.snapshot())[:300]}",
        )

        # --- The pane host deletes the exited pane while g2 still has it focused.
        chord(g1, CTRL_P, b"x", settle=3.5)
        for peer in (host, g1, g2):
            peer.settle(quiet_for=1.0, timeout=15)

        check(
            "the pane is gone on every peer",
            owners(host.snapshot()) == ["host"] == owners(g2.snapshot()) == owners(g1.snapshot()),
            f"host={owners(host.snapshot())} g1={owners(g1.snapshot())} g2={owners(g2.snapshot())}",
        )
        check("no peer died when the focused pane was deleted",
              all(p.alive for p in (host, g1, g2)),
              f"host={host.alive} g1={g1.alive} g2={g2.alive}")

        # THE decisive check: g2's focus was on the pane that just vanished. If focus is
        # stranded on a dead id, g2 renders fine but its keystrokes go nowhere.
        recover = f"RECOVER{index}"
        g2.type(recover)
        g2.send(b"\r")
        time.sleep(3.0)
        g2.settle(quiet_for=1.0, timeout=12)
        check(
            "g2 can still type after its focused pane was deleted (focus not stranded)",
            recover in g2.snapshot(),
            f"keystrokes vanished; g2 screen:\n{g2.snapshot()[:500]}",
        )
        check(
            "g2's recovered typing is visible to the other peers too",
            recover in host.snapshot(),
            f"host view:\n{host.snapshot()[:400]}",
        )

        for peer in (host, g1, g2):
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:250])

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario I: focused pane exits and is deleted under another peer ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        runs.append(run_once(index, verbose=(repeats == 1)))

    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, name in enumerate(n for n, _, _ in runs[0]):
        failures = sum(1 for run in runs if position < len(run) and not run[position][1])
        any_failure |= bool(failures)
        print(f"    {failures}/{len(runs)}  {'FAIL' if failures else 'pass'}  {name}")

    leaked = p2pmux_pids() - baseline
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
