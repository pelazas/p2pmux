#!/usr/bin/env python3
"""Category C: the inbox and pairing across two real machines, neither of them this one.

Scenario W proves the inbox on one machine. That is the on-ramp, never the
pitch: the defensible claim starts at more than one machine, and this is the
scenario that tests the claim.

Both peers are droplets in different regions, so the whole session -- both
nodes, both PTYs, the agent, and the terminal you drop into -- lives on the
public internet, with this Mac only holding the ssh reins. It walks the
multi-machine half of the definition of done:

  * `p2pmux pair` on one machine prints a code and `p2pmux pair <code>` on the
    other succeeds;
  * after pairing, bare `p2pmux` rejoins on either with no code typed;
  * `p2pmux machines` lists the paired machines and whether each is reachable,
    including `asleep` for one that has stopped answering;
  * the inbox lists an agent running on the *other* machine, with that machine
    in its own column and a `needs you` its hook reported;
  * Enter reaches that remote terminal and keystrokes arrive.

Requires the two-droplet lab:

    ./scripts/e2e/provision_droplets.sh create
    python3 scripts/e2e/scenario_x_inbox_cross.py
    ./scripts/e2e/provision_droplets.sh destroy      # always
"""

from __future__ import annotations

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteError, hosts_from_manifest, spawn_remote  # noqa: E402

CTRL_O = b"\x0f"
CTRL_P = b"\x10"
COLS, ROWS = 110, 34

PAIR_CODE = re.compile(r"pairing code:\s*([A-Z0-9-]+)")
BLOCKED_ON = "permission: write to /etc/hosts"

