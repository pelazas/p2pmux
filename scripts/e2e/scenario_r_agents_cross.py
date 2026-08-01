#!/usr/bin/env python3
"""The agents overlay, across two machines.

The README's headline bullet is an overlay "tracking Claude Code, Codex, Cursor, Pi and
OpenCode across every machine in the session". Scenario K proves the notification path
with both peers on one Mac. Nothing checked that an agent running on *someone else's box*
appears in your overlay at all -- which is the whole claim.

It did not, in one direction. An agent on the coordinator reached every member, but an
agent on a member reached that member only: `broadcast_roster` writes to the peers and
the coordinator is not one of them, so the coordinator's own overlay was the single place
a member's agents never arrived. In a two-person session that is the host being unable to
see the guest's agents. This scenario runs the agent on the *member*, which is the
direction that was broken.

Requires the `p2pmux-lab` droplet (see remote.py).

Run: python3 scripts/e2e/scenario_r_agents_cross.py
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost, spawn_remote  # noqa: E402

CTRL_A = b"\x01"
CTRL_P = b"\x10"
AGENT_DIR = "/root/agentcwd"


def main() -> int:
    results: list[bool] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append(bool(ok))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" -- {detail}" if detail and not ok else ""))

    remote = RemoteHost()
    remote.reset_home()
    remote.reset_network()
    remote.run(f"mkdir -p {AGENT_DIR}")
    remote.run("printf 'echo AGENT-LIVE\\nsleep 900\\n' > /root/agent.sh")

    print(f"== scenario R: agents overlay across {remote.alias} ==", flush=True)
    try:
        with Harness("r-agents-cross") as harness:
            host, ticket = harness.create_room("mac")
            guest = spawn_remote(
                harness, remote, "droplet", ["join", ticket, "--name", "droplet"],
                cols=110, rows=34,
            )
            guest.wait_ready(timeout=60)
            guest.wait_for(r"Pane #\d+", timeout=60)
            check("the droplet joined the coordinator's session", True)

            guest.send(CTRL_P)
            time.sleep(0.5)
            guest.send(b"n")
            time.sleep(4.0)
            check("the droplet hosts a pane of its own", "Pane #2" in guest.snapshot(),
                  guest.snapshot()[:300])

            # Detection matches the process's own argv[0]. `exec -a` is a bash builtin and
            # the pane's shell here is dash, so the whole command goes to bash -- naming
            # bash as the target of exec is not enough, dash is the one parsing the `-a`.
            guest.send(
                f"bash -c 'cd {AGENT_DIR} && exec -a claude /bin/bash /root/agent.sh'\r".encode()
            )
            try:
                guest.wait_for("AGENT-LIVE", timeout=30)
                check("the agent is running in the droplet's pane", True)
            except Exception as error:
                check("the agent is running in the droplet's pane", False, str(error)[:200])

            # Detection samples the process tree on a timer; allow more than one tick.
            time.sleep(12)

            guest.send(CTRL_A)
            time.sleep(3.0)
            own = guest.settle(quiet_for=1.0, timeout=20)
            check("the machine hosting the agent lists it", "Claude Code" in own, own[:400])
            guest.send(b"\x1b")
            time.sleep(1.2)

            host.send(CTRL_A)
            time.sleep(3.0)
            overlay = host.settle(quiet_for=1.0, timeout=20)
            check("the coordinator's overlay opened", "Agents" in overlay, overlay[:200])
            check("an agent running on the other machine is listed",
                  "Claude Code" in overlay, overlay[:600])
            check("the overlay attributes it to the droplet", "droplet" in overlay, overlay[:600])
            check("the overlay shows a working directory from the other machine",
                  "agentcwd" in overlay, overlay[:600])

            host.send(b"\x1b")
            time.sleep(1.0)
            check("Esc closes the overlay",
                  "Agents" not in host.settle(quiet_for=0.8, timeout=15))
            check("both peers still alive", host.alive and guest.alive)
    finally:
        remote.reap()

    print(f"\n{sum(results)}/{len(results)} checks passed")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
