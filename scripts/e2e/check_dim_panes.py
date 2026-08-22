#!/usr/bin/env python3
"""Issue #111 by eye, on a real terminal: are unfocused panes actually dimmer?

The unit test asserts `Modifier::DIM` lands on the right cells of a ratatui test
buffer. That is not the same claim as "a real p2pmux, on a real PTY, emits SGR 2
for the pane you are not in" -- between those two sits the whole client, the
node, and ratatui's own diffing.

So this drives the real binary, splits a pane, writes a marker into each, and
reads the *escape sequences* rather than the rendered text: SGR 2 is faint, SGR
22 turns it back off. It then moves focus and checks the two panes swapped.

Run: python3 scripts/e2e/check_dim_panes.py
"""

from __future__ import annotations

import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, orphans_after, p2pmux_pids  # noqa: E402

CTRL_P = b"\x10"
COLS, ROWS = 100, 30

# SGR parameter 2 is "faint". A cell drawn dim carries it in the sequence that
# precedes the glyph; the terminal keeps it until 22 (normal intensity) arrives.
SGR = re.compile(r"\x1b\[([0-9;]*)m")


def intensity_at(raw: str, marker: str) -> bool | None:
    """Whether `marker` was written while faint intensity was in effect.

    Replays the SGR stream up to the marker's last occurrence, which is what the
    terminal itself does. Returns None if the marker was never drawn.
    """
    index = raw.rfind(marker)
    if index < 0:
        return None
    faint = False
    for match in SGR.finditer(raw, 0, index):
        for value in (match.group(1) or "0").split(";"):
            code = value or "0"
            if code == "2":
                faint = True
            elif code in ("0", "22"):
                faint = False
    return faint


def main() -> int:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    baseline = p2pmux_pids()
    failures = 0

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        if not ok:
            failures += 1
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    with Harness("dim-panes") as harness:
        peer = harness.spawn(
            "solo", ["create", "--name", "dim", "--session-name", "dim"],
            cols=COLS, rows=ROWS, env=env,
        )
        peer.wait_ready(timeout=30)
        peer.run_in_shell("printf 'AAAAA\\n'")
        time.sleep(1.0)

        # Ctrl+P then r splits to the right. The new pane takes focus.
        peer.send(CTRL_P)
        time.sleep(0.4)
        peer.send(b"r")
        peer.wait_until(lambda s: "Pane #2" in s, timeout=30, what="the second pane")
        peer.run_in_shell("printf 'BBBBB\\n'")
        time.sleep(2.0)

        # Everything the client has drawn since it started.
        raw = peer.raw_text()

        first = intensity_at(raw, "AAAAA")
        second = intensity_at(raw, "BBBBB")
        check("both panes drew their marker", first is not None and second is not None,
              f"pane1={first} pane2={second}")
        if first is None or second is None:
            return 1
        check("the focused pane (2) is at full intensity", second is False, f"faint={second}")
        check("the unfocused pane (1) is faint", first is True, f"faint={first}")

    leaked = orphans_after(baseline)
    if leaked:
        print(f"leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"dim panes: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
