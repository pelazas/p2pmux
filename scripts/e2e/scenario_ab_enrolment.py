#!/usr/bin/env python3
"""Category C: enrolling a machine you own, with nobody sitting at it.

`p2pmux pair` is a code one human types on one machine within ten minutes.
This is the other way in, the one a provisioning script can take:

  * `p2pmux enroll` on a machine already in the fleet prints a standing token;
  * `p2pmux enroll <token> --name build-box --accept-work` on a fresh droplet
    joins the fleet unattended, exactly as cloud-init would run it;
  * the machine is written into the fleet *record*, so it is still listed after
    the session that introduced them is gone — which is the whole difference
    between a fleet and a member list;
  * and after `p2pmux enroll --revoke` the same token enrols nothing.

The last one is the one worth having: a credential you cannot withdraw is not
a credential, it is a permanent hole.

Requires one provisioned droplet:

    ./scripts/e2e/provision_droplets.sh create
    python3 scripts/e2e/scenario_ab_enrolment.py
    ./scripts/e2e/provision_droplets.sh destroy      # always
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
from remote import RemoteError, hosts_from_manifest  # noqa: E402

TOKEN = re.compile(r"(p2pmux-enrol-v1:[A-Za-z0-9_-]+)")

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    print(f"  [{'PASS' if passed else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""), flush=True)


def clean_env(**overrides: str) -> dict[str, str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if key not in ("P2PMUX_PANE_ID", "P2PMUX_SOCK")
    }
    environment.update(overrides)
    return environment


def main() -> int:
    try:
        droplet = hosts_from_manifest()[0]
    except (RemoteError, IndexError) as error:
        print(f"lab not available: {error}")
        return 2

    with Harness("ab-enrolment") as harness:
        def mac(*args: str, timeout: float = 120.0) -> str:
            result = subprocess.run(
                [str(BINARY), *args],
                capture_output=True,
                text=True,
                timeout=timeout,
                env=clean_env(HOME=str(harness.home), TERM="xterm-256color"),
            )
            return result.stdout + result.stderr

        droplet.reset_home()
        mac("config", "set", "name", "mac")

        # A fleet has to exist before it can invite anything. `pair` with no code
        # is what mints the home session; nobody types the code it prints.
        offered = mac("pair", "--no-accept-work")
        check("`p2pmux pair` mints a fleet to enrol into", "pairing code" in offered, offered.strip()[-160:])

        printed = mac("enroll")
        match = TOKEN.search(printed)
        check("`p2pmux enroll` prints a token", bool(match), printed.strip()[-200:])
        if not match:
            return 1
        token = match.group(1)
        check(
            "and says plainly what holding it buys",
            "revoke" in printed and "work allow" in printed,
            "",
        )
        check(
            "with the cloud-init line to paste it into",
            "runcmd:" in printed,
            "",
        )
        check(
            "printing it twice gives the same token",
            TOKEN.search(mac("enroll")).group(1) == token,
            "a token that rotated on being looked at would strand every image built from the last look",
        )

        # Exactly what cloud-init would run, with nobody at the keyboard.
        enrolled = droplet.cli(
            f"enroll {token} --name build-box --accept-work", timeout=180
        )
        check(
            "the droplet enrols unattended",
            "enrolled as build-box" in enrolled,
            enrolled.strip()[-300:],
        )
        check(
            "and opens the work gate it was told to",
            "login shell" in enrolled,
            enrolled.strip()[-200:],
        )

        listed = ""
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            listed = mac("machines")
            if "build-box" in listed:
                break
            time.sleep(3.0)
        check("the fleet on the Mac lists it", "build-box" in listed, listed.strip()[-300:])

        # The point of a fleet record: it outlives the session that introduced
        # them. A row that only exists while both are in one session is a member
        # list, which is the bug this scenario was written after.
        for name in re.findall(r"^(\S+)\s", mac("ls"), re.MULTILINE):
            if name not in ("NAME",):
                mac("kill", name, "--yes")
        droplet.reap()
        time.sleep(4.0)
        after = mac("machines")
        check(
            "and still lists it once every session is gone",
            "build-box" in after,
            after.strip()[-300:],
        )

        # --- revocation -------------------------------------------------------
        revoked = mac("enroll", "--revoke")
        check("`--revoke` withdraws the invitation", "revoked" in revoked, revoked.strip()[-160:])
        mac("unpair", "build-box")
        check(
            "and unpairing takes the machine out of the fleet",
            "build-box" not in mac("machines"),
            "",
        )

        # A machine that presents the withdrawn token now enrols into nothing.
        # It may still *join* — the ticket is a session invitation and revoking
        # an enrolment is not revoking that — but it must not be written in.
        droplet.reset_home()
        droplet.cli(f"enroll {token} --name build-box --accept-work", timeout=180)
        time.sleep(8.0)
        after_revoke = mac("machines")
        check(
            "the withdrawn token enrols nothing",
            "build-box" not in after_revoke,
            after_revoke.strip()[-300:],
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
