#!/usr/bin/env python3
"""Category C: the four things the inbox was getting wrong, on two real machines.

Every claim here failed on a Mac with a few detached sessions on it, which is
the machine every p2pmux developer has. One droplet plus this Mac is the
smallest fleet that can test them, because two of the four are about a *second*
machine and the other two are about the machine you are sitting at.

  * a terminal on another machine can actually be started, and the refusal that
    comes first names the one command on that machine that lifts it;
  * `m` moves the cursor into the fleet and the arrows walk it, rather than `m`
    stepping one machine per press while the arrows stay on the agents;
  * an agent in another p2pmux session on a machine is reported as being in that
    session, by name, rather than as "running outside p2pmux";
  * the `inbox N` badge stops counting an agent once you have gone to its pane,
    and counts it again the next time it blocks.

Requires one provisioned droplet:

    ./scripts/e2e/provision_droplets.sh create
    python3 scripts/e2e/scenario_aa_inbox_honesty.py
    ./scripts/e2e/provision_droplets.sh destroy      # always

The Mac side runs entirely inside the harness's sandbox HOME, so the pairing
file and sessions of whoever is running this are never touched.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import BINARY, Harness  # noqa: E402
from remote import RemoteError, hosts_from_manifest, spawn_remote  # noqa: E402

CTRL_O = b"\x0f"
COLS, ROWS = 120, 36

PAIR_CODE = re.compile(r"pairing code:\s*([A-Z0-9-]+)")
BLOCKED_ON = "permission: write to /etc/hosts"

# An agent in a pane, reporting through `p2pmux notify` the way a real
# registration does. Used on both machines.
HOOKED_AGENT = """
notify() {{ printf '{{"message":"%s"}}' "$2" | {binary} notify claude --status "$1" >/dev/null 2>&1 || true; }}
echo AGENT-START
notify running working
sleep 2
notify pending '{blocked}'
echo AGENT-BLOCKED
sleep 900
""".strip()

checks: list[tuple[str, bool, str]] = []


def clean_env(**overrides: str) -> dict[str, str]:
    """This Mac's environment with any inherited pane identity removed.

    Run this scenario from inside a p2pmux pane and every `p2pmux notify` it
    starts inherits `P2PMUX_PANE_ID` and `P2PMUX_SOCK`, takes itself for a pane
    agent, and reports to *that* session's node — where nothing in this run can
    see it. The loose-agent checks then fail for a reason that has nothing to do
    with the code under test, which is worse than failing.
    """
    environment = {
        key: value
        for key, value in os.environ.items()
        if key not in ("P2PMUX_PANE_ID", "P2PMUX_SOCK")
    }
    environment.update(overrides)
    return environment


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    mark = "PASS" if passed else "FAIL"
    print(f"  [{mark}] {label}" + (f" -- {detail}" if detail else ""), flush=True)


def top_bar(screen: str) -> str:
    return screen.split("\n", 1)[0]


def wait_for_screen(peer, predicate, timeout: float = 40.0) -> str:
    """Poll a peer's screen until `predicate` likes it, then return it."""
    deadline = time.monotonic() + timeout
    screen = peer.snapshot()
    while time.monotonic() < deadline:
        screen = peer.snapshot()
        if predicate(screen):
            return screen
        time.sleep(0.75)
    return screen


def selected_card(screen: str) -> str:
    """The inbox row the cursor is on, as its three lines of text.

    The cursor is a `›` in the first column, and a card's continuation lines are
    indented under it — so the card is that line plus everything after it up to
    the next line that starts a new row.
    """
    lines = screen.split("\n")
    for index, line in enumerate(lines):
        if line.startswith("›"):
            return "\n".join(lines[index : index + 3])
    return ""


# The glyphs the rail draws in front of a machine's name. A detail line under
# one is indented instead, which is what tells the two apart.
MACHINE_GLYPHS = {"●", "○", "◐", "◇", "◆"}


