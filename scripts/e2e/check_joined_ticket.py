#!/usr/bin/env python3
"""Issue #108: does the node keep a `joined_ticket` the CLI recorded, or eat it?

The reported symptom needs two machines and a sleeping one. The mechanism
underneath does not. `open_home` starts a local session and *then* writes
`descriptor.joined_ticket` into its record, while the node has been holding its
own copy of that descriptor since it was launched -- one built from the
bootstrap, where the field is `None`. Every later `store.write(descriptor)` in
the node's loop (a role change, a peer-scan tick) puts the whole record back
from that copy.

So: start a session, stamp a ticket into its record the way `open_home` does,
and watch whether it is still there a few seconds later.

Run: python3 scripts/e2e/check_joined_ticket.py
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, orphans_after, p2pmux_pids, session_store_dirs_for  # noqa: E402

COLS, ROWS = 100, 30
TICKET = "p2pmux-test-ticket-for-issue-108"
# Longer than the node's peer-scan interval, which is what does the rewriting.
WATCH_SECONDS = 25


def session_files(home: Path) -> list[Path]:
    """Every session record in this sandbox HOME, on either platform's path."""
    found: list[Path] = []
    for root in session_store_dirs_for(home):
        if root.is_dir():
            found.extend(sorted(root.glob("*.json")))
    return found


def main() -> int:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    baseline = p2pmux_pids()
    failures = 0

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        if not ok:
            failures += 1
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    with Harness("joined-ticket") as harness:
        peer = harness.spawn(
            "solo", ["create", "--name", "jt", "--session-name", "jt"],
            cols=COLS, rows=ROWS, env=env,
        )
        peer.wait_ready(timeout=30)
        time.sleep(2.0)

        records = session_files(harness.home)
        check("the session wrote a record", len(records) == 1,
              f"found {[str(r) for r in records]}")
        if len(records) != 1:
            return 1
        record = records[0]

        # Exactly what `open_home` does after its rejoin fails.
        data = json.loads(record.read_text())
        data["joined_ticket"] = TICKET
        record.write_text(json.dumps(data))
        print(f"  stamped joined_ticket into {record.name}")

        # Now let the node run. Any rewrite from its own stale copy loses it.
        lost_after = None
        for elapsed in range(WATCH_SECONDS):
            time.sleep(1.0)
            current = json.loads(record.read_text()).get("joined_ticket")
            if current != TICKET:
                lost_after = elapsed + 1
                break

        check(
            "the node leaves the recorded joined_ticket alone",
            lost_after is None,
            "still there after the whole watch" if lost_after is None
            else f"erased after {lost_after}s -- the node put its own copy back",
        )

    leaked = orphans_after(baseline)
    if leaked:
        print(f"leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"joined ticket: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
