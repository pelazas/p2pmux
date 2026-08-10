#!/usr/bin/env python3
"""Category A: does an agent p2pmux did not start reach the inbox with a state?

Issue #78. Since 0.1.6 the inbox lists agents running anywhere on the machine,
not only the ones in p2pmux panes -- and every one of those rows said `state
unknown -- no hooks`, permanently, including for people who had run `p2pmux
setup` and whose hooks were firing on every tool call. The hooks had nowhere to
report: outside a pane there is no `P2PMUX_PANE_ID` and no node socket.

So this drives an agent that is genuinely outside every pane -- started here,
detached, owned by no session -- with hooks that report exactly the way a real
Claude Code registration does, and reads the inbox back:

  * a loose agent that reported `running` says running, with a clock;
  * one that reported `needs you` says so in its own words, and is counted;
  * one with no hooks at all still says `state unknown -- no hooks`;
  * and the row goes when the process does.

Run: python3 scripts/e2e/scenario_y_loose_agent.py [repeats]
"""

import os
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
)

CTRL_O = b"\x0f"

BLOCKED_ON = "permission: publish the crate"

# A hooked agent that is nobody's child. It reports through `p2pmux notify` with
# no pane in its environment, which is the whole point: that is the path a
# `claude` started in iTerm takes, and before #78 it went nowhere.
LOOSE_AGENT = """
echo $$ > "$1"
notify() {{ printf '{{"message":"%s"}}' "$2" | "$3" notify claude --status "$1" >/dev/null 2>&1 || true; }}
notify running "reading the tests" "$2"
sleep 3
notify pending '{blocked_on}' "$2"
sleep 600
""".strip()

# The honest row this feature must not swallow: an agent nothing has reported on
# is still `unknown`, and says so.
BARE_AGENT = """
echo $$ > "$1"
sleep 600
""".strip()


def start_loose_agent(root: Path, name: str, body: str) -> int:
    """Start a process the scan classifies as `claude`, outside every pane.

    Detached into its own subshell so it is orphaned to init: an agent under the
    harness's own process tree would be a child of something p2pmux started, and
    this scenario is about the ones that are not. `exec -a` for the name, for the
    same reasons as every other scenario here -- detection matches the process's
    own name, and a renamed copy of a signed platform binary is killed by macOS.
    """
    root.mkdir(parents=True, exist_ok=True)
    script = root / f"{name}.sh"
    script.write_text(body.format(blocked_on=BLOCKED_ON) + "\n")
    pid_file = root / f"{name}.pid"
    if pid_file.exists():
        pid_file.unlink()
    subprocess.run(
        [
            "/bin/sh",
            "-c",
            f"(exec -a claude /bin/sh {script} {pid_file} {BINARY} &) "
            f"</dev/null >/dev/null 2>&1",
        ],
        check=True,
    )
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if pid_file.exists():
            raw = pid_file.read_text().strip()
            if raw.isdigit():
                return int(raw)
        time.sleep(0.1)
    raise DeadlineExceeded(f"{name} never reported its pid")


def kill(pid: int) -> None:
    subprocess.run(["kill", "-9", str(pid)], check=False, capture_output=True)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    agents: list[int] = []

    with Harness(f"y-loose-{index}") as harness:
        root = harness.home / "loose"
        try:
            agents.append(start_loose_agent(root, "hooked", LOOSE_AGENT))
            agents.append(start_loose_agent(root, "bare", BARE_AGENT))

            peer = harness.spawn("home", [], env=env)
            peer.wait_ready(timeout=30)
            peer.wait_for(r"Agents", timeout=20)

            # The scan runs every five seconds and the agent reports `pending`
            # three seconds in, so two ticks is the first moment both states can
            # have landed.
            time.sleep(12.0)
            screen = peer.snapshot()

            check(
                "an agent outside every pane says `needs you`, in its own words",
                "needs you" in screen and BLOCKED_ON.split(":")[0] in screen,
                screen[:800],
            )
            check(
                "the header counts it, so the badge a human watches includes it",
                "needs you" in screen and "inbox 1" in screen,
                screen.split("\n")[0][:120],
            )
            check(
                "the row says where it is running, which is nowhere p2pmux owns",
                "running outside p2pmux" in screen,
                screen[:800],
            )
            check(
                "an agent nothing reported on still says so, on its own row",
                "state unknown" in screen and "no hooks" in screen,
                screen[:800],
            )

            # And the row is not a fixture: it goes when the process does.
            kill(agents[0])
            agents[0] = 0
            deadline = time.monotonic() + 30
            gone = False
            while time.monotonic() < deadline:
                time.sleep(2.0)
                if "needs you" not in peer.snapshot():
                    gone = True
                    break
            check(
                "the row goes when the agent does",
                gone,
                peer.snapshot()[:600],
            )
        finally:
            for pid in agents:
                if pid:
                    kill(pid)

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario Y (loose agent) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario Y: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario Y: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