# Reports through `p2pmux notify`, the same path a real Claude Code registration
# uses. The `pending` push carries a message because the product refuses one
# that does not: a row that says `needs you` and cannot say what for is noise.
HOOKED_AGENT = r"""
notify() { printf '{"message":"%s"}' "$2" | /usr/local/bin/p2pmux notify claude --status "$1" >/dev/null 2>&1 || true; }
echo REMOTE-AGENT-START
notify running working
sleep 2
notify pending 'BLOCKED_ON'
echo REMOTE-AGENT-BLOCKED
sleep 900
""".replace("BLOCKED_ON", BLOCKED_ON).strip()

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    print(f"  [{'PASS' if passed else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""), flush=True)


def main() -> int:
    try:
        hosts = hosts_from_manifest()
    except RemoteError as error:
        print(f"lab not available: {error}")
        print("run ./scripts/e2e/provision_droplets.sh create first")
        return 2
    if len(hosts) < 2:
        print("scenario X needs two droplets")
        return 2
    alpha, beta = hosts[0], hosts[1]

    try:
        for host in (alpha, beta):
            host.reset_home()
        # Names, so every assertion below reads as a machine rather than as a
        # truncated droplet hostname.
        alpha.cli("config set name alpha")
        beta.cli("config set name beta")
        # The agent runs on beta, so the script has to live on beta. Alpha is the
        # machine that only ever watches it.
        beta.run(f"cat > {beta.home}/agent.sh <<'SH'\n{HOOKED_AGENT}\nSH")

        # --- pairing -------------------------------------------------------
        #
        # Alpha offers, so alpha holds the room. That matters for the last
        # check: only the coordinator drops a peer from the member list, so if
        # the machine this scenario switches off were the coordinator, there
        # would be nobody left to notice it had gone and `asleep` could not
        # appear for five minutes.
        offered = alpha.cli("pair --no-accept-work", timeout=90)
        match = PAIR_CODE.search(offered)
        check("`p2pmux pair` prints a pairing code", bool(match), offered.strip()[-200:])
        if not match:
            return 1
        code = match.group(1)

        accepted = beta.cli(f"pair {code} --accept-work", timeout=120)
        check(
            "`p2pmux pair <code>` pairs with the machine that printed it",
            "paired: alpha" in accepted,
            accepted.strip()[-300:],
        )

        listed = alpha.cli("machines")
        check(
            "`p2pmux machines` lists the paired machine as reachable",
            "beta" in listed and "ready" in listed,
            listed.strip()[-300:],
        )
        check(
            "and marks the machine you are sitting at",
            "(this machine)" in listed and "alpha" in listed,
            listed.strip()[-300:],
        )

        # Everything beta started while pairing goes away, so the next command
        # has nothing live to attach to. Without this, bare `p2pmux` would be
        # attaching the session pairing had just left running, and the check
        # below would prove attaching rather than rejoining.
        beta.reap()
        time.sleep(3.0)
        check(
            "nothing is left running on the paired machine",
            beta.run("pgrep -f /usr/local/bin/p2pmux || true", check=False).strip() == "",
            "",
        )

        with Harness("x-inbox-cross") as harness:
            # --- bare `p2pmux`, with no code typed and nothing live --------
            b = spawn_remote(harness, beta, "beta", [], cols=COLS, rows=ROWS)
            b.wait_ready(timeout=60)
            b.wait_for(r"Agents", timeout=60)
            check(
                "after pairing, bare `p2pmux` rejoins with no code typed",
                True,
            )

            # An agent, in a terminal on beta.
            b.send(b"n")
            time.sleep(2.5)
            b.run_in_shell(f"/bin/sh -c 'exec -a claude /bin/sh {beta.home}/agent.sh'")
            b.wait_for("REMOTE-AGENT-BLOCKED", timeout=45)

            a = spawn_remote(harness, alpha, "alpha", [], cols=COLS, rows=ROWS)
            a.wait_ready(timeout=45)
            a.wait_for(r"Agents", timeout=40)
            check("bare `p2pmux` opens the inbox on the machine holding the room", True)

            # --- the inbox spans machines ----------------------------------
            deadline = time.monotonic() + 40
            screen = ""
            while time.monotonic() < deadline:
                screen = a.snapshot()
                if "needs you" in screen and "beta" in screen:
                    break
                time.sleep(1.0)
            check(
                "the inbox lists an agent running on the other machine",
                "beta" in screen,
                screen[:600],
            )
            check(
                "with the `needs you` its hook reported, in its own words",
                "needs you" in screen and "permission" in screen,
                screen[:600],
            )
            check(
                "and the badge carries the count",
                "inbox 1" in screen,
                screen.split("\n")[0][:140],
            )

            # --- Enter reaches the remote terminal --------------------------
            a.send(b"\r")
            time.sleep(3.0)
            check(
                "Enter opens the remote agent's terminal",
                "REMOTE-AGENT-BLOCKED" in a.snapshot(),
                a.snapshot()[:400],
            )
            a.type("echo CROSS-MACHINE-OK\n")
            reached = False
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if "CROSS-MACHINE-OK" in a.snapshot():
                    reached = True
                    break
                time.sleep(0.5)
            check("keystrokes reach a program on the other machine", reached, a.snapshot()[:400])

            a.send(CTRL_O)
            time.sleep(2.0)
            check(
                "Ctrl+O returns to the inbox from inside the remote terminal",
                "needs you" in a.snapshot(),
                a.snapshot()[:400],
            )

            # --- a machine that stops answering reads asleep ---------------
            beta.reap()
            time.sleep(15.0)
            listed = alpha.cli("machines")
            check(
                "a paired machine that stopped answering reads asleep",
                "beta" in listed and "asleep" in listed,
                listed.strip()[-300:],
            )
    finally:
        for host in (alpha, beta):
            try:
                host.reap()
            except Exception as error:  # noqa: BLE001 - teardown must not mask a failure
                print(f"  teardown: could not reap {host.target}: {error}")

    failures = [label for label, ok, _ in checks if not ok]
    print(f"{len(checks) - len(failures)}/{len(checks)} checks passed")
    print("scenario X:", "PASS" if not failures else f"FAIL ({len(failures)})")
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
