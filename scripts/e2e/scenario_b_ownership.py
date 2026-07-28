#!/usr/bin/env python3
"""Category B x A: a guest tries to destroy things it does not own.

Scenario-bank entries combined (all severity-2 "wrong ownership outcome" territory):
  B. "guest attempts to delete a pane they do not host -> must be rejected"
  A. "a guest tries to delete someone else's exited pane"
  B. "guest attempts to delete a tab containing any foreign pane -> must be rejected"
  B. "idle hop-in: another member types into an idle pane with no approval"

Includes a POSITIVE CONTROL at the end: the pane host deletes its own exited pane and
that must succeed. Without it, every "rejected" result above could be a false pass from
a chord that silently never fired.

Run: python3 scripts/e2e/scenario_b_ownership.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, p2pmux_pids  # noqa: E402

PANE_HEADER = re.compile(r"Pane #(\d+) host: (\S+)")

CTRL_P = b"\x10"
CTRL_T = b"\x14"


def owners(screen: str) -> list[str]:
    """Owner of each rendered pane, in display order.

    Deliberately keyed on owner and count, never on the `Pane #N` number: that number
    is a display *ordinal* (src/tui.rs:571), so deleting the first pane renumbers the
    survivor to #1. An id-keyed oracle reports a phantom failure when that happens.
    """
    return [host for _, host in PANE_HEADER.findall(screen)]


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    """Send a p2pmux chord (Ctrl+P / Ctrl+T then a letter) like a user would."""
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            suffix = f"  -- {detail}" if detail and not ok else ""
            print(f"    {'PASS' if ok else 'FAIL'}  {name}{suffix}")

    def both(peer_a, peer_b) -> str:
        return f"host={owners(peer_a.snapshot())} guest={owners(peer_b.snapshot())}"

    with Harness(f"b-own-{index}") as harness:
        host, code = harness.create_room("host")
        guest = harness.join_room("guest", code)
        time.sleep(1.0)

        # Guest creates a pane of its own, so the tab holds one pane per owner.
        chord(guest, CTRL_P, b"n", settle=2.5)
        guest.settle(quiet_for=0.6, timeout=10)
        host.settle(quiet_for=0.6, timeout=10)

        check(
            "both peers see two panes with the right owners",
            owners(host.snapshot()) == ["host", "guest"] == owners(guest.snapshot()),
            both(host, guest),
        )

        # Move guest focus onto the host's pane. Chords are consumed by the TUI and
        # are not pane input, so this does NOT grab the host pane's control lease --
        # keeping this scenario purely about ownership. Lease handoff is scenario C.
        chord(guest, CTRL_P, b"\x1b[D", settle=1.0)

        # --- 1. Guest deletes a live pane it does not host: must be rejected.
        chord(guest, CTRL_P, b"x", settle=2.5)
        check(
            "guest cannot delete a live foreign pane",
            owners(host.snapshot()) == ["host", "guest"] == owners(guest.snapshot()),
            both(host, guest),
        )

        # --- 2. Guest deletes the whole tab, which contains a foreign pane.
        #        Two panes are present, so p2pmux raises a confirmation first.
        chord(guest, CTRL_T, b"x", settle=1.0)
        guest.send(b"\r")  # confirm, if a dialog appeared
        time.sleep(2.5)
        check(
            "guest cannot delete a tab containing a foreign pane",
            owners(host.snapshot()) == ["host", "guest"] == owners(guest.snapshot()),
            both(host, guest),
        )
        check(
            "tab still exists after the rejected tab delete",
            "Tab #1" in guest.snapshot() and "Tab #1" in host.snapshot(),
        )

        # --- 3. The host's child exits, so the foreign pane is now exited too.
        host.run_in_shell("exit")
        try:
            host.wait_for("exited", timeout=15)
            exited_ok = True
        except AssertionError:
            exited_ok = False
        check("host's pane reports exited", exited_ok, host.snapshot()[-200:])

        time.sleep(1.0)
        chord(guest, CTRL_P, b"x", settle=2.5)
        check(
            "guest cannot delete an EXITED foreign pane",
            owners(host.snapshot()) == ["host", "guest"] == owners(guest.snapshot()),
            both(host, guest),
        )

        # --- 4. POSITIVE CONTROL: the pane host closes its own exited pane.
        #        This must succeed, otherwise the rejections above prove nothing.
        chord(host, CTRL_P, b"x", settle=3.0)
        host.settle(quiet_for=0.6, timeout=8)
        guest.settle(quiet_for=0.6, timeout=8)
        check(
            "positive control: pane host CAN delete its own exited pane",
            owners(host.snapshot()) == ["guest"] == owners(guest.snapshot()),
            both(host, guest),
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
    print(f"Scenario B: guest cannot destroy what it does not own ({repeats}x)")
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
