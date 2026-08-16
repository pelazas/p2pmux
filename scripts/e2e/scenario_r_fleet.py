"""Scenario R -- the machine features, against a real machine running a real bot.

Every other scenario proves the protocol. This one proves the four claims the
machine issues actually make, and it cannot be run on localhost: three of them
are about a *second* machine, and the fourth is about an agent nobody started
inside p2pmux.

The droplet is the developer's own bot VM. It runs Hermes as a systemd-style
daemon under `python -m hermes_cli.main gateway run`, which is the shape that
matters: not a coding agent someone typed into a pane, but a process that was
already there.

What each check is really asking:

  ownership     does the droplet arrive as a machine you own rather than as a
                person who joined -- the line #71 exists to draw;
  remote pane   can this Mac open a terminal whose shell is on the droplet, and
                does `hostname` in it print the droplet's name rather than this
                Mac's -- the whole of #72;
  detection     does a bot running outside p2pmux appear in the inbox at all,
                which is #76 and the reason #75 has anything to open;
  fleet         does a session created fresh on this Mac -- one the droplet was
                never paired into -- end up with the droplet in it, on its own,
                with nobody typing a code. That is #73's actual promise; the
                daemon is only how it is kept true across reboots.

Run:  python3 scripts/e2e/scenario_r_fleet.py

It needs `mybotvm` reachable over ssh and a release binary built there. The
droplet's p2pmux runs inside a sandbox HOME, so this never touches the pairing
record or sessions the developer actually uses.
"""

from __future__ import annotations

import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost  # noqa: E402


def section(title: str) -> None:
    print(f"\n== {title}")


def step(message: str) -> None:
    print(f"   {message}")

REMOTE_BINARY = "/home/pelazas/p2pmux-src/target/release/p2pmux"
REMOTE_HOME = "/home/pelazas/p2pmux-e2e-home"
LOCAL_BINARY = Path(__file__).resolve().parents[2] / "target" / "release" / "p2pmux"

# What the droplet's owner allows other machines of theirs to start on it.
# Written here because that is exactly how an owner sets it: a file on the
# machine it governs, which nothing on the wire can change.
DROPLET_POLICY = """accepts_work = true

[work]
allow = ["shell", "hermes chat"]
confirm = false
"""


def local(home: Path, args: list[str], timeout: float = 60.0) -> str:
    """A p2pmux CLI command on this Mac, inside the scenario's sandbox HOME."""
    result = subprocess.run(
        [str(LOCAL_BINARY), *args],
        capture_output=True,
        text=True,
        timeout=timeout,
        env={
            "HOME": str(home),
            "TERM": "xterm-256color",
            "PATH": "/usr/bin:/bin",
            "P2PMUX_DEBUG_UI": "/tmp/p2pmux-fleet-mac.log",
        },
    )
    if result.returncode != 0:
        raise AssertionError(
            f"`p2pmux {' '.join(args)}` failed: {result.stderr.strip() or result.stdout.strip()}"
        )
    return result.stdout


