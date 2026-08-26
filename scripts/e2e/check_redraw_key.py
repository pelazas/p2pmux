#!/usr/bin/env python3
"""Issue #120 on a real terminal: does Ctrl+P R actually repaint the screen?

Ratatui writes only the cells that differ from the screen it believes is up. So
the property that matters here cannot be read off the rendered screen at all --
the screen looks the same before and after -- it has to be read off the wire:
after the chord, cells that did not change are written *again*.

That is the whole point of the key. When something has made ratatui's belief
wrong -- a terminal that measures a cluster differently, a stray sequence from a
program in a pane, a multiplexer outside it -- every cell it thinks is already
correct is never written again, and the stale glyphs stay. Until this key, the
only way out was resizing the window, which people found by accident.

So: put a marker in a pane, let the frame settle, record how much output has
been seen, press the chord, and require the marker to cross the wire again.

Run: python3 scripts/e2e/check_redraw_key.py
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, orphans_after, p2pmux_pids  # noqa: E402

CTRL_P = b"\x10"
COLS, ROWS = 100, 30

MARKER = "REPAINTME"


def main() -> int:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    baseline = p2pmux_pids()
    failures = 0

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        if not ok:
            failures += 1
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    with Harness("redraw-key") as harness:
        peer = harness.spawn(
            "solo", ["create", "--name", "redraw", "--session-name", "redraw"],
            cols=COLS, rows=ROWS, env=env,
        )
        peer.wait_ready(timeout=30)
        peer.run_in_shell(f"printf '{MARKER}\\n'")
        peer.wait_until(lambda s: MARKER in s, timeout=30, what="the marker")
        # Let the frame settle, so anything still arriving belongs to the marker
        # being drawn and not to the chord under test.
        peer.settle(quiet_for=0.6, timeout=10)

        before = len(peer.raw_text())
        peer.send(CTRL_P)
        time.sleep(0.4)
        peer.send(b"R")
        time.sleep(1.5)
        after = peer.raw_text()
        emitted = after[before:]

        check(
            "the chord makes the client write the unchanged screen again",
            MARKER in emitted,
            f"{len(emitted)} bytes written, marker absent" if MARKER not in emitted else "",
        )
        check(
            "and the marker is still on screen afterwards",
            MARKER in peer.snapshot(),
            peer.snapshot()[:200],
        )
        # A chord that is consumed never reaches the pane's shell. If it did,
        # the pane would be holding a stray `R` at its prompt.
        prompt_line = [line for line in peer.snapshot().split("\n") if "$" in line]
        check(
            "and the R never reaches the shell in the pane",
            all(not line.rstrip().endswith("R") for line in prompt_line),
            " / ".join(line.strip() for line in prompt_line)[:160],
        )

    leaked = orphans_after(baseline)
    if leaked:
        print(f"leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"redraw key: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