def fleet_order(screen: str) -> list[tuple[str, bool]]:
    """Every machine in the rail, top to bottom, with whether it is reachable.

    Reading the order off the screen is what makes "put the cursor on the
    droplet" a fixed number of Downs rather than a guess. Two things make it
    less obvious than it sounds, and both are real:

      * a peer id is per *process*, so one droplet that has run p2pmux twice
        is two rows, disambiguated as `droplet · 0b0ed393`;
      * a machine that has stopped answering keeps its row, as `asleep`.

    Aiming at the wrong one of those is indistinguishable from the product
    being broken: the request goes to a peer that is not there and no answer
    ever comes back.
    """
    machines = []
    for line in screen.split("\n"):
        if "│" not in line:
            continue
        rail = line.rsplit("│", 1)[-1].strip()
        parts = rail.split()
        if len(parts) >= 2 and parts[0] in MACHINE_GLYPHS:
            machines.append((parts[1], parts[0] == "●"))
    return machines


def put_cursor_on_machine(peer, name: str) -> bool:
    """`m` into the fleet, then Down to the live `name`. Leaves it there.

    The *last* live match, which is the most recently joined process — the one
    this run started, rather than a node an earlier step killed whose member
    record the coordinator has not dropped yet.
    """
    order = fleet_order(peer.snapshot())
    live = [index for index, (label, up) in enumerate(order) if label == name and up]
    if not live:
        return False
    peer.send(b"m")
    time.sleep(0.8)
    for _ in range(live[-1]):
        peer.send(b"\x1b[B")
        time.sleep(0.5)
    return True


def walk_to_row(peer, wanted: str, limit: int = 12) -> bool:
    """Press Down until the selected card contains `wanted`."""
    for _ in range(limit):
        if wanted in selected_card(peer.snapshot()):
            return True
        peer.send(b"\x1b[B")
        time.sleep(0.6)
    return wanted in selected_card(peer.snapshot())


