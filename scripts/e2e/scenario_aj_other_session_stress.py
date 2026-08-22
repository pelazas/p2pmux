#!/usr/bin/env python3
"""Issue #109: hunting the ordering that makes a pane-hosted agent read as loose.

Scenario AI proves the happy path — agent session first, observer second — and
it passes every time. The 19 Aug daily run saw the opposite on the same build:
the observing session called a pane-hosted agent `running outside p2pmux` and
put it on the badge.

The difference has to be *when* things start relative to each other, because the
wiring is the same either way. `name_their_sessions` caches the node-pid → name
map and re-reads it only when a loose agent turns up whose node it has never
heard of — so any ordering that gets an agent onto the roster while its node is
not yet in the map, and does not later trip that re-read, keeps the wrong label
forever.

So this drives the orderings AI does not:

  observer-first   the observing session exists before the agent's session does
  agent-late       both sessions exist, the agent starts minutes later
  node-restart     the agent's session is killed and restarted underneath

Each is checked for the same two things: the row names the session it is in, and
`running outside p2pmux` is nowhere near it.

Run: python3 scripts/e2e/scenario_aj_other_session_stress.py [repeats]
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

BLOCKED_ON = "permission: write to /etc/hosts"
OUTSIDE = "running outside p2pmux"
NO_HOOKS = "state unknown — no hooks"
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


def agent_script(home: Path) -> Path:
    script = home / "agent.sh"
    script.write_text(
        HOOKED_AGENT.format(binary=BINARY, blocked=BLOCKED_ON, kind=AGENT_KIND) + "\n"
    )
    return script


def open_inbox(peer, timeout: float = 30.0) -> None:
    """Ctrl+O until Home is actually up. One press can land before the TUI is ready."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if "Agents" in peer.snapshot() or "inbox" in peer.snapshot().split("\n")[0]:
            peer.send(CTRL_O)
            time.sleep(1.0)
            if "Agents" in peer.snapshot():
                return
        peer.send(CTRL_O)
        time.sleep(1.5)


def agent_row(screen: str) -> str | None:
    """The inbox line mentioning the agent kind, plus the two lines under it."""
    lines = screen.split("\n")
    for index, line in enumerate(lines):
        if AGENT_KIND in line and "notify" not in line:
            return "\n".join(lines[index : index + 3])
    return None


def verdict(peer, label: str, check) -> None:
    """Wait for the agent's row to name its session, then judge what it says."""
    try:
        screen = peer.wait_until(
            lambda s: agent_row(s) is not None and OUTSIDE not in (agent_row(s) or ""),
            timeout=60,
            what=f"{label}: the agent's row naming its session",
        )
    except DeadlineExceeded:
        screen = peer.snapshot()

    row = agent_row(screen)
    summary = " / ".join(part.strip() for part in (row or "<no row>").split("\n"))[:200]
    check(f"{label}: the agent is on the observer's inbox", row is not None, summary)
    if row is None:
        return
    check(f"{label}: it is not called 'running outside p2pmux'", OUTSIDE not in row, summary)
    check(f"{label}: it does not claim the hooks are missing", NO_HOOKS not in row, summary)


def case_observer_first(harness: Harness, script: Path, env: dict, check) -> None:
    """The observing session is already running when the agent's session starts."""
    mine = harness.spawn(
        "mine", ["create", "--name", "mine", "--session-name", "mine"],
        cols=COLS, rows=ROWS, env=env,
    )
    mine.wait_ready(timeout=30)
    open_inbox(mine)
    # Let the observer settle with an empty roster, so its node-pid cache is
    # populated (or not) before the other session exists at all.
    time.sleep(6)

    other = harness.spawn(
        "other", ["create", "--name", "other", "--session-name", "other"],
        cols=COLS, rows=ROWS, env=env,
    )
    other.wait_ready(timeout=30)
    other.run_in_shell(f"bash -c 'exec -a {AGENT_KIND} /bin/sh {script}'")
    other.wait_for("AGENT-BLOCKED", timeout=60)

    open_inbox(mine)
    verdict(mine, "observer-first", check)


def case_agent_late(harness: Harness, script: Path, env: dict, check) -> None:
    """Both sessions have been up a while before any agent starts."""
    other = harness.spawn(
        "other", ["create", "--name", "other", "--session-name", "other"],
        cols=COLS, rows=ROWS, env=env,
    )
    other.wait_ready(timeout=30)
    mine = harness.spawn(
        "mine", ["create", "--name", "mine", "--session-name", "mine"],
        cols=COLS, rows=ROWS, env=env,
    )
    mine.wait_ready(timeout=30)
    open_inbox(mine)
    # Long enough that the observer has done many roster passes with no loose
    # agent at all, which is the state that leaves its cache never refreshed.
    time.sleep(20)

    other.run_in_shell(f"bash -c 'exec -a {AGENT_KIND} /bin/sh {script}'")
    other.wait_for("AGENT-BLOCKED", timeout=60)
    verdict(mine, "agent-late", check)


CASES = {
    "observer-first": case_observer_first,
    "agent-late": case_agent_late,
}


def run_once(index: int, verbose: bool, only: str | None) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    for name, case in CASES.items():
        if only and name != only:
            continue
        # A fresh harness per case: each one is about what a session sees from
        # a cold start, and reusing a HOME would carry the last case's records.
        with Harness(f"aj-{name}-{index}") as harness:
            case(harness, agent_script(harness.home), env, check)
    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    only = sys.argv[2] if len(sys.argv) > 2 else None
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AJ (other-session orderings) run {index}/{repeats}")
        results = run_once(index, verbose, only)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AJ: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AJ: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
