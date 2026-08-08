#!/usr/bin/env python3
"""Category A: does the inbox do what it claims, end to end, on real processes?

Every other check of Home is a unit test over a struct. This one drives the real
binary on a real PTY, with a real agent process reporting through the real hook
path, and reads the bytes the client painted back out.

It walks the parts of the definition of done that only a live session can prove:

  * bare `p2pmux` opens Home rather than a session picker;
  * an agent that reports `needs you` through a hook reaches the inbox, the
    header count and the tab-bar badge;
  * an agent nothing has reported on says so on its own row, and never reaches
    that count -- the trust rule, observed rather than asserted;
  * Enter lands in that agent's terminal, alone on screen, with keystrokes
    reaching the program;
  * Ctrl+O comes back from inside that live pane, and Esc does not.

Run: python3 scripts/e2e/scenario_w_inbox.py [repeats]
"""

import os
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
CTRL_P = b"\x10"

# A hooked agent: it reports through `p2pmux notify`, exactly as a real Claude
# Code registration does. Nothing here rings a bell or relies on silence --
# agent state is hooks-only, and this scenario exists partly to prove it.
#
# The `pending` push carries a message because the product refuses one that
# does not: a row that says `needs you` and cannot say what for is noise, and
# `derive_claude` drops it. Sending one without text would test the drop, not
# the inbox.
BLOCKED_ON = "permission: write to /etc/hosts"
HOOKED_AGENT = """
notify() {{ printf '{{"message":"%s"}}' "$2" | {binary} notify claude --status "$1" >/dev/null 2>&1 || true; }}
echo HOOKED-AGENT-START
notify running working
sleep 2
notify pending '{blocked_on}'
echo HOOKED-AGENT-BLOCKED
sleep 600
""".strip()

# An agent with no hooks at all: it only has to look like `claude` to the process
# scan. Detection knows it is alive and must claim nothing more.
BARE_AGENT = """
echo BARE-AGENT-START
sleep 600
""".strip()


