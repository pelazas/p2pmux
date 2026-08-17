#!/usr/bin/env python3
"""Issue #99: the wheel on a pane that has no scrollback yet.

The daily run of 17 Aug wheeled up on a freshly created, zoomed pane and the
footer became:

    Ctrl+ <p  local scrollback is unavailable for this pane (remote, alternate
    screen, or stale history)

Two things wrong in one line. The reason is three guesses at a local shell one
second old, none of which describes it; and the notice is long enough that the
keybinding hints were drawn into the space it left and cut off mid-chord, so the
bar advertised `Ctrl+ <p` — a chord that does not exist.

This drives the real client on a real PTY at the reported width and checks both:

  * a wheel notch on a pane with no history says nothing at all;
  * the keybinding bar is a whole tier or absent, never a prefix of one;
  * and a pane that *does* have history still scrolls, which is the thing all of
    the above must not have cost.

Zoom is in the repro because it is what put the empty pane under a pointer that
had been over the pane with history — the wheel is addressed by the pointer, and
a zoomed pane owns the whole content area. It is reproduced here for fidelity to
the report, not because zoom is the trigger.

Run: python3 scripts/e2e/scenario_ae_empty_pane_wheel.py [repeats]
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402

CTRL_P = b"\x10"

# The exact width from the report: 17 columns of floor tier plus a 92-column
# notice needs 109, so 99 is comfortably inside the range that used to clip.
COLS = 99
ROWS = 30

STALE_MESSAGE = "local scrollback is unavailable"


def footer_of(screen: str) -> str:
    """The last non-blank line, which is the help bar."""
    for line in reversed(screen.split("\n")):
        if line.strip():
            return line
    return ""


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(
                f"    {'PASS' if ok else 'FAIL'}  {name}"
                + (f"  -- {detail}" if detail and not ok else "")
            )

    with Harness(f"ae-empty-pane-wheel-{index}") as harness:
        host, _ticket = harness.create_room("host", cols=COLS, rows=ROWS)
        host.wait_ready()

        # Give pane #1 a history worth scrolling, so the last check below is
        # about this change rather than about an empty session.
        host.run_in_shell("seq 1 200")
        host.wait_for("200", timeout=30)
        host.settle(quiet_for=1.0, timeout=15)

        # Ctrl+P n: a second pane, brand new and therefore empty.
        host.send(CTRL_P)
        time.sleep(0.35)
        host.send(b"n")
        time.sleep(1.2)

        # Ctrl+P z: zoom it, which is what put it under the pointer.
        host.send(CTRL_P)
        time.sleep(0.35)
        host.send(b"z")
        time.sleep(1.2)
        host.settle(quiet_for=0.8, timeout=10)

        # More than one notch: the first is absorbed returning to the live edge,
        # and the report's message came from the ones after it.
        host.wheel_up(COLS // 2, ROWS // 2, times=6)
        time.sleep(1.5)

        screen = host.snapshot()
        footer = footer_of(screen)

        check(
            "an empty pane's wheel does not claim the history is remote or stale",
            STALE_MESSAGE not in screen,
            f"footer: {footer!r}",
        )

        # The floor tier is `Ctrl+ <p> <t> <q>`. Anything shorter that still
        # starts with `Ctrl` is the clipped bar from the report.
        has_ctrl = "Ctrl" in footer
        whole_tier = "<p>" in footer and "<t>" in footer and "<q>" in footer
        check(
            "the keybinding bar is a whole tier or none of one",
            (not has_ctrl) or whole_tier,
            f"footer: {footer!r}",
        )

        # Unzoom and confirm the pane that does have history still scrolls --
        # the regression this fix must not have introduced.
        host.send(CTRL_P)
        time.sleep(0.35)
        host.send(b"z")
        time.sleep(1.0)
        host.send(CTRL_P)
        time.sleep(0.35)
        host.send(b"\x1b[A")  # focus up, back to pane #1
        time.sleep(1.0)
        host.settle(quiet_for=0.8, timeout=10)

        before = host.snapshot()
        host.wheel_up(COLS // 2, 4, times=6)
        time.sleep(1.5)
        after = host.snapshot()

        check(
            "a pane that does have history still scrolls",
            before != after,
            "the wheel moved nothing on a pane holding 200 lines",
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = True
    failures: list[tuple[str, str]] = []
    for index in range(repeats):
        print(f"  run {index + 1}/{repeats}")
        for name, ok, detail in run_once(index, verbose):
            if not ok:
                failures.append((name, detail))
    if failures:
        print(f"\nFAILED ({len(failures)}):")
        for name, detail in failures:
            print(f"  - {name}: {detail}")
        return 1
    print("\nall checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
