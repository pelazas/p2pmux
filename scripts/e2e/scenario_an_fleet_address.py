#!/usr/bin/env python3
"""A fleet finds itself again after the session it was paired around ends.

This is the 2026-08-16 failure, run on two real Linux machines. Two paired
machines held a `pairing.toml` naming a session that had not existed for days;
the fleet agent redialled it every fifteen seconds for four of them, every
attempt reported "could not reach the session host … the invite may be out of
date", and nothing in the product could ever have corrected it. A machine that
cannot join hears no announcements, and a machine that hears nothing cannot
join. Only a human re-running `p2pmux pair` got the fleet back.

The steps are that failure, in order:

  1. droplet B: `p2pmux pair --accept-work`     -- B coordinates session one
  2. droplet A: `p2pmux pair CODE --accept-work`
  3. both machines hold the same fleet key, and A knows session one's ticket
  4. session one ends everywhere
  5. droplet B: `p2pmux pair` again             -- B coordinates session *two*
  6. droplet A: the fleet agent, with a `pairing.toml` still naming session one

The check that makes this mean anything is in step 6: A's stored ticket must
still be the dead one at the moment it joins the live session. Landing in
session two while holding session one's address is the whole fix. If the two
tickets were equal the run proves nothing, so that is asserted rather than
assumed.

Requires the provisioned lab:

    ./scripts/e2e/provision_droplets.sh create
    python3 scripts/e2e/scenario_an_fleet_address.py
    ./scripts/e2e/provision_droplets.sh destroy      # always

Run: python3 scripts/e2e/scenario_an_fleet_address.py
"""

from __future__ import annotations

import json
import re
import shlex
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from remote import BASE_ENV, hosts_from_manifest  # noqa: E402

CODE = re.compile(r"pairing code:\s*([A-Z0-9]+-[A-Z0-9]+)")
FLEET_KEY = re.compile(r'^fleet_key\s*=\s*"([0-9a-f]{64})"', re.MULTILINE)
TICKET = re.compile(r'^ticket\s*=\s*"([^"]+)"', re.MULTILINE)
# The agent looks again on this cadence when nobody is hosting, so a machine
# that has to notice a session started a moment ago needs a little over one.
FOLLOW_TIMEOUT = 100.0


def pairing_toml(host) -> str:
    return host.run(
        f"cat {shlex.quote(host.home)}/.config/p2pmux/pairing.toml 2>/dev/null || true",
        check=False,
    )


def sessions_on(host) -> list[dict]:
    """Every session record in the droplet's sandbox HOME, parsed."""
    out = host.run(
        f"cat {shlex.quote(host.home)}/.local/state/p2pmux/sessions/*.json 2>/dev/null || true",
        check=False,
    )
    records = []
    for chunk in out.replace("}{", "}\n{").splitlines():
        chunk = chunk.strip()
        if not chunk.startswith("{"):
            continue
        try:
            records.append(json.loads(chunk))
        except json.JSONDecodeError:
            continue
    return records


def hosted_ticket(host) -> str | None:
    """The ticket of the session this machine is coordinating, if any."""
    for record in sessions_on(host):
        if record.get("role") == "coordinator" and record.get("ticket"):
            return record["ticket"]
    return None


def start_fleet_agent(host, log: str) -> str:
    """Run `p2pmux daemon` on the droplet, detached, and return its pid.

    `setsid` so it outlives the ssh that started it, which is the whole point:
    the agent has to still be looking when the other machine starts a session.
    """
    environment = {**BASE_ENV, "HOME": host.home, **host.env}
    assignments = " ".join(f"{k}={shlex.quote(v)}" for k, v in environment.items())
    pid = host.run(
        f"setsid env {assignments} {shlex.quote(host.binary)} daemon "
        f"> {shlex.quote(log)} 2>&1 < /dev/null & echo $!"
    )
    return pid.strip()


def kill_every_session(host) -> None:
    for record in sessions_on(host):
        name = record.get("name")
        if name:
            host.cli(f"kill {shlex.quote(name)} --yes", timeout=60)
    host.reap()


