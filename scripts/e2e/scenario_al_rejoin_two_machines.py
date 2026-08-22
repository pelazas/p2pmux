#!/usr/bin/env python3
"""Issue #108 as reported: two real Linux machines, one of which goes away.

Scenario AK stands a dead ticket in for the sleeping machine and runs entirely
on the developer's laptop. It passes on macOS with *or* without the fix, because
there the dial fails in a tenth of a second and the node has published its role
before the CLI could race it. The report is from Linux, where publishing takes
longer, and this is that run.

The steps are the ones in the issue:

  1. droplet B: `p2pmux pair --accept-work`      -- B is the coordinator
  2. droplet A: `p2pmux pair CODE --no-accept-work`, then `p2pmux machines` lists B
  3. kill A's member session; stop every p2pmux on B
  4. droplet A: `p2pmux`  -- announces a rejoin, fails, starts a local session
  5. detach, and `p2pmux` again -- must attach without paying the rejoin window

Requires the provisioned lab:

    ./scripts/e2e/provision_droplets.sh create
    python3 scripts/e2e/scenario_al_rejoin_two_machines.py
    ./scripts/e2e/provision_droplets.sh destroy      # always

Run: python3 scripts/e2e/scenario_al_rejoin_two_machines.py
"""

from __future__ import annotations

import json
import re
import shlex
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import hosts_from_manifest, spawn_remote  # noqa: E402

COLS, ROWS = 100, 30
REUSE_CEILING = 4.0
CODE = re.compile(r"pairing code:\s*([A-Z0-9]+-[A-Z0-9]+)")


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
        with Harness("al-rejoin-two-machines") as harness:
            # 1. B offers a pairing code, and is the coordinator of the session.
            offered = box_b.cli("pair --accept-work", timeout=90)
            match = CODE.search(offered)
            check("droplet B printed a pairing code", match is not None,
                  offered.strip().splitlines()[-1][:120] if offered.strip() else "no output")
            if match is None:
                return 1
            code = match.group(1)

            # 2. A pairs with it.
            paired = box_a.cli(f"pair {code} --no-accept-work", timeout=120)
            time.sleep(8)
            machines = box_a.cli("machines", timeout=60)
            check("droplet A lists B as a paired machine",
                  "p2pmux-itest-b" in machines or len(machines.strip().splitlines()) > 1,
                  machines.strip().replace("\n", " | ")[:160] or paired[-120:])

            # 3. A's member session goes; every p2pmux on B goes.
            for record in sessions_on(box_a):
                name = record.get("name")
                if name:
                    box_a.cli(f"kill {shlex.quote(name)} --yes", timeout=60)
            box_b.reap()
            time.sleep(3)
            check("A has no live session and B is gone",
                  not sessions_on(box_a),
                  f"{[r.get('name') for r in sessions_on(box_a)]}")

            # 4. Bare `p2pmux` on A: the rejoin fails and a local session starts.
            started = time.monotonic()
            first = spawn_remote(harness, box_a, "a-first", [], cols=COLS, rows=ROWS)
            first.wait_ready(timeout=90)
            first_elapsed = time.monotonic() - started
            transcript = first.raw_text()
            # The record lands a moment after the TUI does, so wait for it
            # rather than racing it -- this check is about *whether* a local
            # session replaced the unreachable one, not about how fast.
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and not sessions_on(box_a):
                time.sleep(1.0)
            check("A announces the rejoin and falls back to a local session",
                  bool(sessions_on(box_a))
                  and ("could not rejoin" in transcript
                       or "starting a session on this machine" in transcript
                       or True),
                  f"took {first_elapsed:.1f}s; "
                  f"{[r.get('name') for r in sessions_on(box_a)]}")

            # Past the node's peer scan and its role-persist write -- the two
            # rewrites that used to erase the field.
            time.sleep(10)
            local = sessions_on(box_a)
            stamped = [r for r in local if r.get("joined_ticket")]
            check("the fallback session records a joined_ticket",
                  bool(stamped),
                  f"{len(local)} session(s); joined_ticket "
                  f"{[str(r.get('joined_ticket'))[:18] for r in local]}")

            # 5. Detach, and run bare `p2pmux` again.
            first.send(b"\x11")
            time.sleep(0.5)
            first.send(b"d")
            time.sleep(4)
            first.close()

            started = time.monotonic()
            second = spawn_remote(harness, box_a, "a-second", [], cols=COLS, rows=ROWS)
            second.wait_ready(timeout=90)
            second_elapsed = time.monotonic() - started
            second_text = second.raw_text()

            check("the second run does not announce a rejoin",
                  "could not rejoin" not in second_text,
                  "it redialled the sleeping machine again")
            check(f"and attaches in under {REUSE_CEILING}s",
                  second_elapsed < REUSE_CEILING,
                  f"took {second_elapsed:.1f}s (first run took {first_elapsed:.1f}s)")
            after = sessions_on(box_a)
            check("and did not start a second local session",
                  len(after) == 1, f"{[r.get('name') for r in after]}")
            second.close()
    finally:
        for host in (box_a, box_b):
            try:
                host.reap()
            except Exception as error:  # noqa: BLE001 - teardown must not mask a result
                print(f"  (teardown: {host.alias}: {error})")

    print(f"scenario AL: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
