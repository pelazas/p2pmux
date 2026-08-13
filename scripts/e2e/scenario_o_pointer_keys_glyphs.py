#!/usr/bin/env python3
"""The three client-side reports from 2026-08-13, each driven through a real PTY.

  #84  the wheel over a pane that does not have focus
  #85  shift+tab reaching the pane's child
  #82  an emoji-presentation glyph not eating the cell beside it

All three live in the attached client, between the terminal and the node, which is
why they survived every unit test the node has: nothing below the client can see
them. Here the bytes are the real ones -- SGR mouse reports and `ESC [ Z` written
into a master fd, and the client's own output rendered back out.

Each check is written so that the *old* behaviour fails it:

  #84  the focused pane's child is silent about the mouse and the pane under the
       pointer is a full-screen app that wants it. Reading the protocol off focus
       sent that notch to local scrollback, which a pane on the alternate screen
       has none of -- so the client said "local scrollback is unavailable" and the
       child never saw the wheel.
  #85  `cat -v` spells out what reaches the pane. Shift+Tab used to encode to
       nothing at all, so nothing arrived.
  #82  `✳️` measures two columns and occupies one. The character next to it
       used to be dropped from ratatui's diff and painted over by the glyph's
       second half.

Run: python3 scripts/e2e/scenario_o_pointer_keys_glyphs.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, p2pmux_pids, orphans_after  # noqa: E402

# The glyph from the report: an eight-spoked asterisk asking for emoji
# presentation. `unicode-width` reads the pair as two columns; a pane's grid
# gives it one.
VS16_GLYPH = "✳️"
# Distinctive enough that finding it proves the cell beside the glyph survived,
# and plain ASCII so no other width question is mixed into the answer.
NEIGHBOURS = "QWERTY"

UNAVAILABLE = "local scrollback is unavailable"


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(
                f"    {'PASS' if ok else 'FAIL'}  {name}"
                + (f"  -- {detail}" if detail and not ok else "")
            )

    with Harness(f"o-pointer-{index}") as harness:
        host, _ticket = harness.create_room("host", cols=120, rows=40)
        host.wait_ready()
        host.settle(quiet_for=1.0, timeout=15)

        # ---------------------------------------------------------------- #82
        # A grid column is what the pane laid out for this glyph; the client
        # must not hand the terminal a symbol that wants two.
        host.run_in_shell(f"printf '{VS16_GLYPH}{NEIGHBOURS}\\n'")
        # Waited for, not asserted on: when the glyph eats its neighbour this
        # never arrives, and a timeout here would abort the scenario before the
        # check below could name what went wrong.
        try:
            host.wait_for(NEIGHBOURS[:3], timeout=12)
        except Exception:
            pass
        host.settle(quiet_for=0.8, timeout=10)
        glyph_screen = host.snapshot()
        check(
            "the character beside an emoji-presentation glyph is not eaten",
            NEIGHBOURS in glyph_screen,
            f"{NEIGHBOURS!r} missing from the pane:\n{glyph_screen[:600]}",
        )

        # ---------------------------------------------------------------- #85
        # `cat -v` names every byte that reaches the pane, so an arriving
        # Shift+Tab is unambiguous and a swallowed one is too.
        host.run_in_shell("cat -v")
        time.sleep(1.0)
        host.send(b"\x1b[Z")
        time.sleep(1.5)
        host.settle(quiet_for=0.8, timeout=10)
        shift_tab_screen = host.snapshot()
        check(
            "shift+tab reaches the pane's child",
            "^[[Z" in shift_tab_screen,
            f"no ESC [ Z in the pane:\n{shift_tab_screen[-600:]}",
        )
        # Ctrl+C only. Ctrl+D would end the shell too, and a pane that exits
        # takes the session's last pane with it and lands the client on the
        # inbox, where there is nothing left to aim a wheel at.
        host.key("ctrl_c")
        time.sleep(1.0)
        host.settle(quiet_for=0.8, timeout=10)

        # ---------------------------------------------------------------- #84
        # Two panes side by side. The pointer goes to the right-hand one while
        # focus stays on the left.
        # Ctrl+P is pane mode. (Ctrl+A is the legacy way into the inbox, not
        # into the chord — sending it here lands on Home and every check below
        # then measures the wrong screen.)
        host.key("ctrl_p")
        time.sleep(0.5)
        host.type("r")
        time.sleep(3.0)
        host.settle(quiet_for=1.0, timeout=15)

        panes = host.snapshot()
        # A split shows up as a vertical run of border characters in the middle
        # of the content rows, which a single pane never has.
        content = [line for line in panes.split("\n") if line.startswith("│")]
        split_rows = sum(1 for line in content if line.count("│") > 2)
        check(
            "the session has two panes to aim at",
            split_rows > len(content) // 2,
            f"the split did not take ({split_rows}/{len(content)} rows divided):\n"
            + panes[:800],
        )

        # The new pane takes focus. Turn on mouse reporting *and* the alternate
        # screen in it: this is the pane the wheel is aimed at, and the one that
        # has no local scrollback to fall back on.
        host.type(r"printf '\033[?1049h\033[?1000h\033[?1006h'")
        host.send(b"\r")
        time.sleep(1.5)
        host.settle(quiet_for=0.8, timeout=10)

        # Hand focus back to the left pane, which is an ordinary shell and says
        # nothing about the mouse.
        host.key("ctrl_p")
        time.sleep(0.5)
        host.key("left")
        time.sleep(2.0)
        host.settle(quiet_for=0.8, timeout=10)

        before_wheel = host.snapshot()
        # Somewhere inside the right-hand pane: the split is even, so three
        # quarters across is comfortably past the border.
        host.wheel_up(90, 12, times=3)
        time.sleep(2.0)
        host.settle(quiet_for=0.8, timeout=10)
        after_wheel = host.snapshot()

        check(
            "a wheel over an unfocused pane is not answered with 'scrollback unavailable'",
            UNAVAILABLE not in after_wheel,
            "the client claimed the pane has no scrollback:\n"
            + "\n".join(
                line for line in after_wheel.split("\n") if UNAVAILABLE in line
            ),
        )
        # The shell behind the alternate screen echoes what it is sent, so a
        # forwarded report lands as text. The pane's own parser eats the leading
        # `ESC [ <`, leaving e.g. `64;91;13M`.
        forwarded = re.search(r"6[45];\d+;\d+M", after_wheel)
        check(
            "the wheel reaches the child of the pane it was aimed at",
            forwarded is not None,
            "no mouse report in the unfocused pane:\n"
            + f"before:\n{before_wheel[-500:]}\nafter:\n{after_wheel[-500:]}",
        )

        for peer in (host,):
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:300])
            check(f"{peer.name} still alive", peer.alive, f"exit={peer.exit_code}")

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario O: pointer, keys and glyphs in the attached client ({repeats}x)")
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
