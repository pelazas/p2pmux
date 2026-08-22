#!/usr/bin/env python3
"""Issue #108: the local fallback after a failed paired rejoin is not reused.

The report needs two machines and one of them asleep. The mechanism does not:
what is being tested is whether the session `open_home` starts *instead* of the
unreachable one is remembered as this machine's answer to that pairing ticket.

So a real but dead ticket stands in for the sleeping machine. Start a session,
take its ticket, kill it -- now the ticket is well formed and addresses nothing,
which is precisely what a paired machine that has gone to sleep looks like from
here. Write it into this sandbox's pairing record as a ticket that came from
somewhere else (`offered_here = false`, so `rejoin_ticket` is willing to dial
it) and run bare `p2pmux`.

What must then be true:

  1. the rejoin is attempted, fails, and a local session starts (this already worked);
  2. that session's record carries `joined_ticket` -- the half that was missing;
  3. a second bare `p2pmux` attaches it *without* spending the rejoin window again.

Check 3 is the one a user feels. It is timed rather than inferred from the
screen, because "did it wait five seconds" is the entire complaint.

Run: python3 scripts/e2e/scenario_ak_rejoin_fallback.py [repeats]
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import BINARY, Harness, orphans_after, p2pmux_pids, sandbox_environ  # noqa: E402

COLS, ROWS = 100, 30

# `open_home` gives an interactive rejoin about five seconds. A reused local
# answer must not pay it; allow generous headroom for a loaded machine so this
# fails on the behaviour rather than on timing noise.
REJOIN_BUDGET = 5.0
REUSE_CEILING = 4.0


def session_dir(home: Path) -> Path | None:
    for root in (
        home / ".local" / "state" / "p2pmux" / "sessions",
        home / "Library" / "Application Support" / "p2pmux" / "sessions",
    ):
        if root.is_dir():
            return root
    return None


def records(home: Path) -> list[dict]:
    root = session_dir(home)
    if root is None:
        return []
    out = []
    for path in sorted(root.glob("*.json")):
        try:
            out.append(json.loads(path.read_text()))
        except (OSError, json.JSONDecodeError):
            continue
    return out


def cli(home: Path, *args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    env = sandbox_environ()
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_STATE_HOME"] = str(home / ".local" / "state")
    return subprocess.run(
        [str(BINARY), *args],
        capture_output=True, text=True, timeout=timeout, env=env,
    )


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}

    with Harness(f"ak-rejoin-fallback-{index}") as harness:
        # A real ticket for a session that will not exist by the time it is dialled.
        doomed = harness.spawn(
            "doomed", ["create", "--name", "doomed", "--session-name", "doomed"],
            cols=COLS, rows=ROWS, env=env,
        )
        doomed.wait_ready(timeout=30)
        ticket = harness.wait_for_ticket("doomed", timeout=30)
        doomed.close()
        cli(harness.home, "kill", "doomed", "--yes")
        time.sleep(2.0)
        check("the stand-in ticket was minted and its session is gone",
              bool(ticket) and not any(r.get("name") == "doomed" for r in records(harness.home)),
              f"ticket {ticket[:24]}…" if ticket else "no ticket")

        # Paired, with a ticket that came from elsewhere, so bare `p2pmux` dials it.
        pairing = harness.home / ".config" / "p2pmux" / "pairing.toml"
        pairing.parent.mkdir(parents=True, exist_ok=True)
        pairing.write_text(
            f'ticket = "{ticket}"\n'
            "accepts_work = false\n"
            "offered_here = false\n"
        )

        # 1. Bare `p2pmux`: rejoin is attempted, fails, a local session starts.
        #
        # Watched from a thread at 20ms, because the interesting question is not
        # "is the ticket there in the end" but "was it ever missing". The old
        # code could only ever answer yes: the node wrote the record first and
        # the CLI stamped the field over it afterwards, so there was always a
        # window -- and any rewrite the node made inside that window, or after
        # it from its own untouched copy, put the record back without it.
        sightings: list[tuple[str, bool]] = []
        watching = True

        def watch() -> None:
            seen: set[str] = set()
            while watching:
                root = session_dir(harness.home)
                if root is not None:
                    for path in sorted(root.glob("*.json")):
                        try:
                            data = json.loads(path.read_text())
                        except (OSError, json.JSONDecodeError):
                            continue
                        if data.get("name") == "doomed":
                            continue
                        key = f"{path.name}:{data.get('joined_ticket') == ticket}"
                        if key in seen:
                            continue
                        seen.add(key)
                        sightings.append((path.name, data.get("joined_ticket") == ticket))
                time.sleep(0.02)

        watcher = threading.Thread(target=watch, daemon=True)
        watcher.start()

        started = time.monotonic()
        home_peer = harness.spawn("home", [], cols=COLS, rows=ROWS, env=env)
        home_peer.wait_ready(timeout=60)
        first_elapsed = time.monotonic() - started

        # Past the node's peer scan (2s) and its role-persist write, which are
        # the two rewrites that used to eat the field.
        time.sleep(8.0)
        watching = False
        watcher.join(timeout=5)

        local = [r for r in records(harness.home) if r.get("name") != "doomed"]
        stamped = [r for r in local if r.get("joined_ticket") == ticket]
        check("a local session started in place of the unreachable one",
              len(local) == 1, f"took {first_elapsed:.1f}s; {[r.get('name') for r in local]}")

        # 2. The fallback is recorded as this machine's answer to that ticket...
        check("the fallback session records the ticket it stands in for",
              bool(stamped),
              f"joined_ticket values {[str(r.get('joined_ticket'))[:20] for r in local]}")

        # ...from its very first write, and without ever losing it again.
        without = [name for name, ok in sightings if not ok]
        check("and carries it from the moment the record exists, through every rewrite",
              not without,
              f"records seen with no joined_ticket: {without}" if without
              else f"{len(sightings)} sighting(s), all stamped")

        # 3. Detach, and the next bare `p2pmux` reuses it instead of redialling.
        home_peer.send(b"\x11")   # Ctrl+Q
        time.sleep(0.5)
        home_peer.send(b"d")
        time.sleep(3.0)
        home_peer.close()

        started = time.monotonic()
        again = harness.spawn("again", [], cols=COLS, rows=ROWS, env=env)
        again.wait_ready(timeout=60)
        second_elapsed = time.monotonic() - started
        second_transcript = again.raw_text()

        check("the second run does not announce a rejoin at all",
              "could not rejoin" not in second_transcript,
              second_transcript[-160:].replace("\n", " ") if "could not rejoin" in second_transcript else "")
        check(f"and attaches in under {REUSE_CEILING}s rather than paying the rejoin window",
              second_elapsed < REUSE_CEILING,
              f"took {second_elapsed:.1f}s (first run took {first_elapsed:.1f}s)")

        after = records(harness.home)
        check("and it did not start yet another session",
              len([r for r in after if r.get("name") != "doomed"]) == 1,
              f"{[r.get('name') for r in after]}")

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    verbose = os.environ.get("VERBOSE", "1") != "0"
    baseline = p2pmux_pids()
    failures = 0
    for index in range(1, repeats + 1):
        print(f"scenario AK (paired rejoin fallback) run {index}/{repeats}")
        results = run_once(index, verbose)
        failed = [name for name, ok, _ in results if not ok]
        print(f"  {len(results) - len(failed)}/{len(results)} checks passed")
        failures += len(failed)
    leaked = orphans_after(baseline)
    if leaked:
        print(f"scenario AK: leaked p2pmux pids {sorted(leaked)}")
        failures += 1
    print(f"scenario AK: {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