def main() -> int:
    hosts = hosts_from_manifest()
    if len(hosts) < 2:
        print("need two provisioned droplets")
        return 1
    box_a, box_b = hosts[0], hosts[1]
    failures = 0

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        if not ok:
            failures += 1
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    print(f"  A={box_a.hostname}  B={box_b.hostname}")
    for host in (box_a, box_b):
        host.reset_home()

    try:
        # 1. B offers a pairing code and coordinates the first session.
        offered = box_b.cli("pair --accept-work", timeout=90)
        match = CODE.search(offered)
        check(
            "droplet B printed a pairing code",
            match is not None,
            offered.strip().splitlines()[-1][:120] if offered.strip() else "no output",
        )
        if match is None:
            return 1
        code = match.group(1)

        # 2. A pairs with it.
        box_a.cli(f"pair {code} --accept-work", timeout=120)
        time.sleep(8)

        # 3. Both hold the same fleet address, and A knows the first session.
        key_a = FLEET_KEY.search(pairing_toml(box_a))
        key_b = FLEET_KEY.search(pairing_toml(box_b))
        check(
            "both machines hold a fleet address",
            key_a is not None and key_b is not None,
            f"A={bool(key_a)} B={bool(key_b)}",
        )
        check(
            "and it is the same address",
            key_a is not None and key_b is not None and key_a.group(1) == key_b.group(1),
            "the pairing handed one over" if key_a and key_b else "",
        )
        first_ticket = hosted_ticket(box_b)
        stored_before = TICKET.search(pairing_toml(box_a))
        check(
            "A's record names the session it paired into",
            first_ticket is not None
            and stored_before is not None
            and stored_before.group(1) == first_ticket,
            f"stored {str(stored_before and stored_before.group(1))[:24]}",
        )

        # 4. That session ends everywhere. This is the state the fleet used to
        #    never come back from.
        kill_every_session(box_a)
        kill_every_session(box_b)
        time.sleep(3)
        check(
            "the session both machines were paired around is gone",
            not sessions_on(box_a) and not sessions_on(box_b),
            f"A={[r.get('name') for r in sessions_on(box_a)]} "
            f"B={[r.get('name') for r in sessions_on(box_b)]}",
        )

        # 5. B starts a second session. Nothing tells A about it: A has no node
        #    running, so there is no session for an announcement to arrive in.
        box_b.cli("pair --accept-work", timeout=90)
        time.sleep(6)
        second_ticket = hosted_ticket(box_b)
        check(
            "droplet B is coordinating a second, different session",
            second_ticket is not None and second_ticket != first_ticket,
            "the two runs produced the same ticket" if second_ticket == first_ticket else "",
        )

        # The control. Without this the run proves nothing: A must still be
        # holding the *dead* address at the moment it finds the live session.
        stored_now = TICKET.search(pairing_toml(box_a))
        check(
            "A's record still names the session that ended",
            stored_now is not None and stored_now.group(1) == first_ticket,
            f"stored {str(stored_now and stored_now.group(1))[:24]}",
        )

        # 6. A's fleet agent, started knowing only the dead address.
        agent_log = f"{box_a.home}/fleet-agent.log"
        agent = start_fleet_agent(box_a, agent_log)
        deadline = time.monotonic() + FOLLOW_TIMEOUT
        joined = None
        while time.monotonic() < deadline:
            for record in sessions_on(box_a):
                if record.get("joined_ticket") == second_ticket:
                    joined = record
                    break
            if joined:
                break
            time.sleep(2.0)
        elapsed = time.monotonic() - (deadline - FOLLOW_TIMEOUT)
        log = box_a.run(f"cat {shlex.quote(agent_log)} 2>/dev/null || true", check=False)
        check(
            "A's fleet agent joined the session B is actually in",
            joined is not None,
            f"after {elapsed:.0f}s; log: {log.strip().replace(chr(10), ' | ')[:200]}",
        )
        check(
            "and it never dialled the dead one",
            "could not reach the session host" not in log,
            log.strip().replace("\n", " | ")[:200],
        )
        if agent.isdigit():
            box_a.run(f"kill {agent} 2>/dev/null || true", check=False)

        # And the fleet is whole again, on both sides.
        time.sleep(5)
        peers_b = {p.get("name") for r in sessions_on(box_b) for p in r.get("peers", [])}
        check(
            "and B sees A in it",
            len(peers_b) >= 2,
            f"peers on B: {sorted(peers_b)}",
        )
    finally:
        for host in (box_a, box_b):
            try:
                host.reap()
            except Exception as error:  # noqa: BLE001 - teardown must not mask a result
                print(f"  (teardown: {host.alias}: {error})")

    print(f"scenario AN: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
