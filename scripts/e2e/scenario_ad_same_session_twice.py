#!/usr/bin/env python3
"""Category C: one agent, one session, two rows.

Two p2pmux on one machine, both in the *same* session -- which is what a laptop
looks like after a second window rejoins on a ticket it still had. An agent runs
in a pane of the first. The second one's inbox is the session's inbox, and the
agent is in a pane of that session, so it belongs there exactly once.

It arrived twice. Once correctly, from the roster of the node hosting its pane:

    needs you · permission: publish the crate · tab 1 · pane 1

and once from the second node's own scan of the machine, which lists every agent
that is not in one of *its* panes and had no way to tell "in a pane of another
session" from "in a pane of this session, hosted by the other p2pmux here":

    running · state unknown -- no hooks · another p2pmux session · p2pmux attach <this session>

The way in it offered was an attach to the session already on screen. The fleet
rail counted the process twice.

Run: python3 scripts/e2e/scenario_ad_same_session_twice.py [repeats]

Both sessions live in the harness's sandbox HOME, so the sessions of whoever is
running this are never touched -- but the *scan* sees the whole machine, and
every agent open on it lands in the inbox too. Every check below is therefore
scoped to the one card this scenario put there.
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
COLS, ROWS = 120, 40

SESSION = "calgary"
BLOCKED_ON = "permission: publish the crate"
# What a row for an agent in another session says. In this scenario there is no
# other session, so this string appearing at all is the bug.
OTHER_SESSION = "another p2pmux session"

# `opencode`, not `claude`, for the reason scenario AC spells out: a loose agent
# descending from another of its own kind is folded into it, and this harness is
# very often started from inside a `claude`.
AGENT_KIND = "opencode"

# Reported through the pane, which is how a hooked agent in a pane reports and
# what the report this scenario is about had running. The state reaches the
# session over the node hosting the pane, so the correct row is the one that
# says `needs you`; the duplicate is the second node's own scan of the machine,
# which knows the process is running and nothing else.
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


PAGE_OF = re.compile(r"page (\d+) of (\d+)")


def agent_rows(screen: str) -> list[str]:
    """Every line of the inbox that names this scenario's agent kind."""
    return [line for line in screen.split("\n") if AGENT_KIND in line]


def every_page(peer) -> str:
    """The whole inbox, not just the page it opened on.

    A developer's own machine supplies as many `running` rows as it likes, and
    the duplicate this scenario is about is one of them -- so it lands on
    whichever page those rows push it onto, and a check that reads page one
    passes by luck.
    """
    seen = [peer.snapshot()]
    match = PAGE_OF.search(seen[0])
    pages = int(match.group(2)) if match else 1
    for _ in range(pages - 1):
        peer.send(b"l")
        time.sleep(0.6)
        seen.append(peer.snapshot())
    return "\n".join(seen)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}

    with Harness(f"ad-same-session-{index}") as harness:
        script = harness.home / "agent.sh"
        script.write_text(
            HOOKED_AGENT.format(binary=BINARY, blocked=BLOCKED_ON, kind=AGENT_KIND) + "\n"
        )

        host = harness.spawn(
            "host", ["create", "--name", "host", "--session-name", SESSION],
            cols=COLS, rows=ROWS, env=env,
        )
        host.wait_ready(timeout=30)
        ticket = harness.wait_for_ticket(SESSION, timeout=30)

        # `exec -a` for the process name, as everywhere else here: detection
        # matches what the process calls itself, and macOS kills a renamed copy
        # of a signed platform binary.
        host.run_in_shell(f"bash -c 'exec -a {AGENT_KIND} /bin/sh {script}'")
        host.wait_for("AGENT-BLOCKED", timeout=60)

        # The second p2pmux on this machine, in the *same* session. This is the
        # window that rejoined on a ticket it still had.
        guest = harness.spawn(
            "guest", ["join", ticket, "--name", "guest"],
            cols=COLS, rows=ROWS, env=env,
        )
        guest.wait_ready(timeout=30)
        guest.wait_until(lambda s: "Pane #1" in s, timeout=45, what="the shared layout")

        # Ctrl+O toggles, so it is pressed until the inbox is up and not once a
        # tick: a second press on an open inbox closes it again.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and "Agents" not in guest.snapshot():
            guest.send(CTRL_O)
            time.sleep(2.0)

        # The row the session is supposed to have: the pane's own agent, blocked,
        # reached through the node hosting it. Its message stays on that machine's
        # own overlay, so what arrives here is the state and the location.
        try:
            guest.wait_until(
                lambda s: any("needs you" in line for line in agent_rows(s)),
                timeout=90,
                what="the pane agent's row",
            )
        except DeadlineExceeded:
            check("the second p2pmux lists the agent", False, guest.snapshot()[:900])
            return results
        check("the second p2pmux lists the agent", True)

        # The scan that produced the duplicate runs every five seconds and the
        # roster heartbeats every five, so give both a couple of turns to land
        # before deciding there is only one row.
        time.sleep(12.0)
        screen = every_page(guest)

        rows = agent_rows(screen)
        check(
            "the agent is listed once, not once per p2pmux on the machine",
            len(rows) == 1,
            " | ".join(line.strip()[:70] for line in rows),
        )
        check(
            "and listed where Enter reaches it, as a pane of this session",
            any("tab 1 · pane 1" in line for line in screen.split("\n")),
            " | ".join(line.strip()[:70] for line in rows),
        )
        check(
            "no row calls a pane of this session another session",
            OTHER_SESSION not in screen,
            next(
                (line.strip()[:110] for line in screen.split("\n") if OTHER_SESSION in line),
                "",
            ),
        )
        check(
            "and none offers an attach to the session already on screen",
            f"p2pmux attach {SESSION}" not in screen,
            next(
                (
                    line.strip()[:110]
                    for line in screen.split("\n")
                    if f"p2pmux attach {SESSION}" in line
                ),
                "",
            ),
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AD (one agent, two p2pmux, one session) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AD: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AD: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
