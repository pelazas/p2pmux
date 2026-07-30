"""Scenario O -- three members, two continents, and one Mac behind a home router.

Scenario N proved two droplets can share a session, but droplets are the easy case:
public IPs, no NAT, nothing in the way. The machine p2pmux actually ships to is a Mac
behind a consumer router, and the thing that machine's owner will want to believe is
that somebody else can drive a terminal *on it*.

So this scenario puts the coordinator in nyc3, joins from this Mac, joins again from
fra1, and then has the fra1 member take control of the Mac's pane and run a command on
it. Every claim in the pitch is one of the checks below:

  three-way join   a NAT'd Mac and two droplets in one room;
  ×N badge         the coordinator reports both peers, not just the last one;
  three hosts      each machine hosts its own PTY and all three render everywhere;
  onto the Mac     fra1 types, and the process runs on this Mac under this user;
  off the Mac      this Mac types, and the process runs in fra1.

Run:  scripts/e2e/provision_droplets.sh create
      cargo build --release
      python3 scripts/e2e/scenario_o_three_peers.py
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
IDLE_AFTER = 30.0

# `direct 85ms`, `relayed 140ms ×2`.
LINK_BADGE = re.compile(r"\b(direct|relayed|other)\b(?:\s+(?:<1|\d+)ms)?(?:\s+×(\d+))?")

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


def peer_count(screen: str) -> int | None:
    """How many peers the tab bar says are connected. No `×N` suffix means one."""
    match = LINK_BADGE.search(screen.split("\n", 1)[0])
    if not match:
        return None
    return int(match.group(2)) if match.group(2) else 1


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


def wait_for_free_pane(peer, host: str, timeout: float = IDLE_AFTER + 20.0) -> str:
    return peer.wait_until(
        lambda screen: f"host: {host} control: free" in screen,
        timeout=timeout,
        what=f"pane hosted by {host} to go free",
    )


def drive(typist, watcher, host: str, command: str, expect: str, label: str) -> None:
    """`typist` claims the pane hosted by `host` and runs one command in it."""
    if not guard(f"{label}: the pane frees up first", lambda: wait_for_free_pane(typist, host)):
        return
    typist.click(*pane_body_cell(typist.snapshot(), host))
    typist.run_in_shell(command)
    guard(
        f"{label}: the whole line reached the pane's shell",
        lambda: typist.wait_for(re.escape(command), timeout=60.0),
        command,
    )
    guard(
        f"{label}: it ran on {host}'s machine",
        lambda: typist.wait_for(re.escape(expect), timeout=60.0),
        expect,
    )
    guard(
        f"{label}: the third member saw it too",
        lambda: watcher.wait_for(re.escape(expect), timeout=60.0),
    )


def main() -> int:
    if not BINARY.exists():
        print(f"{BINARY} missing -- run: cargo build --release", file=sys.stderr)
        return 2
    try:
        nyc_host, fra_host = hosts_from_manifest()[:2]
    except (RemoteError, ValueError) as error:
        print(error, file=sys.stderr)
        return 2

    print(f"== scenario O: {nyc_host.alias} + this Mac + {fra_host.alias} ==", flush=True)
    for host in (nyc_host, fra_host):
        if not host.binary_ready():
            print(f"{host.alias} has no binary at {host.binary}", file=sys.stderr)
            return 2
        host.reset_network()
        host.reset_home()

    nyc_box = nyc_host.run("hostname").strip()
    fra_box = fra_host.run("hostname").strip()
    mac_box = socket.gethostname()
    print(f"   {nyc_box} / {mac_box} / {fra_box}", flush=True)

    try:
        with Harness("scenario_o_three_peers") as harness:
            nyc = spawn_remote(
                harness,
                nyc_host,
                "nyc",
                ["create", "--name", "nyc", "--session-name", "itest3"],
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

            mac = harness.spawn("mac", ["join", ticket, "--name", "mac"], cols=COLS, rows=ROWS)
            mac.wait_ready(timeout=60.0)
            if not guard(
                "this Mac joined from behind its router",
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
                "the third member joined from fra1",
                lambda: fra.wait_for(r"Pane #\d+", timeout=90.0),
                fra_box,
            ):
                print(fra.snapshot(), flush=True)
                return 1

            guard(
                "the coordinator reports two connected peers",
                lambda: nyc.wait_until(
                    lambda screen: peer_count(screen) == 2,
                    timeout=60.0,
                    what="a ×2 connectivity badge",
                ),
                nyc.snapshot().split("\n", 1)[0][-24:].strip(),
            )

            # Each member hosts a terminal of its own.
            for peer, label in ((mac, "mac"), (fra, "fra")):
                chord(peer, CTRL_P, b"n")
                peer.settle(quiet_for=1.0, timeout=25.0)
                guard(
                    f"{label} hosts a pane of its own",
                    lambda peer=peer, label=label: peer.wait_for(f"host: {label}", timeout=45.0),
                )
            for peer in (nyc, mac, fra):
                guard(
                    f"{peer.name} sees all three members' panes",
                    lambda peer=peer: peer.wait_until(
                        lambda screen: all(
                            f"host: {name}" in screen for name in ("nyc", "mac", "fra")
                        ),
                        timeout=60.0,
                        what="three hosted panes",
                    ),
                )

            # The claim that matters to whoever installs this: someone on another
            # continent typing into a terminal on *your* Mac, running as you.
            drive(
                fra,
                nyc,
                "mac",
                "echo O-ONTO-MAC-$(hostname)",
                f"O-ONTO-MAC-{mac_box}",
                "fra1 drives this Mac's terminal",
            )
            drive(
                mac,
                nyc,
                "fra",
                "echo O-FROM-MAC-$(hostname)",
                f"O-FROM-MAC-{fra_box}",
                "this Mac drives fra1's terminal",
            )

            for peer in (nyc, mac, fra):
                check(f"{peer.name} still alive", peer.alive)

            if any(not passed for _, passed, _ in checks):
                for peer in (nyc, mac, fra):
                    print(f"\n--- {peer.name} screen ---", flush=True)
                    print(peer.snapshot(), flush=True)
    finally:
        for host in (nyc_host, fra_host):
            try:
                host.reap()
                host.reset_network()
            except RemoteError as error:
                print(f"[cleanup] {host.alias}: {error}", file=sys.stderr)

    failures = [label for label, passed, _ in checks if not passed]
    print(f"\n{len(checks) - len(failures)}/{len(checks)} checks passed", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
