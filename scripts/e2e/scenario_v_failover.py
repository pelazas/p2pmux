"""Scenario V -- the coordinator's machine dies and the room keeps working.

Every other scenario assumes the coordinator stays up. This one kills it, and asks the
two questions a user would ask next: *is my terminal still there*, and *can we carry on*.

They are separate questions and they fail separately. Panes survive a lost coordinator
for free -- each one runs on its own host and settles its own control lease -- so the
first answer was already yes before any of this was built. The second was no: layout
edits, joins and presence all run through the coordinator, and losing it froze them
permanently. What is under test is the unfreezing.

The kill is deliberately violent. `pkill -9` leaves no departure notice, so the
survivors learn about it the way a real crash or a closed laptop teaches them: their
control stream stops answering and nothing explains why. A graceful `p2pmux kill` would
take the coordinator out of the roster on its way past and test nothing.

  panes survive     fra can still type into the Mac's pane with nobody coordinating;
  frozen, not dead  a split is refused with a notice rather than hanging;
  somebody takes over  the earliest surviving member promotes itself;
  the room follows  the other survivor reattaches without being handed anything;
  edits work again  a split committed by the new coordinator lands on both machines.

Grace is cut to `GRACE_SECS` through the environment. Five real minutes per run is the
difference between a check that gets run and one that gets skipped.

Run:  scripts/e2e/provision_droplets.sh create
      cargo build --release
      python3 scripts/e2e/scenario_v_failover.py
"""

from __future__ import annotations

import re
import socket
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import BINARY, Harness  # noqa: E402
from remote import RemoteError, hosts_from_manifest, spawn_remote  # noqa: E402

CTRL_P = b"\x10"
COLS, ROWS = 180, 44
GRACE_SECS = 20
# Grace, plus the stagger the second-ranked survivor waits, plus room for a dial across
# an ocean and a fresh snapshot to come back.
FAILOVER_TIMEOUT = GRACE_SECS + 60.0
GRACE_ENV = {"P2PMUX_FAILOVER_GRACE_SECS": str(GRACE_SECS)}

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    print(f"  [{'PASS' if passed else 'FAIL'}] {label}{(' -- ' + detail) if detail else ''}",
          flush=True)


def guard(label: str, action, detail: str = "") -> bool:
    try:
        action()
    except AssertionError as failure:
        check(label, False, str(failure)[:400])
        return False
    check(label, True, detail)
    return True


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def pane_body_cell(screen: str, host: str) -> tuple[int, int]:
    for row, line in enumerate(screen.split("\n")):
        index = line.find(f"host: {host}")
        if index == -1:
            continue
        left = line.rfind("┌", 0, index)
        return (left if left != -1 else 0) + 3, row + 3
    raise AssertionError(f"no pane hosted by {host!r} on screen:\n{screen}")


def panes_on_screen(screen: str) -> int:
    return len(re.findall(r"Pane #\d+", screen))


