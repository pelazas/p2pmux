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

import json
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
    # `bash -c`, not `sh -c`: `exec -a` is a bashism, and on Linux /bin/sh is
    # dash, where the whole command is a syntax error and the agent never
    # starts. The script itself stays POSIX.
    return f"/bin/bash -c 'exec -a claude /bin/sh {script}'"


# A release nobody could be running, so the notice is unambiguous when it shows.
NEWER_VERSION = "99.0.0"


def seed_update_check(home: Path, version: str) -> None:
    """Answer the update check from its own cache, so no test needs a network.

    The cache is exactly what a check that already ran leaves behind, and it is
    what the inbox reads. Stamping it here exercises everything from the cache
    outwards: the version comparison, the upgrade command, and the line.
    """
    directory = home / ".config" / "p2pmux"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "update-check.json").write_text(
        json.dumps({"latest": f"v{version}", "checked_unix_ms": int(time.time() * 1000)})
    )


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
        seed_update_check(harness.home, NEWER_VERSION)

        # No arguments at all. A fresh HOME has no session and no pairing, so
        # this exercises the whole first-run path.
        peer = harness.spawn("home", [], env=env)
        peer.wait_ready(timeout=30)

        # Since `13124c2` that path ends *in the session*, not on the inbox: a
        # session created a moment ago has one pane and no agents in it, so Home
        # would open on an empty list -- the emptiest screen there is, in front
        # of the reader least able to interpret it. The inbox is one keystroke
        # away, and everything below is about the inbox.
        try:
            peer.wait_for(r"Pane #1", timeout=20)
            check("bare `p2pmux` lands in a session", True)
        except DeadlineExceeded as error:
            check("bare `p2pmux` lands in a session", False, str(error)[:300])

        peer.send(CTRL_O)
        time.sleep(1.0)
        try:
            peer.wait_for(r"Agents", timeout=20)
            check("Ctrl+O opens the inbox from there", True)
        except DeadlineExceeded as error:
            check("Ctrl+O opens the inbox from there", False, str(error)[:300])

        screen = peer.snapshot()
        # Since 0.1.6 the inbox lists every agent on the machine, not only the
        # ones in p2pmux panes -- so on a developer's own Mac it is rarely
        # empty and rarely all-unreported, whatever this scenario started. The
        # checks that are *about* those two states can only run where they hold.
        machine_is_quiet = "Nothing running yet." in screen
        if machine_is_quiet:
            check(
                # A checklist since `bc810a9`: the screen is emptiest exactly
                # when its reader is newest, so the steps go in the room the
                # cards freed.
                "the first-run empty state says what to do",
                "p2pmux setup" in screen and "Start claude" in screen,
                screen[:400],
            )
        else:
            print("    NOTE  agents are running outside this scenario; "
                  "skipping the two empty-inbox checks")
        check(
            "the tab bar carries an inbox badge with no count",
            "inbox" in screen and "inbox 0" not in screen,
            screen.split("\n")[0][:120],
        )
        # Issue #77. The version is seeded into the check's own cache rather
        # than fetched, so this asserts what the inbox does with an answer and
        # never depends on GitHub being reachable from a test machine. Polled
        # rather than sampled: the check runs on its own thread precisely so
        # that nothing waits for it, which means nothing can predict its frame.
        try:
            update = peer.wait_until(
                lambda drawn: f"{NEWER_VERSION} is out" in drawn,
                timeout=10,
                what="the update notice",
            )
            check(
                "a newer release is named on the inbox, with a way to take it",
                "u update" in update and f"{NEWER_VERSION} is out" in update,
                [line for line in update.split("\n") if "is out" in line][:1],
            )
        except DeadlineExceeded as error:
            check(
                "a newer release is named on the inbox, with a way to take it",
                False,
                str(error)[:400],
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

        # With only the unhooked agent running, every row is unreported -- the
        # state a default install lands in, and the one line that decides
        # whether anyone opens the inbox twice.
        peer.send(CTRL_O)
        time.sleep(3.0)
        screen = peer.snapshot()
        if machine_is_quiet:
            check(
                "an install with no hooks is told exactly what to run",
                "Run `p2pmux setup` to see which agents need you." in screen,
                screen[:600],
            )
        check(
            "nothing is said about machines until there are two",
            "asleep" not in screen and " ✓ " not in screen,
            screen[:600],
        )
        peer.send(CTRL_O)
        time.sleep(1.0)

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

        # Enter lands on the tab holding that agent's terminal.
        peer.send(b"\r")
        time.sleep(2.0)
        screen = peer.snapshot()
        check(
            "Enter opens that agent's terminal",
            "HOOKED-AGENT-BLOCKED" in screen,
            screen[:400],
        )
        check(
            # Whole, not zoomed over: Enter used to blow the agent's pane up to
            # fill the screen, which hid the panes beside it that the person had
            # arranged deliberately. `72552e7` made it land on the tab instead.
            "it lands on the agent's tab rather than zooming over it",
            screen.count("host: ") > 1,
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
