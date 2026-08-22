#!/usr/bin/env python3
"""Issue #107: your trusted machines join the session you just opened.

The auto-join machinery already existed and already worked for the easy case:
both machines up, one starts a session, the other follows within seconds. What
did not work is the case the promise is actually about -- the machine that was
*off* when you started the session.

A machine that comes back finds its own session record still on disk, because a
record outlives the node that wrote it. `follow_fleet_invite` read that as "I am
already in my home session", so it never rejoined; and since invitations travel
over the home session, it never heard about anything started while it was away.
It sat there, paired, awake, and alone.

Two checks, both on real machines:

  live   B is up when A creates a session. B joins it.
  woken  B is down when A creates a session. B's fleet agent starts, and B ends
         up in that session.

Requires the provisioned lab:

    ./scripts/e2e/provision_droplets.sh create
    python3 scripts/e2e/scenario_am_trusted_autojoin.py
    ./scripts/e2e/provision_droplets.sh destroy      # always

Run: python3 scripts/e2e/scenario_am_trusted_autojoin.py
"""

from __future__ import annotations

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import hosts_from_manifest, spawn_remote  # noqa: E402

COLS, ROWS = 100, 30
CODE = re.compile(r"pairing code:\s*([A-Z0-9]+-[A-Z0-9]+)")

# A machine that has just woken has to rejoin its home session and then be told
# about the other one, both over the network. Generous, because the failure this
# guards is "never", not "slowly".
JOIN_DEADLINE = 60.0


def live_sessions(host) -> list[str]:
    """Session names on the droplet whose node is actually running.

    The records of dead nodes stay on disk until something sweeps them, and
    counting those would make this scenario pass on the very bug it is for.
    """
    out = host.run(
        "python3 - <<'EOF'\n"
        "import json, glob, os\n"
        f"live = set()\n"
        "for line in os.popen('ps -eo pid=,args=').read().splitlines():\n"
        "    parts = line.split(None, 1)\n"
        "    if len(parts) == 2 and '__node' in parts[1] and 'p2pmux' in parts[1]:\n"
        "        live.add(int(parts[0]))\n"
        f"for p in sorted(glob.glob({host.home + '/.local/state/p2pmux/sessions'!r} + '/*.json')):\n"
        "    try: d = json.load(open(p))\n"
        "    except Exception: continue\n"
        "    if d.get('node_pid') in live:\n"
        "        print(d.get('name'))\n"
        "EOF",
        check=False,
    )
    return sorted(name for name in out.split() if name)


def wait_for_session(host, name: str, deadline: float) -> bool:
    end = time.monotonic() + deadline
    while time.monotonic() < end:
        if name in live_sessions(host):
            return True
        time.sleep(3.0)
    return False


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
        # A offers, so A holds the home session and B is the machine that comes
        # and goes -- a desktop and a laptop, which is the shape of the ask.
        offered = box_a.cli("pair --accept-work", timeout=90)
        match = CODE.search(offered)
        check("A printed a pairing code", match is not None)
        if match is None:
            return 1
        box_b.cli(f"pair {match.group(1)} --accept-work", timeout=120)
        time.sleep(12)
        check("both machines are in the home session",
              bool(live_sessions(box_a)) and bool(live_sessions(box_b)),
              f"A={live_sessions(box_a)} B={live_sessions(box_b)}")

        with Harness("am-trusted-autojoin") as harness:
            # --- live -------------------------------------------------------
            live = spawn_remote(
                harness, box_a, "a-live", ["create", "--session-name", "together"],
                cols=COLS, rows=ROWS,
            )
            live.wait_ready(timeout=90)
            joined = wait_for_session(box_b, "together", JOIN_DEADLINE)
            check("B joins a session A opens while B is up", joined,
                  f"B has {live_sessions(box_b)}")
            live.close()

            # --- woken ------------------------------------------------------
            box_b.reap()
            time.sleep(3)
            check("B is now genuinely down", not live_sessions(box_b),
                  f"B has {live_sessions(box_b)}")

            away = spawn_remote(
                harness, box_a, "a-away", ["create", "--session-name", "whileaway"],
                cols=COLS, rows=ROWS,
            )
            away.wait_ready(timeout=90)
            time.sleep(5)

            # B comes back the way a real machine does: its fleet agent starts.
            box_b.run(
                f"env HOME={box_b.home} nohup {box_b.binary} daemon "
                "</dev/null >/tmp/daemon.log 2>&1 & echo ok",
                check=False,
            )
            back = wait_for_session(box_b, "whileaway", JOIN_DEADLINE)
            check("and reaches the one A opened while B was away", back,
                  f"B has {live_sessions(box_b)}; daemon said "
                  f"{box_b.run('tail -3 /tmp/daemon.log', check=False).strip()!r}")
            away.close()
    finally:
        for host in (box_a, box_b):
            try:
                host.reap()
            except Exception as error:  # noqa: BLE001 - teardown must not mask a result
                print(f"  (teardown: {host.alias}: {error})")

    print(f"scenario AM: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