def main() -> int:
    if not BINARY.exists():
        print(f"{BINARY} missing -- run: cargo build --release", file=sys.stderr)
        return 2
    try:
        nyc_host, fra_host = hosts_from_manifest()[:2]
    except (RemoteError, ValueError) as error:
        print(error, file=sys.stderr)
        return 2

    print(f"== scenario V: kill the coordinator on {nyc_host.alias} ==", flush=True)
    for host in (nyc_host, fra_host):
        if not host.binary_ready():
            print(f"{host.alias} has no binary at {host.binary}", file=sys.stderr)
            return 2
        host.env.update(GRACE_ENV)
        host.reset_network()
        host.reset_home()

    nyc_box = nyc_host.run("hostname").strip()
    fra_box = fra_host.run("hostname").strip()
    mac_box = socket.gethostname()
    print(f"   coordinator {nyc_box} / members {mac_box}, {fra_box}", flush=True)

    try:
        with Harness("scenario_v_failover") as harness:
            nyc = spawn_remote(
                harness,
                nyc_host,
                "nyc",
                ["create", "--name", "nyc", "--session-name", "failover"],
                cols=COLS,
                rows=ROWS,
            )
            nyc.wait_ready(timeout=60.0)
            if not guard(
                "coordinator came up in nyc3",
                lambda: nyc.wait_for(r"Pane #\d+", timeout=60.0),
                nyc_box,
            ):
                print(nyc.snapshot(), flush=True)
                return 1

            ticket = nyc_host.cli("ticket").strip().splitlines()[-1].strip()

            # The Mac joins first, so it is the earliest survivor and therefore the one
            # the succession picks. That is not incidental: the check below asserts *which*
            # machine takes over, not merely that some machine did.
            mac = harness.spawn(
                "mac",
                ["join", ticket, "--name", "mac"],
                cols=COLS,
                rows=ROWS,
                env=dict(GRACE_ENV),
            )
            mac.wait_ready(timeout=60.0)
            if not guard(
                "this Mac joined",
                lambda: mac.wait_for(r"Pane #\d+", timeout=90.0),
                mac_box,
            ):
                print(mac.snapshot(), flush=True)
                return 1

            fra = spawn_remote(
                harness,
                fra_host,
                "fra",
                ["join", ticket, "--name", "fra"],
                cols=COLS,
                rows=ROWS,
            )
            fra.wait_ready(timeout=60.0)
            if not guard(
                "fra1 joined",
                lambda: fra.wait_for(r"Pane #\d+", timeout=90.0),
                fra_box,
            ):
                print(fra.snapshot(), flush=True)
                return 1

            # Each survivor hosts a pane of its own, so that after the kill there is still
            # something on screen that belongs to a machine that is still up.
            for peer, label in ((mac, "mac"), (fra, "fra")):
                chord(peer, CTRL_P, b"n")
                guard(
                    f"{label} hosts a pane of its own",
                    lambda peer=peer, label=label: peer.wait_for(
                        f"host: {label}", timeout=60.0
                    ),
                )
            panes_before = panes_on_screen(mac.snapshot())

            # -------------------------------------------------- the coordinator dies
            print("   killing the coordinator, hard", flush=True)
            nyc_host.reap()
            check("the coordinator's node is gone", not nyc_host._p2pmux_pids(), nyc_box)

            guard(
                "the survivors notice",
                lambda: mac.wait_for("coordinator unreachable", timeout=60.0),
            )
            guard(
                "a layout edit is refused with a notice, not left hanging",
                lambda: (
                    chord(mac, CTRL_P, b"n"),
                    mac.wait_for("layout changes are paused", timeout=20.0),
                )[-1],
            )

            # The claim that matters most: with nobody coordinating anything, one member
            # can still drive a terminal on another member's machine.
            guard(
                "fra can still type into a pane hosted on this Mac",
                lambda: fra.wait_until(
                    lambda screen: "host: mac control: free" in screen,
                    timeout=60.0,
                    what="the Mac's pane to go free",
                ),
            )
            fra.click(*pane_body_cell(fra.snapshot(), "mac"))
            fra.run_in_shell(f"echo still-alive-$(hostname)")
            guard(
                "and the command runs on this Mac with no coordinator in the session",
                lambda: fra.wait_for(re.escape(f"still-alive-{mac_box}"), timeout=60.0),
                mac_box,
            )

            # -------------------------------------------------- somebody takes over
            guard(
                "this Mac promotes itself once grace expires",
                lambda: mac.wait_until(
                    lambda screen: "coordinated here" in screen,
                    timeout=FAILOVER_TIMEOUT,
                    what="the Mac to take the coordinator role",
                ),
                f"{GRACE_SECS}s grace",
            )
            guard(
                "fra reattaches to it without being handed a ticket",
                lambda: fra.wait_until(
                    lambda screen: "coordinator unreachable" not in screen,
                    timeout=FAILOVER_TIMEOUT,
                    what="fra to find the new coordinator",
                ),
            )

            # -------------------------------------------------- the session works again
            chord(fra, CTRL_P, b"n")
            guard(
                "a split committed by the new coordinator lands",
                lambda: fra.wait_until(
                    lambda screen: panes_on_screen(screen) > panes_before,
                    timeout=90.0,
                    what="a pane the new coordinator committed",
                ),
                f"{panes_before} panes before",
            )
            guard(
                "and this Mac draws it too",
                lambda: mac.wait_until(
                    lambda screen: panes_on_screen(screen) > panes_before,
                    timeout=90.0,
                    what="the new pane on the coordinator's own screen",
                ),
            )
    finally:
        for host in (nyc_host, fra_host):
            try:
                host.reap()
            except RemoteError as error:
                print(f"   (cleanup on {host.alias}: {error})", file=sys.stderr)

    failed = [label for label, passed, _ in checks if not passed]
    print(f"\n== {len(checks) - len(failed)}/{len(checks)} passed ==", flush=True)
    for label in failed:
        print(f"   FAILED: {label}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