def install_agent(home: Path, name: str, body: str) -> str:
    """Write an agent script and the command that runs it under argv[0] `claude`.

    Detection matches the process's own name, so the script cannot simply be
    called `claude`: a shebang script is reported as its interpreter and is
    never classified, and copying a signed platform binary under a new name is
    killed by macOS. `exec -a` gives the real /bin/sh the right argv[0] with no
    such problem.
    """
    bindir = home / "fakebin"
    bindir.mkdir(parents=True, exist_ok=True)
    script = bindir / f"{name}.sh"
    script.write_text(body.format(binary=BINARY, blocked_on=BLOCKED_ON) + "\n")
    return f"/bin/sh -c 'exec -a claude /bin/sh {script}'"


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}

    with Harness(f"w-inbox-{index}") as harness:
        hooked = install_agent(harness.home, "hooked", HOOKED_AGENT)
        bare = install_agent(harness.home, "bare", BARE_AGENT)

        # No arguments at all. A fresh HOME has no session and no pairing, so
        # this exercises the whole first-run path: start a session, land on Home.
        peer = harness.spawn("home", [], env=env)
        peer.wait_ready(timeout=30)

        try:
            peer.wait_for(r"Agents", timeout=20)
            check("bare `p2pmux` opens Home, not a session picker", True)
        except DeadlineExceeded as error:
            check(
                "bare `p2pmux` opens Home, not a session picker",
                False,
                str(error)[:300],
            )

        screen = peer.snapshot()
        check(
            "the first-run empty state says what to do",
            "Start an agent in any terminal and it appears here." in screen,
            screen[:400],
        )
        check(
            "the tab bar carries an inbox badge with no count",
            "inbox" in screen and "inbox 0" not in screen,
            screen.split("\n")[0][:120],
        )
        check(
            "the key bar is visible without looking anything up",
            "enter open" in screen and "q quit" in screen,
            screen[-200:],
        )

        # `n` from Home has to give a first run somewhere to go.
        peer.send(b"n")
        time.sleep(2.0)
        check(
            "n opens a terminal and leaves Home for it",
            "Pane #1" in peer.snapshot(),
            peer.snapshot()[:200],
        )

        # Two agents in two panes: one that reports, one that never will.
        peer.run_in_shell(bare)
        peer.wait_for("BARE-AGENT-START", timeout=20)
        peer.send(CTRL_P)
        time.sleep(0.3)
        peer.send(b"n")
        time.sleep(2.0)
        peer.run_in_shell(hooked)
        peer.wait_for("HOOKED-AGENT-BLOCKED", timeout=30)

        # Back to Home, and give the sampler a tick to see both panes.
        peer.send(CTRL_O)
        time.sleep(3.0)
        screen = peer.snapshot()

        check(
            "a hook's `needs you` reaches the inbox, in the agent's own words",
            "needs you" in screen and "permission" in screen,
            screen[:600],
        )
        check(
            "the header counts exactly the agent a hook reported as blocked",
            "Agents · 1 needs you" in screen,
            [line for line in screen.split("\n") if "Agents" in line][:2],
        )
        check(
            "the badge carries the same count",
            "inbox 1" in screen,
            screen.split("\n")[0][:120],
        )
        check(
            "an agent with no hooks says so on its own row",
            "state unknown" in screen and "no hooks" in screen,
            screen[:600],
        )
        blocked_line = next(
            (i for i, line in enumerate(screen.split("\n")) if "needs you" in line), None
        )
        unknown_line = next(
            (i for i, line in enumerate(screen.split("\n")) if "state unknown" in line), None
        )
        check(
            "a row that needs you never appears below one that does not",
            blocked_line is not None
            and unknown_line is not None
            and blocked_line < unknown_line,
            f"blocked at {blocked_line}, unreported at {unknown_line}",
        )

        # Enter lands in the selected agent's terminal, alone on screen.
        peer.send(b"\r")
        time.sleep(2.0)
        screen = peer.snapshot()
        check(
            "Enter opens that agent's terminal",
            "HOOKED-AGENT-BLOCKED" in screen,
            screen[:400],
        )
        check(
            "the terminal is full screen, not the pane grid",
            screen.count("host: ") == 1,
            f"{screen.count('host: ')} pane titles on screen",
        )

        # Esc must reach the program: swallowing it would break the very agents
        # this screen exists to manage.
        peer.send(b"\x1b")
        time.sleep(0.8)
        check(
            "Esc stays with the program in the pane",
            "Agents" not in peer.snapshot().split("\n")[2],
            peer.snapshot()[:200],
        )

        # Keystrokes reach the program, from inside the zoom.
        peer.type("echo INSIDE-THE-ZOOM\n")
        try:
            peer.wait_for("INSIDE-THE-ZOOM", timeout=15)
            check("keystrokes reach the program in the opened terminal", True)
        except DeadlineExceeded as error:
            check(
                "keystrokes reach the program in the opened terminal",
                False,
                str(error)[:300],
            )

        # And Ctrl+O comes back, from inside that live pane.
        peer.send(CTRL_O)
        time.sleep(1.5)
        screen = peer.snapshot()
        check(
            "Ctrl+O returns to Home from inside a live pane",
            "needs you" in screen,
            screen[:400],
        )

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("P2PMUX_E2E_VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario W (inbox) run {index}/{repeats}")
        results = run_once(index, verbose)
        run_failures = [name for name, ok, _ in results if not ok]
        failures += len(run_failures)
        print(f"  {len(results) - len(run_failures)}/{len(results)} checks passed")

    leaked = orphans_after(baseline)
    if leaked:
        print(f"FAIL  the run leaked p2pmux processes: {sorted(leaked)}")
        failures += 1
    print("scenario W:", "PASS" if failures == 0 else f"FAIL ({failures})")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
