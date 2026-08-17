#!/usr/bin/env python3
"""Issue #98: what the other session's agent is *doing*, not just where it is.

Scenario AC already proves the row for an agent in another p2pmux session names
that session and refuses the click. It gets its state there by stripping
`P2PMUX_PANE_ID` and `P2PMUX_SOCK` from the hook, which sends the report down
the path a bot outside every pane takes. Its own comment says why: reported
through the pane instead, the status "would reach only the node hosting it, and
the session doing the looking is not that one."

That workaround is the bug. A real agent in a real pane has those variables set,
so its status went to one node and no further, and the inbox of every other
session on the same machine showed `state unknown — no hooks` — on a box where
the hooks were installed and firing, to the one person who could answer the
prompt it was blocked on.

So this is scenario AC's setup with the workaround removed: the hook reports the
ordinary way, from inside the pane, and the second session must still say
`needs you`.

The badge is checked too, and checked to stay *empty*. Going to an agent is what
answers its summons and there is no going to this one from here, so a count
including it could only ever rise. Saying the truth in the row and staying out
of the badge are two separate promises and this change must keep both.

Run: python3 scripts/e2e/scenario_ai_other_session_state.py [repeats]

Both sessions live in the harness's sandbox HOME, so the sessions of whoever is
running this are never touched -- but the *scan* sees the whole machine, so every
check below is scoped to the one card this scenario put there.
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
WAY_IN = "another p2pmux session · p2pmux attach other"
NO_HOOKS = "state unknown — no hooks"

# `opencode` rather than `claude`, for scenario AC's two reasons: a loose agent
# descending from another of its own kind is folded into it, and this harness is
# very often started from inside a `claude` -- so a fake claude here would be
# that claude's descendant and never get a row at all. It also sorts to the top.
AGENT_KIND = "opencode"

# The whole point of this scenario: no `env -u`. The hook runs inside the pane
# with the pane's own variables intact, which is what a real agent does and what
# used to leave the other session with nothing to show.
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


def card_holding(screen: str, needle: str) -> tuple[int, list[str]]:
    """The (headline row, three lines) of the card whose location line matches."""
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

    with Harness(f"ai-other-session-state-{index}") as harness:
        script = harness.home / "agent.sh"
        script.write_text(
            HOOKED_AGENT.format(binary=BINARY, blocked=BLOCKED_ON, kind=AGENT_KIND) + "\n"
        )

        other = harness.spawn(
            "other", ["create", "--name", "other", "--session-name", "other"],
            cols=COLS, rows=ROWS, env=env,
        )
        other.wait_ready(timeout=30)
        other.run_in_shell(f"bash -c 'exec -a {AGENT_KIND} /bin/sh {script}'")
        other.wait_for("AGENT-BLOCKED", timeout=60)

        mine = harness.spawn(
            "mine", ["create", "--name", "mine", "--session-name", "mine"],
            cols=COLS, rows=ROWS, env=env,
        )
        mine.wait_ready(timeout=30)
        mine.wait_until(lambda s: "Pane #1" in s, timeout=45, what="the session's first pane")

        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and "Agents" not in mine.snapshot():
            mine.send(CTRL_O)
            time.sleep(2.0)

        try:
            screen = mine.wait_until(
                lambda s: WAY_IN in s, timeout=60, what="the other session's agent"
            )
        except DeadlineExceeded:
            check("the second session lists the first one's agent", False, mine.snapshot()[:600])
            return results
        check("the second session lists the first one's agent", True)

        # The state is what this scenario is for, and the scan that fills it in
        # runs on its own tick -- so the row can arrive before its status does.
        try:
            screen = mine.wait_until(
                lambda s: "needs you" in "\n".join(card_holding(s, WAY_IN)[1]),
                timeout=45,
                what="the other session's agent saying what it needs",
            )
        except (DeadlineExceeded, AssertionError):
            screen = mine.snapshot()

        _, card = card_holding(screen, WAY_IN)
        card_text = "\n".join(card)
        summary = " / ".join(line.strip() for line in card)[:220]

        check("the row says what the agent is blocked on", "needs you" in card_text, summary)
        check(
            "and never falls back to claiming the hooks are missing",
            NO_HOOKS not in card_text,
            summary,
        )
        check(
            "the agent's own words reach the machine they are running on",
            BLOCKED_ON in card_text,
            summary,
        )

        # The badge must stay empty: this row is unreachable from here, so a
        # count including it could never be cleared from here either.
        tab_bar = screen.split("\n")[0]
        badge = re.search(r"inbox\s+(\d+)", tab_bar)
        check(
            "and it still does not put a number on the inbox badge",
            badge is None,
            f"tab bar: {tab_bar.strip()[:120]!r}",
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AI (another session's agent state) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AI: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AI: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
