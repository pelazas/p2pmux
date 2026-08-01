#!/usr/bin/env python3
"""Category E x D: mouse forwarding, and terminal fidelity across mismatched sizes.

Scenario-bank entries combined:
  E. "click and wheel into a pane whose child reports mouse"
  E. "click and wheel into one that does not -- verify p2pmux's own scrollback still scrolls"
  D. "unicode, wide chars, emoji in the stream"
  D. "peers running with very different terminal sizes"

The wheel is the interesting case because p2pmux has to decide, per pane, between two
incompatible behaviours: scroll its own scrollback, or forward the wheel to a child that
does its own scrolling. Getting that backwards means either the user cannot scroll back,
or a full-screen app silently never sees the wheel. Both are tested here, in one session,
with the SAME peer -- so the decision has to be made per pane state, not once at startup.

Mouse input is sent as real SGR (1006) reports, the encoding the client actually enables.

Run: python3 scripts/e2e/scenario_f_mouse.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, diff_screens, mask_link_badge, p2pmux_pids, orphans_after  # noqa: E402

UNICODE = "日本語 ABC 🙂🎉 café ĄŻ"


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

    with Harness(f"f-mouse-{index}") as harness:
        # Deliberately mismatched terminal sizes: the pane grid is host-owned, so a much
        # smaller guest must still track the host's stream without corrupting it.
        host, ticket = harness.create_room("host", cols=120, rows=40)
        guest = harness.join_room("guest", ticket, cols=64, rows=20)
        time.sleep(1.5)

        # --- D: unicode, wide chars and emoji must survive the trip to the guest.
        host.run_in_shell(f"echo '{UNICODE}'")
        try:
            host.wait_for("café", timeout=12)
            guest.wait_for("café", timeout=12)
            unicode_ok = True
        except AssertionError:
            unicode_ok = False
        check("unicode/wide/emoji reaches a much smaller guest", unicode_ok,
              f"guest screen:\n{pane_body(guest.snapshot())[:300]}")

        check(
            "wide characters are not mangled on the host",
            "日本語" in host.snapshot() and "🙂" in host.snapshot(),
            f"host pane:\n{pane_body(host.snapshot())[:300]}",
        )

        # --- E, part 1: a plain shell does NOT report mouse, so the wheel must drive
        #     p2pmux's own scrollback.
        host.type("seq 1 200")
        host.send(b"\r")
        host.wait_for("200", timeout=30)
        host.settle(quiet_for=1.0, timeout=15)

        at_bottom = host.snapshot()
        host.wheel_up(20, 10, times=5)
        time.sleep(2.0)
        scrolled = host.snapshot()

        check(
            "wheel scrolls p2pmux's own scrollback when the child does not report mouse",
            scrolled != at_bottom,
            "screen did not move on wheel-up",
        )

        def last_number(screen: str) -> int | None:
            numbers = re.findall(r"│\s*(\d+)\s*│", screen)
            return int(numbers[-1]) if numbers else None

        before_n, after_n = last_number(at_bottom), last_number(scrolled)
        check(
            "wheel-up moves backwards through history, not forwards",
            before_n is not None and after_n is not None and after_n < before_n,
            f"bottom line went {before_n} -> {after_n}",
        )

        host.wheel_down(20, 10, times=8)
        time.sleep(2.0)
        # Masked: the tab bar's connectivity badge carries a live RTT, so an unmasked
        # whole-screen comparison fails whenever the path jitters between the two
        # snapshots -- a phantom failure that says nothing about scrollback.
        check(
            "wheel-down returns exactly to the live bottom",
            mask_link_badge(host.snapshot()) == mask_link_badge(at_bottom),
            "\n"
            + diff_screens(
                "at bottom",
                mask_link_badge(at_bottom),
                "after scroll down",
                mask_link_badge(host.snapshot()),
            ),
        )

        # --- E, part 2: same pane, same peer, but now the child turns mouse reporting on.
        #     The wheel must be forwarded to the child instead of moving scrollback.
        host.type(r"printf '\033[?1000h\033[?1006h'")
        host.send(b"\r")
        time.sleep(2.0)
        host.settle(quiet_for=0.8, timeout=10)

        before_forward = host.snapshot()
        before_offset_marker = last_number(before_forward)
        host.wheel_up(20, 10, times=3)
        time.sleep(2.0)
        after_forward = host.snapshot()

        # The child (a shell) echoes whatever it receives, so a forwarded SGR wheel report
        # shows up as text on the command line. Only the tail is visible: the pane's own
        # vt100 parser consumes the leading `ESC [ <` as an escape sequence, so what lands
        # on screen is e.g. `64;19;6M` (64 = wheel up, 65 = wheel down), not `[<64;...`.
        forwarded_text = re.search(r"6[45];\d+;\d+M", after_forward)
        check(
            "wheel is forwarded to a child that reports mouse",
            forwarded_text is not None,
            f"no mouse report reached the child:\n{pane_body(after_forward)[-400:]}",
        )
        check(
            "p2pmux scrollback does NOT move while the child owns the wheel",
            last_number(after_forward) == before_offset_marker,
            f"scrollback moved: {before_offset_marker} -> {last_number(after_forward)}",
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
    print(f"Scenario F: mouse forwarding + fidelity across mismatched sizes ({repeats}x)")
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
