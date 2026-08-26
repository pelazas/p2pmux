#!/usr/bin/env python3
"""Issue #121: the inbox that has already seen an agent outside p2pmux.

Scenario AI proves the happy path and AJ proves three orderings, and all of them
pass on both platforms. What none of them has is the state the machine in the
daily run is always in: an agent that is *genuinely* outside p2pmux -- somebody's
own `claude` in another terminal -- running before the observing session starts.

That matters because of how the observer learns which session a pane-hosted
agent belongs to. Its scan can see that the agent's process descends from a
node; only the session store knows that node is called `dakar`. The map from
node pid to name is read from disk once and re-read only when an agent turns up
whose node is not in it -- and "whose node" is keyed on the node having been
identified at all. A bystander's node pid is 0, forever and correctly. So a
bystander is exactly the thing that can get the map loaded early, while it holds
nothing, and then never ask for it again.

  1. a hooked agent starts outside every pane
  2. the observing session starts, and its inbox lists the bystander
  3. only then does the agent's own session start, with an agent in a pane

The pane-hosted agent must read as `another p2pmux session`, the bystander must
still read as `running outside p2pmux`, and the badge must count neither: one is
unreachable from here and the other is only `running`.

Run: python3 scripts/e2e/scenario_ao_bystander_then_session.py [repeats]

Both sessions live in the harness's sandbox HOME, so the sessions of whoever is
running this are never touched -- but the *scan* sees the whole machine, so every
check below is scoped to the two cards this scenario put there.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import (  # noqa: E402
    BINARY,
    DeadlineExceeded,
    Harness,
    orphans_after,
    p2pmux_pids,
    sandbox_environ,
)

CTRL_O = b"\x0f"
COLS, ROWS = 120, 36

BLOCKED_ON = "permission: write to /etc/hosts"
WAY_IN = "another p2pmux session · p2pmux attach other"
OUTSIDE = "running outside p2pmux"
NO_HOOKS = "state unknown — no hooks"

# Two kinds, so the two rows can never be mistaken for each other: the bystander
# is a `claude` and the pane-hosted agent is an `opencode`.
BYSTANDER_KIND = "claude"
PANE_KIND = "opencode"

PANE_AGENT = """
notify() {{ printf '{{"message":"%s"}}' "$2" \
  | {binary} notify {kind} --status "$1" >/dev/null 2>&1 || true; }}
echo AGENT-START
notify running "reading the tests"
sleep 2
notify pending '{blocked}'
echo AGENT-BLOCKED
sleep 900
""".strip()

# The bystander never asks for anything: `running` keeps it off the badge, so
# the badge assertion below is about the pane-hosted agent alone.
BYSTANDER = """
echo $$ > "$1"
printf '{"message":"reading somebody else terminal"}' \
  | "$2" notify claude --status running >/dev/null 2>&1 || true
sleep 900
""".strip()


def start_bystander(home: Path) -> int:
    """An agent that is nobody's child, hooked, in this run's sandbox HOME.

    Detached into its own subshell so it is orphaned to init: an agent under the
    harness's own tree would descend from something this scenario started, and
    the whole point of it is that it descends from nothing p2pmux knows.
    """
    script = home / "bystander.sh"
    script.write_text(BYSTANDER + "\n")
    pid_file = home / "bystander.pid"
    subprocess.run(
        [
            "/bin/bash",
            "-c",
            f"(exec -a {BYSTANDER_KIND} /bin/sh {script} {pid_file} {BINARY} &) "
            f"</dev/null >/dev/null 2>&1",
        ],
        check=True,
        # The sandbox HOME, so its status record lands in the store the peers
        # below read -- and without a live pane's markers, which this process
        # inherits whenever the suite is run from a terminal inside p2pmux. An
        # agent that has them is not outside p2pmux at all.
        env={**sandbox_environ(), "HOME": str(home), "P2PMUX_TELEMETRY": "0"},
    )
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if pid_file.exists() and pid_file.read_text().strip().isdigit():
            return int(pid_file.read_text().strip())
        time.sleep(0.1)
    raise DeadlineExceeded("the bystander never reported its pid")


def card_holding(screen: str, needle: str) -> list[str]:
    """The three lines of the card whose location line matches."""
    lines = screen.split("\n")
    for index, line in enumerate(lines):
        if needle in line:
            return lines[max(0, index - 2) : index + 1]
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
    bystander = None

    with Harness(f"ao-bystander-{index}") as harness:
        try:
            bystander = start_bystander(harness.home)

            mine = harness.spawn(
                "mine", ["create", "--name", "mine", "--session-name", "mine"],
                cols=COLS, rows=ROWS, env=env,
            )
            mine.wait_ready(timeout=30)
            open_inbox(mine)
            # The observer must actually have seen the bystander before the
            # other session exists: that sighting is what loads its session map
            # while the map has nothing in it worth having.
            try:
                mine.wait_until(
                    lambda s: OUTSIDE in s, timeout=60, what="the bystander's row"
                )
            except DeadlineExceeded:
                check("the observer sees the agent outside p2pmux first", False, mine.snapshot()[:600])
                return results
            check("the observer sees the agent outside p2pmux first", True)

            script = harness.home / "agent.sh"
            script.write_text(
                PANE_AGENT.format(binary=BINARY, blocked=BLOCKED_ON, kind=PANE_KIND) + "\n"
            )
            other = harness.spawn(
                "other", ["create", "--name", "other", "--session-name", "other"],
                cols=COLS, rows=ROWS, env=env,
            )
            other.wait_ready(timeout=30)
            other.run_in_shell(f"bash -c 'exec -a {PANE_KIND} /bin/sh {script}'")
            other.wait_for("AGENT-BLOCKED", timeout=60)

            open_inbox(mine)
            try:
                screen = mine.wait_until(
                    lambda s: WAY_IN in s,
                    timeout=60,
                    what="the other session's agent naming its session",
                )
            except DeadlineExceeded:
                screen = mine.snapshot()

            rows = [line for line in screen.split("\n") if PANE_KIND in line]
            summary = " / ".join(line.strip() for line in rows)[:200]
            check("the pane-hosted agent is on the observer's inbox", bool(rows), screen[:600])
            if not rows:
                return results

            check("and it names the session it is in", WAY_IN in screen, summary)
            card = "\n".join(card_holding(screen, WAY_IN)) if WAY_IN in screen else ""
            check(
                "and is never called 'running outside p2pmux'",
                PANE_KIND not in "\n".join(
                    line for line in screen.split("\n") if OUTSIDE in line
                ),
                summary,
            )
            check(
                "and still says what it is blocked on",
                "needs you" in card and NO_HOOKS not in card,
                " / ".join(line.strip() for line in card.split("\n"))[:200],
            )
            # The bystander is the control: it must not be relabelled either.
            check(
                "the bystander is still outside p2pmux",
                OUTSIDE in screen,
                summary,
            )
            tab_bar = screen.split("\n")[0]
            badge = re.search(r"inbox\s+(\d+)", tab_bar)
            check(
                "and nothing has put a number on the inbox badge",
                badge is None,
                f"tab bar: {tab_bar.strip()[:120]!r}",
            )
        finally:
            if bystander:
                subprocess.run(["kill", "-9", str(bystander)], check=False, capture_output=True)

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AO (a bystander, then a session) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AO: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AO: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
