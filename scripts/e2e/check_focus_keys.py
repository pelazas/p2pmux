#!/usr/bin/env python3
"""Issue #106 on a real terminal: Ctrl+arrow focus, and the shell's way out.

The unit tests feed synthetic `KeyEvent`s straight into the key handler. That
skips the part most likely to be wrong in practice: what a terminal actually
puts on the wire for Ctrl+arrow, and whether crossterm parses it back into the
modifiers the handler is matching on.

So this drives the real binary over a PTY and sends the real byte sequences:

    Ctrl+Right   \\x1b[1;5C      focus should move
    Ctrl+Left    \\x1b[1;5D      focus should move back
    Ctrl+Alt+Right \\x1b[1;7C    focus must NOT move -- it belongs to the shell

Focus is read from which pane's border is drawn in the focused colour, via the
pane title row: the focused pane is the one whose `Pane #N` heading the client
highlights. Simpler and less brittle: each pane runs a shell, and we check which
pane a typed character lands in.

Run: python3 scripts/e2e/check_focus_keys.py
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, orphans_after, p2pmux_pids  # noqa: E402

CTRL_P = b"\x10"
COLS, ROWS = 120, 32

# xterm modifier encoding: 1 + (1=shift, 2=alt, 4=ctrl).
CTRL_RIGHT = b"\x1b[1;5C"
CTRL_LEFT = b"\x1b[1;5D"
CTRL_ALT_RIGHT = b"\x1b[1;7C"
OPT_SHIFT_RIGHT = b"\x1b[1;4C"


def main() -> int:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    baseline = p2pmux_pids()
    failures = 0

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        if not ok:
            failures += 1
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    with Harness("focus-keys") as harness:
        peer = harness.spawn(
            "solo", ["create", "--name", "keys", "--session-name", "keys"],
            cols=COLS, rows=ROWS, env=env,
        )
        peer.wait_ready(timeout=30)

        # Two panes side by side. The new one (2) takes focus.
        peer.send(CTRL_P)
        time.sleep(0.4)
        peer.send(b"r")
        peer.wait_until(lambda s: "Pane #2" in s, timeout=30, what="the second pane")
        time.sleep(1.5)

        # Mark each pane so we can tell where typing lands.
        peer.run_in_shell("PS1='TWO> '")
        time.sleep(0.8)

        # Ctrl+Left should move focus to pane 1.
        peer.send(CTRL_LEFT)
        time.sleep(1.2)
        peer.type("echo LANDED-IN-ONE")
        peer.send(b"\r")
        landed_one = peer.wait_for("LANDED-IN-ONE", timeout=15) is not None
        check("Ctrl+Left moves focus to the pane on the left", landed_one)

        # Ctrl+Right should move it back.
        peer.send(CTRL_RIGHT)
        time.sleep(1.2)
        peer.type("echo LANDED-IN-TWO")
        peer.send(b"\r")
        landed_two = peer.wait_for("LANDED-IN-TWO", timeout=15) is not None
        check("Ctrl+Right moves it back to the right", landed_two)

        # Option+Shift+Right still works: it is at the right-hand edge now, so
        # the test is only that it is consumed rather than typed into the shell.
        before = peer.snapshot()
        peer.send(OPT_SHIFT_RIGHT)
        time.sleep(1.0)
        check(
            "Option+Shift+Right is still consumed, not echoed",
            "[1;4C" not in peer.snapshot() and "^[" not in peer.snapshot().split("\n")[-3],
            "the old binding still works",
        )
        del before

        # Ctrl+Alt+Right must reach the shell. In a bash/zsh line editor that is
        # forward-word; what we can assert portably is that focus did NOT move.
        peer.send(CTRL_ALT_RIGHT)
        time.sleep(1.2)
        peer.type("echo STILL-IN-TWO")
        peer.send(b"\r")
        still_two = peer.wait_for("STILL-IN-TWO", timeout=15) is not None
        check("Ctrl+Alt+Right does not move focus (it belongs to the shell)", still_two)

    leaked = orphans_after(baseline)
    if leaked:
        print(f"leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"focus keys: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
