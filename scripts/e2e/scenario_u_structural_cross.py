#!/usr/bin/env python3
"""Renames, tab lifecycle and `p2pmux kill`, across machines.

The last shared-layout operations that only ever ran with both peers on one Mac. Renames
are documented as applying "for every admitted member", which is a claim about the *other*
machine and so cannot be checked on one. `kill` matters for the opposite reason: a member
shutting itself down is the ordinary way a session shrinks, and the coordinator has to
survive it.

Both directions are exercised on purpose — the Mac renames a pane, the droplet renames a
tab — because the agents overlay turned out to work coordinator-to-member and not the
other way round, and nothing about the layout path guarantees it is symmetric either.

Requires the `p2pmux-lab` droplet (see remote.py).

Run: python3 scripts/e2e/scenario_u_structural_cross.py
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost, spawn_remote  # noqa: E402

CTRL_P, CTRL_T = b"\x10", b"\x14"


def main() -> int:
    results: list[bool] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append(bool(ok))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" -- {detail}" if detail and not ok else ""))

    remote = RemoteHost()
    remote.reset_home()
    remote.reset_network()
    print(f"== scenario U: renames, tabs and kill across {remote.alias} ==", flush=True)

    try:
        with Harness("u-structural-cross") as harness:
            host, ticket = harness.create_room("mac", cols=120, rows=36)
            guest = spawn_remote(
                harness, remote, "droplet", ["join", ticket, "--name", "droplet"],
                cols=120, rows=36,
            )
            guest.wait_ready(timeout=60)
            guest.wait_for(r"Pane #\d+", timeout=60)

            # Pane rename on the Mac, seen on the droplet.
            host.send(CTRL_P)
            time.sleep(0.5)
            host.send(b"e")
            time.sleep(1.0)
            host.send(b"BUILD-BOX\r")
            time.sleep(3.0)
            check("a pane rename reaches the other machine",
                  "BUILD-BOX" in guest.snapshot(), guest.snapshot()[:300])

            # Tab rename on the droplet, seen on the Mac -- the other direction.
            guest.send(CTRL_T)
            time.sleep(0.5)
            guest.send(b"e")
            time.sleep(1.0)
            guest.send(b"SHIPPING\r")
            time.sleep(3.0)
            check("a tab rename reaches the other machine",
                  "SHIPPING" in host.snapshot(), host.snapshot()[:200])

            guest.send(CTRL_T)
            time.sleep(0.5)
            guest.send(b"n")
            time.sleep(4.0)
            tab_bar = host.snapshot().splitlines()[0]
            check("a tab created on the other machine appears in the Mac's tab bar",
                  len(re.findall(r"Tab #\d+|SHIPPING", tab_bar)) >= 2, tab_bar)

            guest.send(CTRL_T)
            time.sleep(0.5)
            guest.send(b"x")
            time.sleep(4.0)
            tab_bar = host.snapshot().splitlines()[0]
            check("deleting that tab is reflected on the Mac",
                  len(re.findall(r"Tab #\d+|SHIPPING", tab_bar)) == 1, tab_bar)

            # A blank title is documented to restore `Pane #N`.
            host.send(CTRL_P)
            time.sleep(0.5)
            host.send(b"e")
            time.sleep(1.0)
            for _ in range(20):
                host.send(b"\x7f")
            time.sleep(0.5)
            host.send(b"\r")
            time.sleep(3.0)
            check("a blank rename restores Pane #N for both peers",
                  "BUILD-BOX" not in host.snapshot() and "BUILD-BOX" not in guest.snapshot(),
                  host.snapshot()[:250])

            # The member shuts its own session down from its own machine.
            listing = remote.cli("ls").strip()
            name = next((line.split()[0] for line in listing.splitlines()[1:] if line.strip()), None)
            remote.cli(f"kill {name} --yes")
            time.sleep(8)
            check("killing the member's session ends its client", not guest.alive,
                  f"exit={guest.exit_code}")
            check("the member's record is gone from its machine",
                  not [l for l in remote.cli("ls").strip().splitlines()[1:] if l.strip()],
                  remote.cli("ls"))
            check("the coordinator survives a member leaving", host.alive)
            surviving = host.settle(quiet_for=1.0, timeout=20)
            check("the coordinator has no panic after the departure",
                  "panicked" not in host.raw_text(), surviving[-200:])
    finally:
        remote.reap()

    print(f"\n{sum(results)}/{len(results)} checks passed")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