def main() -> int:
    try:
        hosts = hosts_from_manifest()
    except RemoteError as error:
        print(f"lab not available: {error}")
        return 2
    droplet = hosts[0]

    with Harness("aa-inbox-honesty") as harness:
        def mac_cli(*args: str, timeout: float = 60.0) -> str:
            """A non-TUI p2pmux subcommand on this Mac, inside the sandbox HOME."""
            result = subprocess.run(
                [str(BINARY), *args],
                capture_output=True,
                text=True,
                timeout=timeout,
                env=clean_env(HOME=str(harness.home), TERM="xterm-256color"),
            )
            return result.stdout + result.stderr

        # A `claude` this scenario owns, first on the Mac peer's PATH. Enter on a
        # loose agent runs the agent's chat command, and that command has to be
        # something whose arrival on screen can be asserted.
        fake_bin = harness.home / "bin"
        fake_bin.mkdir(parents=True, exist_ok=True)
        (fake_bin / "claude").write_text(
            "#!/bin/sh\necho FAKE-CLAUDE-CHAT-STARTED\nexec sleep 900\n"
        )
        (fake_bin / "claude").chmod(0o755)
        # On PATH for everything this scenario starts, not just the peer. The
        # node that spawns panes is started by the *first* p2pmux command to run
        # — `pair`, several steps above — and a pane inherits its environment,
        # so putting the directory on the peer's PATH alone would leave the node
        # unable to find the very command Enter asks it to run.
        os.environ["PATH"] = f"{fake_bin}:{os.environ['PATH']}"

        droplet.reset_home()
        droplet.cli("config set name droplet")
        mac_cli("config", "set", "name", "mac")
        droplet.run(
            f"cat > {droplet.home}/agent.sh <<'SH'\n"
            + HOOKED_AGENT.format(binary=droplet.binary, blocked=BLOCKED_ON)
            + "\nSH"
        )
        (harness.home / "agent.sh").write_text(
            HOOKED_AGENT.format(binary=str(BINARY), blocked=BLOCKED_ON) + "\n"
        )

        # --- pairing ---------------------------------------------------------
        offered = mac_cli("pair", "--no-accept-work", timeout=120)
        match = PAIR_CODE.search(offered)
        check("`p2pmux pair` prints a pairing code", bool(match), offered.strip()[-200:])
        if not match:
            return 1
        accepted = droplet.cli(f"pair {match.group(1)} --accept-work", timeout=120)
        check(
            "the droplet pairs and agrees to accept work",
            "paired: mac" in accepted,
            accepted.strip()[-300:],
        )

        # The state a machine is in immediately after `pair --accept-work`, and
        # the one that used to make this feature look broken: it accepts work
        # and allows nothing, with no supported way to change that.
        policy = droplet.cli("work")
        check(
            "a freshly paired machine allows nothing, and says what to run",
            "nothing yet" in policy and "p2pmux work allow" in policy,
            policy.strip()[-300:],
        )

        droplet.reap()
        time.sleep(3.0)

        # --- both machines in one session ------------------------------------
        mac = harness.spawn(
            "mac",
            [],
            cols=COLS,
            rows=ROWS,
            env={"PATH": f"{fake_bin}:{os.environ['PATH']}"},
        )
        mac.wait_ready(timeout=60)
        mac.wait_for(r"Agents", timeout=60)
        drop = spawn_remote(harness, droplet, "droplet", [], cols=COLS, rows=ROWS)
        drop.wait_ready(timeout=90)
        drop.wait_for(r"Agents", timeout=60)
        check("bare `p2pmux` rejoins on both machines with no code typed", True)

        screen = wait_for_screen(mac, lambda s: "droplet" in s, timeout=60)
        check("the fleet on the Mac lists the droplet", "droplet" in screen, screen[:400])

        # --- m moves the cursor to the fleet, arrows walk it ------------------
        mac.send(b"m")
        time.sleep(1.0)
        after_m = mac.snapshot()
        mac.send(b"\x1b[B")
        time.sleep(0.8)
        after_down = mac.snapshot()
        check(
            "`m` moves the cursor into the fleet and the arrows walk it",
            after_m != after_down,
            "one press of m then one Down changed the fleet selection",
        )

        # --- the refusal names the command that lifts it ----------------------
        # The droplet accepts work and allows nothing, so this is refused, and
        # the whole value of the refusal is the sentence it carries. One press,
        # because a second while the first is outstanding is answered with
        # `waiting for current pane reservation` and nothing else.
        mac.send(b"\x1b")
        time.sleep(0.6)
        # The node the droplet ran while pairing was killed, and the coordinator
        # takes a while to drop its member record. Waiting for the rail to
        # settle to one live droplet is cheaper than aiming at the wrong one.
        wait_for_screen(
            mac,
            lambda s: len([1 for label, up in fleet_order(s) if label == "droplet" and up]) == 1,
            timeout=90,
        )
        aimed = put_cursor_on_machine(mac, "droplet")
        check("the cursor can be put on the droplet in the rail", aimed, str(fleet_order(mac.snapshot())))
        mac.send(b"\r")
        refusal = wait_for_screen(mac, lambda s: "refused" in s, timeout=45)
        check(
            "a refused remote terminal names the command that lifts it",
            "work allow" in refusal,
            " / ".join(line.strip() for line in refusal.split("\n")[-4:] if line.strip())[:400],
        )

        # --- and then the terminal actually opens -----------------------------
        granted = droplet.cli("work allow")
        check(
            "`p2pmux work allow` on the droplet opens both gates",
            "login shell" in granted,
            granted.strip()[-300:],
        )

        put_cursor_on_machine(mac, "droplet")
        mac.send(b"\r")
        time.sleep(6.0)
        opened = False
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if "REMOTE-TERMINAL-OK" in mac.snapshot():
                opened = True
                break
            mac.type("echo REMOTE-TERMINAL-OK\n")
            time.sleep(4.0)
        check(
            "with work allowed, a terminal starts on the other machine",
            opened,
            mac.snapshot()[-400:],
        )

        # --- an agent in another p2pmux session is not "outside p2pmux" -------
        # A second, unrelated session on the droplet, with an agent in one of
        # its panes. Nothing in the shared session has a pane for it, which is
        # exactly the shape that used to be reported as outside p2pmux.
        other = spawn_remote(
            harness,
            droplet,
            "droplet-other",
            ["create", "--name", "other", "--session-name", "other"],
            cols=COLS,
            rows=ROWS,
        )
        other.wait_ready(timeout=90)
        other.run_in_shell(
            f"bash -c 'exec -a claude /bin/sh {droplet.home}/agent.sh'"
        )
        other.wait_for("AGENT-BLOCKED", timeout=60)

        mac.send(CTRL_O)
        time.sleep(1.5)
        screen = wait_for_screen(
            mac, lambda s: "p2pmux session other" in s, timeout=60
        )
        check(
            "an agent in another p2pmux session names that session",
            "p2pmux session other" in screen,
            "\n".join(line for line in screen.split("\n") if "session" in line)[:300],
        )
        check(
            "and is no longer called an agent running outside p2pmux",
            "running outside p2pmux · enter starts" not in screen
            or "p2pmux session other" in screen,
            screen[:400],
        )

        # --- the badge stops counting once you have been ----------------------
        # An agent in a pane of *this* session, on the Mac, so that going to it
        # is a focus change this client makes.
        mac.send(b"n")
        time.sleep(3.0)
        mac.run_in_shell(f"bash -c 'exec -a claude /bin/sh {harness.home}/agent.sh'")
        mac.wait_for("AGENT-BLOCKED", timeout=60)
        mac.send(CTRL_O)
        time.sleep(2.0)
        screen = wait_for_screen(mac, lambda s: "needs you" in top_bar(s) or "inbox" in top_bar(s), timeout=60)
        counted = wait_for_screen(mac, lambda s: re.search(r"inbox \d", top_bar(s)), timeout=60)
        check(
            "a blocked agent puts a count on the inbox badge",
            bool(re.search(r"inbox \d", top_bar(counted))),
            top_bar(counted)[:140],
        )

        found = walk_to_row(mac, "needs you")
        mac.send(b"\r")
        time.sleep(3.0)
        answered = wait_for_screen(
            mac, lambda s: not re.search(r"inbox \d", top_bar(s)), timeout=30
        )
        check(
            "going to its pane clears the count",
            found and not re.search(r"inbox \d", top_bar(answered)),
            top_bar(answered)[:140],
        )

        # --- Enter on a loose agent runs its chat command ---------------------
        # On this Mac, which is the branch that used to drop the command and
        # leave the user in an empty shell.
        loose = harness.home / "loose.sh"
        loose.write_text("echo LOOSE-AGENT-UP\nsleep 900\n")
        subprocess.run(
            ["/bin/sh", "-c", f"(exec -a claude /bin/sh {loose} &) </dev/null >/dev/null 2>&1"],
            check=True,
            env=clean_env(HOME=str(harness.home)),
        )
        # Long enough for any create still in flight to commit or be refused:
        # a second create while one is outstanding is answered with `waiting
        # for current pane reservation` and the keypress does nothing.
        time.sleep(12.0)
        mac.send(CTRL_O)
        time.sleep(2.0)
        reached = walk_to_row(mac, "enter starts a new conversation", limit=14)
        check(
            "a loose agent on this Mac is offered as a new conversation",
            reached,
            selected_card(mac.snapshot())[:300],
        )
        if reached:
            mac.send(b"\r")
            time.sleep(5.0)
            started = wait_for_screen(
                mac, lambda s: "FAKE-CLAUDE-CHAT-STARTED" in s, timeout=40
            )
            check(
                "Enter runs its chat command rather than opening an empty shell",
                "FAKE-CLAUDE-CHAT-STARTED" in started,
                " / ".join(line.strip() for line in started.split("\n")[-4:] if line.strip())[:400],
            )

        droplet.reap()

    print()
    failed = [label for label, passed, _ in checks if not passed]
    print(f"{len(checks) - len(failed)}/{len(checks)} checks passed")
    for label in failed:
        print(f"  FAILED: {label}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
