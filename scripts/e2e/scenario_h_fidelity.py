#!/usr/bin/env python3
"""Category D x C: full-screen TUI, splits, mid-stream resize, paste, reattach.

Scenario-bank entries combined:
  D. "run a full-screen TUI (vim, htop, less) inside a shared pane"
  D. "splits, resize one pane, resize a peer's whole terminal mid-stream"
  D. "bracketed paste of a multi-line blob"
  C. "a guest reattaching after quitting"

A full-screen TUI is the hardest thing to replicate: `less` drives the alternate screen,
absolute cursor moves and full repaints, so any divergence between host and guest shows up
immediately. Resizing the *guest's* whole terminal while that TUI is live is the nasty
part -- the pane grid is host-owned, so the guest must crop its view without corrupting
the stream or disturbing the host.

Run: python3 scripts/e2e/scenario_h_fidelity.py [repeats]
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import DeadlineExceeded, Harness, diff_screens, p2pmux_pids  # noqa: E402

CTRL_P = b"\x10"
CTRL_Q = b"\x11"
PASTE_LINES = 6


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

    with Harness(f"h-fid-{index}") as harness:
        host, ticket = harness.create_room("host", cols=100, rows=28)
        guest = harness.join_room("guest", ticket, cols=100, rows=28)
        time.sleep(1.0)

        # --- D: a full-screen TUI in a shared pane.
        host.type("seq 1 500 | less")
        host.send(b"\r")
        time.sleep(3.0)
        host.settle(quiet_for=0.8, timeout=15)
        guest.settle(quiet_for=0.8, timeout=15)

        check(
            "host and guest render the full-screen TUI identically",
            pane_body(host.snapshot()) == pane_body(guest.snapshot()),
            "\n" + diff_screens("host", pane_body(host.snapshot()),
                                "guest", pane_body(guest.snapshot())),
        )

        host.send(b" ")  # page down inside less
        time.sleep(2.5)
        host.settle(quiet_for=0.8, timeout=12)
        guest.settle(quiet_for=0.8, timeout=12)
        paged_host = pane_body(host.snapshot())
        check(
            "paging inside the TUI stays in sync on the guest",
            paged_host == pane_body(guest.snapshot()) and "1\n2\n3" not in paged_host,
            "\n" + diff_screens("host", paged_host, "guest", pane_body(guest.snapshot())),
        )

        # --- D: resize the GUEST's whole terminal while the TUI is live.
        guest.resize(70, 20)
        time.sleep(2.5)
        guest.resize(100, 28)
        time.sleep(3.0)
        host.settle(quiet_for=0.8, timeout=15)
        guest.settle(quiet_for=1.0, timeout=15)

        check("guest survives a mid-stream terminal resize", guest.alive,
              f"exit={guest.exit_code}")
        check("host is undisturbed by the guest's resize", host.alive and
              pane_body(host.snapshot()) == paged_host,
              "\n" + diff_screens("before", paged_host, "after", pane_body(host.snapshot())))
        check(
            "guest reconverges on the host's view after resizing back",
            pane_body(guest.snapshot()) == pane_body(host.snapshot()),
            "\n" + diff_screens("host", pane_body(host.snapshot()),
                                "guest", pane_body(guest.snapshot())),
        )

        host.send(b"q")  # leave less
        time.sleep(2.0)

        # --- D: a split, then a bracketed paste of a multi-line blob.
        host.send(CTRL_P)
        time.sleep(0.35)
        host.send(b"n")
        time.sleep(2.5)
        host.settle(quiet_for=0.8, timeout=12)
        guest.settle(quiet_for=0.8, timeout=12)
        check(
            "a split is replicated to the guest",
            host.snapshot().count("Pane #") == 2 == guest.snapshot().count("Pane #"),
            f"host={host.snapshot().count('Pane #')} guest={guest.snapshot().count('Pane #')}",
        )

        blob = "".join(f"echo PASTE{index}-{n}\n" for n in range(1, PASTE_LINES + 1))
        host.send(b"\x1b[200~" + blob.encode() + b"\x1b[201~")
        time.sleep(4.0)
        host.settle(quiet_for=1.0, timeout=20)

        body = pane_body(host.snapshot())
        arrived = [n for n in range(1, PASTE_LINES + 1) if f"PASTE{index}-{n}" in body]
        check(
            "every line of a multi-line paste arrives",
            len(arrived) == PASTE_LINES,
            f"only {arrived} of {PASTE_LINES} lines arrived:\n{body[-400:]}",
        )
        positions = [body.rfind(f"PASTE{index}-{n}") for n in arrived]
        check(
            "pasted lines are not reordered",
            positions == sorted(positions),
            f"out of order: {list(zip(arrived, positions))}",
        )

        # --- C: the guest quits, then reattaches to the same session.
        member = next(
            (d for d in harness.session_descriptors()
             if str(d.get("role", "")).lower() == "member"),
            None,
        )
        check("found the guest's session record", member is not None)

        guest.send(CTRL_Q)
        time.sleep(3.0)
        check("guest client exits on Ctrl+Q", not guest.alive, f"exit={guest.exit_code}")

        if member is not None:
            again = harness.spawn("rejoin", ["attach", member["name"]], cols=100, rows=28)
            try:
                again.wait_ready(timeout=30)
                again.wait_for(r"Pane #\d+", timeout=30)
                reattached = True
            except (DeadlineExceeded, AssertionError):
                reattached = False
            check("guest can reattach after quitting", reattached,
                  f"screen:\n{again.snapshot()[:400]}")

            if reattached:
                again.settle(quiet_for=1.0, timeout=15)
                check(
                    "the reattached guest sees the pasted history, not a blank pane",
                    f"PASTE{index}-{PASTE_LINES}" in pane_body(again.snapshot()),
                    f"reattached view:\n{pane_body(again.snapshot())[-400:]}",
                )

        for peer in harness.peers:
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:250])

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario H: full-screen TUI, resize, paste, reattach ({repeats}x)")
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
