#!/usr/bin/env python3
"""Detach and re-attach a member, across machines.

USAGE.md: "Ctrl+Q asks which leaving you meant: `d` detaches that client without stopping
shells, Iroh, or hosted panes."
That is a README-level promise with unit coverage (tests/detach_resume_node.rs) and no
cross-machine test at all. The interesting part is what the *other* machine sees: a
detached member still hosts its panes, so the coordinator must keep rendering them and
keep being able to drive them while nobody is watching from that end.

Requires the `p2pmux-lab` droplet (see remote.py).

Run: python3 scripts/e2e/scenario_s_detach_cross.py
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost, spawn_remote  # noqa: E402

CTRL_P, CTRL_Q = b"\x10", b"\x11"


def main() -> int:
    results: list[bool] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append(bool(ok))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" -- {detail}" if detail and not ok else ""))

    def wait_free(peer, timeout: float = 45.0) -> bool:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if "control: free" in peer.snapshot():
                return True
            time.sleep(0.5)
        return False

    remote = RemoteHost()
    remote.reset_home()
    remote.reset_network()
    print(f"== scenario S: detach/re-attach across {remote.alias} ==", flush=True)

    try:
        with Harness("s-detach-cross") as harness:
            host, ticket = harness.create_room("mac")
            guest = spawn_remote(
                harness, remote, "droplet", ["join", ticket, "--name", "droplet"],
                cols=110, rows=34,
            )
            guest.wait_ready(timeout=60)
            guest.wait_for(r"Pane #\d+", timeout=60)

            # The droplet hosts a pane and marks it, so the coordinator has something of
            # the droplet's to keep rendering after the droplet's client leaves.
            guest.send(CTRL_P)
            time.sleep(0.5)
            guest.send(b"n")
            time.sleep(4.0)
            guest.send(b"echo DROPLET-PANE-ALIVE\r")
            host.wait_for("DROPLET-PANE-ALIVE", timeout=25)
            check("the droplet's pane and its output reached the coordinator", True)

            # Ctrl+Q asks which leaving was meant; `d` is detach.
            guest.send(CTRL_Q)
            time.sleep(0.5)
            guest.send(b"d")
            time.sleep(6.0)
            check("the droplet's client exited on Ctrl+Q then d", not guest.alive,
                  f"exit={guest.exit_code}")

            listing = remote.cli("ls").strip()
            rows = [line for line in listing.splitlines()[1:] if line.strip()]
            check("the detached session is still listed on the droplet", len(rows) == 1, listing)

            time.sleep(3)
            after = host.settle(quiet_for=1.0, timeout=20)
            check("the coordinator still renders the detached member's pane",
                  "DROPLET-PANE-ALIVE" in after, after[-400:])
            check("the coordinator did not lose the pane header",
                  len(re.findall(r"Pane #\d+", after)) >= 2, after[:400])

            # The promise is not just that the pane is drawn but that it still works.
            check("that pane frees up so the coordinator can claim it", wait_free(host))
            host.send(b"\x1b[C")  # focus right, onto the droplet's pane
            time.sleep(1.0)
            host.send(b"echo DRIVEN-WHILE-DETACHED\r")
            try:
                host.wait_for("DRIVEN-WHILE-DETACHED", timeout=25)
                check("the coordinator can still run a command in the detached member's pane", True)
            except Exception as error:
                check("the coordinator can still run a command in the detached member's pane",
                      False, str(error)[:200])

            name = rows[0].split()[0] if rows else None
            back = spawn_remote(
                harness, remote, "droplet2", ["attach", name], cols=110, rows=34,
            )
            back.wait_ready(timeout=60)
            try:
                back.wait_for(r"Pane #\d+", timeout=45)
                check("the droplet re-attached to its own live session", True)
            except Exception as error:
                check("the droplet re-attached to its own live session", False, str(error)[:200])

            reattached = back.settle(quiet_for=1.0, timeout=20)
            check("the re-attached client sees the work done while it was away",
                  "DRIVEN-WHILE-DETACHED" in reattached, reattached[-500:])
            check("the coordinator is still alive", host.alive)
    finally:
        remote.reap()

    print(f"\n{sum(results)}/{len(results)} checks passed")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
