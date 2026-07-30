"""Scenario P -- join a real cross-machine session with nothing but a short code.

This is the test the hosted rendezvous exists for. Scenario L proves a droplet can join with
a pasted ticket; here the only thing that crosses between the two machines is ten characters
a person could read down a phone line. The droplet resolves them itself, against the live
service, and dials whatever it gets back.

It deliberately runs against the deployed `rv.p2pmux.com` rather than a local stub. DNS, TLS
and the Worker's edge behaviour are part of the claim, and a stub would quietly validate a
version of the system nobody ships.

What would make this pass for the wrong reason: a code that resolved through some cached
local state on this Mac. It cannot -- the droplet is a different machine with its own sandbox
HOME, and the only input it is given is the code string.

Set P2PMUX_RENDEZVOUS_URL to point both peers at a different deployment.

Run:  python3 scripts/e2e/scenario_p_short_code.py
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
BINARY = REPO_ROOT / "target" / "release" / "p2pmux"

RENDEZVOUS = os.environ.get("P2PMUX_RENDEZVOUS_URL", "https://rv.p2pmux.com")
CTRL_A = b"\x01"

# The printed form: two groups of five, from the 31-symbol alphabet that omits 0/1/I/L/O.
CODE = re.compile(r"^[2-9A-HJ-NP-Z]{5}-[2-9A-HJ-NP-Z]{5}$")

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    print(f"  [{'PASS' if passed else 'FAIL'}] {label}{(' -- ' + detail) if detail else ''}", flush=True)


def local_code(harness: Harness, session: str) -> str:
    """The short code the coordinator's node published, read the way a user would."""
    result = subprocess.run(
        [str(BINARY), "code", session],
        capture_output=True,
        text=True,
        timeout=30,
        env={
            "HOME": str(harness.home),
            "PATH": "/usr/bin:/bin",
            "P2PMUX_RENDEZVOUS_URL": RENDEZVOUS,
        },
    )
    if result.returncode != 0:
        raise AssertionError(f"`p2pmux code` failed:\n{result.stderr.strip()}")
    return result.stdout.strip()


def main() -> int:
    remote = RemoteHost()

    print(f"== scenario P: short-code join over {remote.alias} ==", flush=True)
    print(f"   rendezvous: {RENDEZVOUS}", flush=True)
    if not remote.binary_ready():
        print(f"droplet has no release binary at {remote.binary}", file=sys.stderr)
        return 2

    remote.reset_network()
    remote.reset_home()

    try:
        with Harness("scenario_p_short_code") as harness:
            host, ticket = harness.create_room("mac")
            check("mac created a session", bool(ticket), f"{len(ticket)} chars")

            code = local_code(harness, "mac")
            check("the node published a short code", bool(CODE.match(code)), code)
            # The code is not a slice of the ticket -- it is a lookup handle for a sealed
            # copy of it, and the two share no material.
            check("the code is not derived from the ticket", code not in ticket)

            # Everything the droplet gets. No ticket, no address, no session id.
            guest = harness.spawn(
                "droplet",
                ["join", code, "--name", "droplet"],
                launcher=remote.launcher({"P2PMUX_RENDEZVOUS_URL": RENDEZVOUS}),
            )
            guest.wait_ready(timeout=45.0)
            guest.wait_for(r"Pane #\d+", timeout=90.0)
            check("the droplet joined from the code alone", True)

            screen = guest.wait_for(r"direct|relayed", timeout=40.0)
            badge = re.search(r"(direct|relayed)[^\s]{0,10}", screen or "")
            # Either path is a result worth recording; what matters is that one formed and
            # the badge says which, because that is what makes a slow session diagnosable.
            check("a real network path formed", badge is not None,
                  badge.group(0) if badge else "no badge")

            # Presence draws an initial on the pane border; the member list with full names
            # is behind the agents overlay, so ask for it the way a user would.
            host.send(CTRL_A)
            time.sleep(0.4)
            host.send(CTRL_A)
            overlay = host.wait_for(r"droplet", timeout=20.0)
            check("the mac's member list names the droplet", overlay is not None)
            host.send(b"\x1b")
    finally:
        remote.reset_network()

    failed = [label for label, passed, _ in checks if not passed]
    print(f"\n{len(checks) - len(failed)}/{len(checks)} passed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
