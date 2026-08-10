#!/usr/bin/env python3
"""Category A: does dragging a selection past a pane's edge scroll it?

Issue #79. A selection used to stop at the edge of what was on screen: drag to
the top of a pane and the pane sat still, so anything more than a page could not
be selected at all.

This drives the real client on a real PTY, fills a pane with numbered lines,
presses inside it and drags out through the top -- then holds the pointer still,
which is the case that matters: a terminal reports a drag when the cell under
the pointer changes, so a pointer parked above the pane sends nothing at all and
anything driven by drag events alone would scroll one row and stop.

It checks the two halves of the feature separately, because either can work
without the other:

  * the pane scrolls while the button is held past its edge;
  * and the text that scrolled past is in what gets copied -- not just the page
    that happened to be visible when the button came up.

Run: python3 scripts/e2e/scenario_z_selection_scroll.py [repeats]
"""

import os
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import (  # noqa: E402
    DeadlineExceeded,
    Harness,
    orphans_after,
    p2pmux_pids,
)

# SGR button codes: 0 is left-down/up, 32 is motion with the left button held.
LEFT_DOWN = 0
LEFT_DRAG = 32

LINES = 300
FILL = f"for i in $(seq 1 {LINES}); do echo LINE-$i; done"

# Long enough that a one-row-per-tick scroll passes a screenful several times
# over, so "it scrolled a bit and stopped" cannot pass.
HOLD_SECONDS = 3.0


def clipboard_text() -> str:
    if sys.platform != "darwin":
        return ""
    return subprocess.run(["pbpaste"], capture_output=True, text=True).stdout


def set_clipboard(text: str) -> None:
    if sys.platform != "darwin":
        return
    subprocess.run(["pbcopy"], input=text, text=True, check=False)


def line_rows(screen: str) -> list[int]:
    """Rows of the snapshot showing a numbered line, top first."""
    return [index for index, line in enumerate(screen.split("\n")) if "LINE-" in line]


def line_numbers(screen: str) -> list[int]:
    return [int(match) for match in re.findall(r"LINE-(\d+)", screen)]


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    # The copy lands on the real clipboard, because that is what copying means.
    # Put back whatever was there when this is done.
    saved = clipboard_text()
    set_clipboard(f"p2pmux-scenario-z-{index}")

    try:
        with Harness(f"z-selection-{index}") as harness:
            # `--name` as well as `--session-name`: without a display name
            # `create` stops at a prompt and the TUI never draws.
            peer = harness.spawn(
                "host",
                ["create", "--name", "host", "--session-name", f"zsel{index}"],
                env=env,
            )
            peer.wait_ready(timeout=30)
            peer.run_in_shell(FILL)
            peer.wait_for(f"LINE-{LINES}", timeout=30)
            time.sleep(1.0)

            screen = peer.snapshot()
            rows = line_rows(screen)
            if not rows:
                check("the pane filled with numbered lines", False, screen[:400])
                return results
            top, bottom = rows[0], rows[-1]
            visible_before = set(line_numbers(screen))

            # Press on the last line, drag up to the first, then out through the
            # top -- and then stop moving, which is the whole point.
            column = 4
            peer._sgr_mouse(LEFT_DOWN, column, bottom + 1)
            time.sleep(0.2)
            peer._sgr_mouse(LEFT_DRAG, column, top + 1)
            time.sleep(0.2)
            peer._sgr_mouse(LEFT_DRAG, column, top)
            time.sleep(HOLD_SECONDS)

            scrolled = peer.snapshot()
            visible_during = set(line_numbers(scrolled))
            check(
                "the pane scrolls while the pointer is held past its top",
                bool(visible_during) and min(visible_during) < min(visible_before),
                f"before {min(visible_before, default=0)}, during {min(visible_during, default=0)}",
            )
            check(
                "it keeps scrolling for as long as it is held, not one row",
                bool(visible_during)
                and min(visible_before) - min(visible_during) > len(rows),
                f"{min(visible_before, default=0) - min(visible_during, default=0)} rows past "
                f"{len(rows)} on screen",
            )

            peer._sgr_mouse(LEFT_DOWN, column, top, release=True)
            time.sleep(1.5)
            released = peer.snapshot()
            check(
                "releasing copies, and says how much",
                "copied" in released and "line" in released,
                [line for line in released.split("\n") if "copied" in line][:1],
            )

            copied = clipboard_text()
            copied_numbers = [int(match) for match in re.findall(r"LINE-(\d+)", copied)]
            check(
                "the copy holds the lines that scrolled past, not just the visible page",
                len(copied_numbers) > len(rows),
                f"{len(copied_numbers)} lines copied, {len(rows)} were on screen",
            )
            check(
                "the copied lines run unbroken from where the drag ended to where it began",
                copied_numbers == list(range(copied_numbers[0], copied_numbers[-1] + 1))
                if copied_numbers
                else False,
                f"{copied_numbers[:3]} … {copied_numbers[-3:]}" if copied_numbers else "none",
            )
    except DeadlineExceeded as error:
        check("the session stayed up", False, str(error)[:300])
    finally:
        set_clipboard(saved)

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario Z (selection scroll) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario Z: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario Z: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
