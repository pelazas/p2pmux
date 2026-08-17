"""Scenario AH -- a pane keeps its text when the machine hosting it narrows and widens.

A pane's text lives in one place: the terminal state the machine hosting its PTY keeps
for it. Nothing else can rebuild it -- the process that printed a line has usually
exited by the time anyone resizes anything, so there is no repaint to ask for. Cutting
the visible rows to a narrower width therefore destroys text outright, and it destroys
it for every viewer at once, not only for the one who dragged the divider.

This drives that from the far end: the droplet hosts the pane, prints a line far wider
than the pane, then takes its own window down to a third of its width and back. Both
peers then have to still show the whole line -- the droplet because it did the reflow,
and the Mac because the reflowed frame has to survive the trip.

Run:  python3 scripts/e2e/scenario_ah_reflow_cross.py
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteError, RemoteHost, hosts_from_manifest  # noqa: E402

CTRL_P = b"\x10"

WIDE = 140
NARROW = 50
ROWS = 30

# 300 characters with no spaces in them: wider than either pane at either width, and
# a partial survivor reads as obviously truncated rather than as a plausible line.
MARKER = "ABCDEFGHIJ" * 30

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    mark = "PASS" if passed else "FAIL"
    print(f"  [{mark}] {label}{(' -- ' + detail) if detail else ''}", flush=True)


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def pane_strips(snapshot: str) -> list[str]:
    """Each pane's own column strip, joined down the frame.

    A pane border sits between the halves of every soft-wrapped line, so reading the
    snapshot row by row reports a wrapped line as truncated whether it is or not.
    Taking one pane's columns and joining them puts the line back together.

    Side-by-side panes put their borders back to back, so a row reads `│…││…│` and
    splitting on the border leaves a zero-width piece between the two. Dropping the
    empty pieces is what keeps a pane's index the same on every row.
    """
    strips: dict[int, list[str]] = {}
    for line in snapshot.split("\n"):
        for index, segment in enumerate(
            piece for piece in line.split("│")[1:-1] if piece
        ):
            strips.setdefault(index, []).append(segment)
    return ["".join(parts) for parts in strips.values()]


def shows_marker(snapshot: str) -> bool:
    """Whether some pane holds the marker whole. Which pane is which depends on the
    order the two peers created theirs, and this scenario is not asserting on that."""
    return any(MARKER in strip for strip in pane_strips(snapshot))


def main() -> int:
    # The throwaway lab if one is up, the standing box otherwise. Only one droplet is
    # needed here: the second peer is this Mac. `--host=` picks a different one, which
    # is how the same run is repeated against a droplet carrying an older binary.
    wanted = next(
        (arg.split("=", 1)[1] for arg in sys.argv[1:] if arg.startswith("--host=")),
        None,
    )
    try:
        hosts = hosts_from_manifest()
        remote = next((h for h in hosts if h.alias == wanted), None) or hosts[0]
    except (RemoteError, IndexError):
        remote = RemoteHost()
    print(f"== scenario AH: {remote.alias} ==", flush=True)
    if not remote.binary_ready():
        print(f"droplet has no release binary yet at {remote.binary}", file=sys.stderr)
        return 2

    remote.reset_network()
    remote.reset_home()

    try:
        with Harness("scenario_ah_reflow_cross") as harness:
            host, ticket = harness.create_room("mac", cols=WIDE, rows=ROWS)
            guest = harness.spawn(
                "droplet",
                ["join", ticket, "--name", "droplet"],
                cols=WIDE,
                rows=ROWS,
                launcher=remote.launcher(),
            )
            guest.wait_ready(timeout=45.0)
            guest.wait_for(r"Pane #\d+", timeout=60.0)
            check("droplet joined over the internet", True)

            # The droplet has to host the pane itself: a pane's grid is decided by the
            # machine whose PTY it is, so only that machine's resize can reflow it.
            chord(guest, CTRL_P, b"n")
            guest.settle(quiet_for=1.0, timeout=15.0)
            host.wait_for(r"host: droplet", timeout=45.0)
            check("droplet hosts a pane in the shared layout", True)

            guest.run_in_shell(f"printf '{MARKER}\\n'")
            guest.wait_until(shows_marker, timeout=30.0)
            check("the line printed on the droplet", True)
            host.wait_until(shows_marker, timeout=45.0)
            check("the line reached the mac whole", True)

            # The round trip. Before the reflow this took the line with it: the shrink
            # cut every row at 23 columns and widening again came back to blanks.
            guest.resize(NARROW, ROWS)
            guest.settle(quiet_for=1.0, timeout=20.0)
            narrow = guest.snapshot()
            guest.resize(WIDE, ROWS)
            guest.settle(quiet_for=1.0, timeout=20.0)

            kept = shows_marker(guest.snapshot())
            check("the droplet still has the whole line", kept)
            if not kept:
                print("\n--- droplet at its narrowest ---", flush=True)
                print(narrow, flush=True)
                print("\n--- droplet back at full width ---", flush=True)
                print(guest.snapshot(), flush=True)

            try:
                host.wait_until(shows_marker, timeout=45.0)
                check("the mac still has the whole line", True)
            except AssertionError:
                check("the mac still has the whole line", False)
                print("\n--- mac ---", flush=True)
                print(host.snapshot(), flush=True)

            check("droplet peer still alive", guest.alive)
            check("mac peer still alive", host.alive)
    finally:
        remote.reap()
        remote.reset_network()

    failures = [label for label, passed, _ in checks if not passed]
    print(f"\n{len(checks) - len(failures)}/{len(checks)} checks passed", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
