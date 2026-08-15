#!/usr/bin/env python3
"""Category C: the inbox row that pointed at nothing.

Two p2pmux sessions on one machine, which is what a working Mac looks like by
Thursday. An agent runs in a pane of the first; the second lists it, because
the inbox lists every agent on the machine and not only the ones in its own
panes.

Opening that row used to run `p2pmux attach other` in a fresh pane -- a whole
second p2pmux nested inside a pane of the first. On a session that already has
a terminal, which is the common case since a node serves one at a time, that
ends at `Error: already attached` and leaves an empty shell. The report this
scenario exists for was, in full: "it just takes me to a new tab with a new
terminal, empty".

So the row is drawn dim, the cursor walks past it, and the click does nothing
at all. What it does instead is say where the agent actually is:

    another p2pmux session · p2pmux attach other

Run: python3 scripts/e2e/scenario_ac_other_session.py [repeats]

Both sessions live in the harness's sandbox HOME, so the sessions of whoever is
running this are never touched -- but the *scan* sees the whole machine, and
every claude open on it lands in the inbox too. Every check below is therefore
scoped to the one card this scenario put there.
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import BINARY, DeadlineExceeded, Harness, orphans_after, p2pmux_pids  # noqa: E402

CTRL_O = b"\x0f"
COLS, ROWS = 120, 36

BLOCKED_ON = "permission: publish the crate"
# The line the row has to carry: which session, and the command that reaches it.
WAY_IN = "another p2pmux session · p2pmux attach other"

# `opencode`, not `claude`, and the choice is load-bearing on two counts.
#
# A loose agent that descends from another loose agent *of its own kind* is
# folded into it and only the outermost gets a row -- a daemon's workers are one
# agent. This harness is very often started from inside a `claude`, so a fake
# claude here is that claude's descendant and never gets a row at all: the
# scenario would fail on the developer's own machine and nowhere else.
#
# It also sorts. The state below puts this row at the top of the list, above
# every `running` row the machine happens to have open, which is what keeps the
# card on page one of an inbox this scenario does not control the length of.
AGENT_KIND = "opencode"

# An agent in a pane, reporting the way a real registration does -- but with the
# pane's own variables stripped, so the report takes the path a hook outside
# every pane takes and lands in the status ledger this machine's *other* client
# reads. Reported through the pane instead, it would reach only the node hosting
# it, and the session doing the looking is not that one.
HOOKED_AGENT = """
notify() {{ printf '{{"message":"%s"}}' "$2" \
  | env -u P2PMUX_PANE_ID -u P2PMUX_SOCK {binary} notify {kind} --status "$1" >/dev/null 2>&1 || true; }}
echo AGENT-START
notify running "reading the tests"
sleep 2
notify pending '{blocked}'
echo AGENT-BLOCKED
sleep 900
""".strip()


def card_holding(screen: str, needle: str) -> tuple[int, list[str]]:
    """The (headline row, three lines) of the card whose location line matches.

    A full card is headline, what the agent said, where it is running -- so the
    needle is the third line and the headline is two above it. Returned as a
    screen row so the click below lands on the card rather than near it.
    """
    lines = screen.split("\n")
    for index, line in enumerate(lines):
        if needle in line:
            headline = max(0, index - 2)
            return headline, lines[headline : index + 1]
    raise AssertionError(f"no card holding {needle!r} on screen:\n{screen}")


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}

    with Harness(f"ac-other-session-{index}") as harness:
        script = harness.home / "agent.sh"
        script.write_text(
            HOOKED_AGENT.format(binary=BINARY, blocked=BLOCKED_ON, kind=AGENT_KIND) + "\n"
        )

        # The session the agent lives in, and which this scenario is never
        # attached to again after this point.
        other = harness.spawn(
            "other", ["create", "--name", "other", "--session-name", "other"],
            cols=COLS, rows=ROWS, env=env,
        )
        other.wait_ready(timeout=30)
        # `exec -a` for the process name, as everywhere else here: detection
        # matches what the process calls itself, and macOS kills a renamed copy
        # of a signed platform binary.
        other.run_in_shell(f"bash -c 'exec -a {AGENT_KIND} /bin/sh {script}'")
        other.wait_for("AGENT-BLOCKED", timeout=60)

        # The session the user is sitting in. `create`, not bare: bare p2pmux
        # would find the live session above and attach to it, which is the one
        # thing this scenario must not do.
        mine = harness.spawn(
            "mine", ["create", "--name", "mine", "--session-name", "mine"],
            cols=COLS, rows=ROWS, env=env,
        )
        mine.wait_ready(timeout=30)
        # The first non-blank frame is the trust warning `create` prints, not
        # the client: a Ctrl+O sent then goes nowhere. Wait for the pane.
        mine.wait_until(lambda s: "Pane #1" in s, timeout=45, what="the session's first pane")

        # Ctrl+O toggles, so it is pressed until the inbox is up and not once a
        # tick: a second press on an open inbox closes it again.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and "Agents" not in mine.snapshot():
            mine.send(CTRL_O)
            time.sleep(2.0)

        # The scan runs every five seconds, so the row can be two ticks away.
        try:
            screen = mine.wait_until(
                lambda s: WAY_IN in s, timeout=60, what="the other session's agent"
            )
        except DeadlineExceeded:
            check(
                "the second session lists the first one's agent",
                False,
                mine.snapshot()[:600],
            )
            return results

        check("the second session lists the first one's agent", True)

        headline_row, card = card_holding(screen, WAY_IN)
        check(
            "the row names the session it is in and the command that reaches it",
            WAY_IN in "\n".join(card),
            " / ".join(line.strip() for line in card)[:200],
        )
        check(
            "and never calls it an agent running outside p2pmux",
            "running outside p2pmux" not in "\n".join(card),
            " / ".join(line.strip() for line in card)[:200],
        )

        # --- the cursor walks past it ----------------------------------------
        landed_on = ""
        for _ in range(8):
            mine.send(b"j")
            time.sleep(0.25)
            walked = mine.snapshot()
            if WAY_IN not in walked:
                continue
            row, card = card_holding(walked, WAY_IN)
            if card[0].startswith("›"):
                landed_on = card[0][:120]
                break
        check(
            "the arrows never put the cursor on it",
            not landed_on,
            landed_on,
        )

        # --- and the click, which is where the whole report started -----------
        before = mine.snapshot()
        headline_row, _ = card_holding(before, WAY_IN)
        bar_before = before.split("\n")[0]
        # SGR mouse coordinates are 1-based: screen row r is row r + 1.
        mine.click(4, headline_row + 1)
        time.sleep(2.0)
        after = mine.settle(quiet_for=1.0, timeout=15)
        check(
            "a click on it opens no tab and no pane",
            after.split("\n")[0] == bar_before,
            f"{bar_before.strip()[:90]!r} -> {after.split(chr(10))[0].strip()[:90]!r}",
        )
        check(
            "and leaves the inbox up rather than going somewhere",
            "Agents" in after and WAY_IN in after,
            after[:300],
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AC (agents in another session) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AC: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AC: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