def main() -> int:
    remote = RemoteHost(
        alias="mybotvm",
        home=REMOTE_HOME,
        binary_path=REMOTE_BINARY,
        env={"P2PMUX_DEBUG_UI": "/tmp/p2pmux-fleet-droplet.log"},
    )
    section("preflight")
    version = remote.run(f"{REMOTE_BINARY} --version").strip()
    step(f"droplet binary: {version}")
    if not LOCAL_BINARY.exists():
        raise AssertionError(f"build the Mac binary first: {LOCAL_BINARY}")
    hermes = remote.run(
        "ps -eo args | grep 'hermes_cli.main' | grep -v grep | head -1", check=False
    ).strip()
    if not hermes:
        raise AssertionError("no Hermes gateway running on the droplet; nothing to detect")
    step(f"hermes on the droplet: {hermes[:70]}")

    remote.reset_home()
    remote.run(f"mkdir -p {REMOTE_HOME}/.config/p2pmux")
    remote.run(
        f"cat > {REMOTE_HOME}/.config/p2pmux/pairing.toml <<'EOF'\n{DROPLET_POLICY}EOF"
    )
    step("droplet policy: accepts work, allows a shell and `hermes chat`")

    with Harness("scenario-r-fleet") as harness:
        section("pairing")
        offer = local(harness.home, ["pair", "--no-accept-work"], timeout=120.0)
        code = next(
            (
                line.split(":", 1)[1].strip()
                for line in offer.splitlines()
                if line.startswith("pairing code:")
            ),
            None,
        )
        if not code:
            raise AssertionError(f"no pairing code in:\n{offer}")
        step(f"this Mac offered code {code}")

        joined = remote.cli(f"pair {code} --accept-work", timeout=180.0)
        step(f"droplet joined: {joined.strip().splitlines()[-1] if joined.strip() else '(quiet)'}")

        section("ownership (#71)")
        deadline = time.monotonic() + 60
        machines = ""
        while time.monotonic() < deadline:
            machines = local(harness.home, ["machines"])
            if "mybotvm" in machines:
                break
            time.sleep(2)
        step(machines.strip().replace("\n", "\n      "))
        assert "mybotvm" in machines, "the droplet never reached this Mac's fleet"
        fleet, _, guests = machines.partition("IN THIS SESSION, NOT YOURS")
        assert "mybotvm" in fleet, (
            "the droplet must be listed as a machine you own, not as somebody who joined"
        )
        assert "mybotvm" not in guests, "the droplet is in the guest list"
        step("the droplet is fleet, not a guest")

        section("fleet follows you (#73)")
        before = remote.cli("ls", timeout=60.0)
        # Under a PTY, because `create` opens the real UI and measures the
        # terminal it is going to draw into. A pipe has no size to ask for.
        followme, _ = harness.create_room("followme")
        step("created a session on this Mac the droplet was never paired into")
        deadline = time.monotonic() + 120
        after = before
        while time.monotonic() < deadline:
            after = remote.cli("ls", timeout=60.0)
            if after.count("\n") > before.count("\n"):
                break
            time.sleep(3)
        step(f"droplet sessions before:\n      {before.strip()}")
        step(f"droplet sessions after:\n      {after.strip()}")
        assert after.count("\n") > before.count("\n"), (
            "the droplet did not follow this Mac into the new session"
        )
        step("the droplet joined it on its own, with no code typed there")

        # That peer has done its job, and a session with a client on it is one
        # bare `p2pmux` refuses to attach to.
        followme.close()
        time.sleep(1.0)

        section("the inbox, on the machine you are sitting at")
        # Bare `p2pmux` rejoins the paired session and opens the inbox, which
        # is the screen all three remaining checks are about.
        inbox = harness.spawn("inbox", [])
        inbox.wait_for(r"MACHINES", timeout=60.0)
        screen = inbox.wait_for(r"mybotvm", timeout=60.0)
        step("the droplet is on the inbox's machine list")

        section("a bot nobody started in p2pmux (#76)")
        screen = inbox.wait_for(r"(?i)hermes", timeout=90.0)
        step("the Hermes gateway on the droplet reached this Mac's inbox")
        assert "hermes" in screen.lower(), screen

        section("a terminal on the droplet (#72)")
        # `m` moves the cursor into the fleet and the arrows walk it -- pressing
        # it twice moves back out, which is how this check used to end up with
        # the cursor on an agent row and Enter trying to start `claude` here.
        inbox.send("m")
        time.sleep(0.5)
        inbox.key("down")
        time.sleep(0.5)
        inbox.key("enter")
        inbox.wait_for(r"host: mybotvm|mybotvm", timeout=90.0)
        step("a pane opened, hosted by the droplet")
        inbox.run_in_shell("hostname")
        screen = inbox.wait_for(r"mybotvm", timeout=60.0)
        step("`hostname` in it prints the droplet's name, not this Mac's")

        section("a chat with the bot (#75)")
        # Back to the inbox. The Hermes row says which of the three things
        # enter will do, on the row, before anybody presses it — which is the
        # whole point of the distinction and makes this readable without
        # driving a cursor around.
        inbox.send(b"\x0f")
        inbox.wait_for(r"MACHINES", timeout=30.0)
        screen = inbox.wait_for(r"enter starts a new c", timeout=60.0)
        step("the Hermes row says enter starts a new conversation, not joins one")
        assert "enter joins its" not in screen, (
            "Hermes cannot join its gateway's conversation, and the inbox must "
            f"not imply otherwise:\n{screen}"
        )

    print("\nscenario R: every check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
