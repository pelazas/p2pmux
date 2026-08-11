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


def fleet_record(pairing_file: Path) -> str:
    """The `[[machine]]` blocks of a pairing file, and nothing else.

    `p2pmux machines` deliberately lists session members alongside the fleet —
    a machine of yours that is in the session, and under its own heading anyone
    else who is. So the listing is the wrong thing to assert ownership against:
    a peer that joined and was *refused* the fleet still appears in it, which is
    the whole point of printing them apart. This reads the record instead,
    which is the only thing that decides.
    """
    text = pairing_file.read_text()
    return "".join(
        block for block in text.split("[[machine]]")[1:]
    )


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

        # `pair` also opens the ten-minute window, and a droplet arriving inside
        # it would be written into the fleet by the window rather than by the
        # token — which would make this scenario pass without testing anything.
        # Closing it is what the tenth minute would do, and this run has no ten
        # minutes to spare.
        pairing_file = harness.home / ".config" / "p2pmux" / "pairing.toml"
        pairing_file.write_text(
            "\n".join(
                line
                for line in pairing_file.read_text().splitlines()
                if not line.startswith("pending_until")
            )
            + "\n"
        )
        check(
            "the pairing window is shut, so only the token can let anything in",
            "pending_until" not in pairing_file.read_text(),
            "",
        )

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

        # A fleet record outlives *membership*: the droplet stops being in the
        # session and keeps its row, as `asleep`. A row that exists only while
        # both are in one session is a member list, which is the bug this
        # scenario was written after.
        droplet.reap()
        time.sleep(6.0)
        check(
            "it keeps its row once it leaves the session",
            "build-box" in fleet_record(pairing_file),
            pairing_file.read_text()[-200:],
        )

        # --- revocation -------------------------------------------------------
        # Deliberately before the session is torn down: restarting one mints a
        # new ticket, and a token refused because its session is gone would
        # prove nothing about revocation.
        revoked = mac("enroll", "--revoke")
        check("`--revoke` withdraws the invitation", "revoked" in revoked, revoked.strip()[-160:])

        # The same droplet, forgetting everything, coming back under a new name
        # with the withdrawn token. Its machine id is unchanged, so an
        # enrolment that wrongly succeeded would *rename* the row it already
        # has — which makes the check sharper than an absence: the fleet must
        # still say `build-box` and must never say `intruder`.
        droplet.reset_home()
        try:
            # It may well *join*: the ticket is a session invitation, and
            # withdrawing an enrolment is not withdrawing that. Only the fleet
            # write is refused, so whether this command succeeds is not the
            # question — what the Mac's record says afterwards is.
            droplet.cli(f"enroll {token} --name intruder --accept-work", timeout=180)
        except RemoteError as error:
            print(f"  (the refused enrolment exited non-zero: {error})", flush=True)
        time.sleep(10.0)
        after_revoke = mac("machines")
        check(
            "the withdrawn token enrols nothing",
            "intruder" not in fleet_record(pairing_file),
            pairing_file.read_text()[-240:],
        )
        check(
            "and cannot rename the machine it already enrolled",
            "build-box" in fleet_record(pairing_file),
            pairing_file.read_text()[-240:],
        )
        # It did *join*, and the fleet list says so in the one place that
        # cannot be mistaken for ownership. This is the distinction the whole
        # feature turns on: in your session is not in your fleet.
        guests = after_revoke.split("IN THIS SESSION, NOT YOURS", 1)
        check(
            "and is listed as a guest of the session rather than as a machine",
            len(guests) == 2 and "intruder" in guests[1],
            after_revoke.strip()[-300:],
        )

        # …and the record survives the session that introduced them ending.
        droplet.reap()
        for name in re.findall(r"^(\S+)\s", mac("ls"), re.MULTILINE):
            if name != "NAME":
                mac("kill", name, "--yes")
        time.sleep(4.0)
        after = mac("machines")
        check(
            "the fleet is still there once every session is gone",
            "build-box" in after,
            after.strip()[-300:],
        )

    print()
    failed = [label for label, passed, _ in checks if not passed]
    print(f"{len(checks) - len(failed)}/{len(checks)} checks passed")
    for label in failed:
        print(f"  FAILED: {label}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
