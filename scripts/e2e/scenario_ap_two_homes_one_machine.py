#!/usr/bin/env python3
"""Issue #121: two sessions on one machine that cannot read each other's records.

Scenario AI drives two sessions in one sandbox HOME, and AJ drives three
orderings of the same, and all of them pass on both platforms. What none of them
does is give the two sessions *different* HOMEs -- which is what the daily run
does every time it starts two probes, what happens when a session is started
under `sudo`, and what happens to anyone whose second p2pmux runs as another
user of the same machine.

It matters because of where the label comes from. The observing session's scan
finds the p2pmux node above the other session's agent from the process table,
which is machine-wide; the *name* for that node comes from the session store,
which lives under `HOME`. Two HOMEs, two stores: the node is found, no name is
found, and the row fell back to `running outside p2pmux` -- about an agent
sitting in a pane two windows over, with `enter starts a new conversation`
offering to run a second copy of it.

So: session A in HOME A with a hooked agent in a pane, session B in HOME B, and
B's inbox has to say which of the two things it is looking at.

Run: python3 scripts/e2e/scenario_ap_two_homes_one_machine.py [repeats]

Each session lives in its own sandbox HOME, so the sessions of whoever is
running this are never touched -- but the *scan* sees the whole machine, so every
check below is scoped to the card this scenario put there.
"""

from __future__ import annotations

import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import BINARY, DeadlineExceeded, Harness, orphans_after, p2pmux_pids  # noqa: E402

CTRL_O = b"\x0f"
COLS, ROWS = 120, 36

BLOCKED_ON = "permission: write to /etc/hosts"
ANOTHER_SESSION = "another p2pmux session"
OUTSIDE = "running outside p2pmux"
NO_HOOKS = "state unknown — no hooks"

# `opencode` for scenario AC's two reasons: a loose agent descending from another
# of its own kind is folded into it, and this harness is very often started from
# inside a `claude`.
AGENT_KIND = "opencode"

HOOKED_AGENT = """
notify() {{ printf '{{"message":"%s"}}' "$2" \
  | {binary} notify {kind} --status "$1" >/dev/null 2>&1 || true; }}
echo AGENT-START
notify running "reading the tests"
sleep 2
notify pending '{blocked}'
echo AGENT-BLOCKED
sleep 900
""".strip()


def is_row(line: str) -> bool:
    """Whether this line is an inbox row rather than prose that names an agent.

    The empty state says "Start claude, codex or opencode in any terminal",
    which contains the agent kind. A row carries a status dot.
    """
    return AGENT_KIND in line and ("●" in line or "○" in line)


def card_holding(screen: str, needle: str) -> list[str]:
    """The three lines of the card whose headline matches."""
    lines = screen.split("\n")
    for index, line in enumerate(lines):
        if needle in line and is_row(line):
            return lines[index : index + 3]
    raise AssertionError(f"no card holding {needle!r} on screen:\n{screen}")


def open_inbox(peer, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and "Agents" not in peer.snapshot():
        peer.send(CTRL_O)
        time.sleep(1.5)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}

    # Two harnesses, and therefore two HOMEs: the whole point of the scenario.
    with Harness(f"ap-agent-home-{index}") as theirs, Harness(f"ap-observer-home-{index}") as ours:
        script = theirs.home / "agent.sh"
        script.write_text(
            HOOKED_AGENT.format(binary=BINARY, blocked=BLOCKED_ON, kind=AGENT_KIND) + "\n"
        )

        other = theirs.spawn(
            "other", ["create", "--name", "other", "--session-name", "other"],
            cols=COLS, rows=ROWS, env=env,
        )
        other.wait_ready(timeout=30)
        other.run_in_shell(f"bash -c 'exec -a {AGENT_KIND} /bin/sh {script}'")
        other.wait_for("AGENT-BLOCKED", timeout=60)

        mine = ours.spawn(
            "mine", ["create", "--name", "mine", "--session-name", "mine"],
            cols=COLS, rows=ROWS, env=env,
        )
        mine.wait_ready(timeout=30)
        mine.wait_until(lambda s: "Pane #1" in s, timeout=45, what="the session's first pane")
        open_inbox(mine)

        try:
            screen = mine.wait_until(
                lambda s: any(is_row(line) for line in s.split("\n")),
                timeout=60,
                what="the other session's agent",
            )
        except DeadlineExceeded:
            check("the second session lists the first one's agent", False, mine.snapshot()[:600])
            return results
        check("the second session lists the first one's agent", True)

        # The row can arrive before its state does: the scan and the status
        # records are two different ticks.
        try:
            screen = mine.wait_until(
                lambda s: ANOTHER_SESSION in s,
                timeout=45,
                what="the row saying which session the agent is in",
            )
        except DeadlineExceeded:
            screen = mine.snapshot()

        has_row = any(is_row(line) for line in screen.split("\n"))
        card = "\n".join(card_holding(screen, AGENT_KIND)) if has_row else ""
        summary = " / ".join(line.strip() for line in card.split("\n"))[:220]

        check(
            "and says it is in another p2pmux session, with no name to give for it",
            ANOTHER_SESSION in screen,
            summary,
        )
        check(
            "and never calls a pane two windows over 'running outside p2pmux'",
            OUTSIDE not in screen,
            summary,
        )
        check(
            "and never offers to start a second copy of an agent already running",
            "enter starts a new conversation" not in screen,
            summary,
        )
        check(
            "and still says what the agent is blocked on",
            "needs you" in card and NO_HOOKS not in card,
            summary,
        )
        # A row nothing here can reach must not be counted as a summons.
        tab_bar = screen.split("\n")[0]
        badge = re.search(r"inbox\s+(\d+)", tab_bar)
        check(
            "and puts no number on the inbox badge",
            badge is None,
            f"tab bar: {tab_bar.strip()[:120]!r}",
        )

        # The control: the session hosting the agent is unaffected by any of it.
        open_inbox(other)
        try:
            theirs_screen = other.wait_until(
                lambda s: "needs you" in s, timeout=45, what="the agent's own session's inbox"
            )
        except DeadlineExceeded:
            theirs_screen = other.snapshot()
        check(
            "the session hosting it still says where in itself it is",
            "tab 1 · pane 1" in theirs_screen,
            " / ".join(
                line.strip() for line in theirs_screen.split("\n") if AGENT_KIND in line
            )[:200],
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AP (two homes, one machine) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AP: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AP: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
