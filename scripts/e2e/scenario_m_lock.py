"""Scenario M -- lock the door on a live cross-machine session.

Builds on scenario L: this Mac hosts, a droplet peer joins over the public internet, the
host presses Ctrl+P then Shift+L, and a *second* droplet peer -- holding the same valid
ticket -- is turned away. Then the host unlocks and that same peer walks in.

Why the second peer is remote too: a locked door has to hold against someone arriving over
the real network, not against a loopback connection that never left the Mac. The two
droplet peers get separate sandbox HOMEs so neither can see the other's session store.

What this test is really checking is that the refusal is *specific*. A joiner told only
"connection lost" cannot tell a locked session from a crashed one, so the assertion is on
the words the rejected peer prints, not merely on its failure.

Run:  python3 scripts/e2e/scenario_m_lock.py
"""

from __future__ import annotations

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

CTRL_P = b"\x10"

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    print(f"  [{'PASS' if passed else 'FAIL'}] {label}{(' -- ' + detail) if detail else ''}", flush=True)


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def full_ticket(harness: Harness, code: str) -> str:
    result = subprocess.run(
        [str(BINARY), "ticket", code],
        capture_output=True,
        text=True,
        timeout=20,
        env={"HOME": str(harness.home), "PATH": "/usr/bin:/bin"},
    )
    if result.returncode != 0:
        raise AssertionError(f"`p2pmux ticket {code}` failed:\n{result.stderr.strip()}")
    return result.stdout.strip()


def main() -> int:
    insider = RemoteHost(home="/root/e2e-home")
    outsider = RemoteHost(home="/root/e2e-home-2")

    print(f"== scenario M: session lock over {insider.alias} ==", flush=True)
    if not insider.binary_ready():
        print(f"droplet has no release binary at {insider.binary}", file=sys.stderr)
        return 2

    insider.reset_network()
    insider.reset_home()
    outsider.run(f"rm -rf {outsider.home} && mkdir -p {outsider.home}")

    try:
        with Harness("scenario_m_lock") as harness:
            host, code = harness.create_room("mac")
            ticket = full_ticket(harness, code)
            check("mac created a session", bool(code), f"join code {code}")

            guest = harness.spawn(
                "insider",
                ["join", ticket, "--name", "insider"],
                launcher=insider.launcher(),
            )
            guest.wait_ready(timeout=45.0)
            guest.wait_for(r"Pane #\d+", timeout=60.0)
            check("first droplet peer joined while open", True)

            # Ctrl+P then Shift+L. Deliberately a different key from the lowercase `k`
            # pane lock, which locks one pane rather than the session.
            chord(host, CTRL_P, b"L", settle=2.0)
            try:
                host.wait_for(r"session locked", timeout=15.0)
                check("host reports the session as locked", True)
            except AssertionError as failure:
                check("host reports the session as locked", False, str(failure)[:250])

            try:
                host.wait_until(lambda screen: "locked" in screen.split("\n", 1)[0], timeout=15.0)
                check("locked badge is drawn in the tab bar", True)
            except AssertionError:
                check(
                    "locked badge is drawn in the tab bar",
                    False,
                    repr(host.snapshot().split("\n", 1)[0][:120]),
                )

            # The stranger holds a fully valid ticket. Only the lock should stop it.
            stranger = harness.spawn(
                "stranger",
                ["join", ticket, "--name", "stranger"],
                launcher=outsider.launcher(),
            )
            try:
                stranger.wait_for(r"locked", timeout=45.0)
                check(
                    "stranger is refused and told the session is locked",
                    True,
                    "refusal names the lock",
                )
            except AssertionError:
                check(
                    "stranger is refused and told the session is locked",
                    False,
                    repr(stranger.snapshot()[-400:]),
                )

            stranger.settle(quiet_for=1.0, timeout=20.0)
            check(
                "stranger never reached the shared layout",
                not re.search(r"Pane #\d+", stranger.snapshot()),
                "a refused peer must not render panes",
            )

            # The peer already inside must be untouched by the lock.
            marker = "M-INSIDER-STILL-WORKING"
            guest.run_in_shell(f"echo {marker}")
            try:
                guest.wait_for(marker, timeout=30.0)
                check("locking did not evict the peer already inside", True)
            except AssertionError as failure:
                check("locking did not evict the peer already inside", False, str(failure)[:250])

            # The other lock, and the other reading of "lock the screen": Ctrl+P then
            # lowercase `k` locks one pane against everyone but its own host. It has
            # existed for a while but has only ever been exercised on loopback, where a
            # refusal and a slow network look identical.
            chord(host, CTRL_P, b"k", settle=2.0)
            try:
                host.wait_for(r"locked by mac", timeout=20.0)
                check("mac's pane shows it is locked", True)
            except AssertionError as failure:
                check("mac's pane shows it is locked", False, str(failure)[:250])

            blocked = "M-SHOULD-NOT-APPEAR"
            guest.run_in_shell(f"echo {blocked}")
            guest.settle(quiet_for=1.5, timeout=20.0)
            host.settle(quiet_for=1.5, timeout=20.0)
            check(
                "a locked pane refuses a remote peer's keystrokes",
                blocked not in host.snapshot(),
                "the host's own pane must not run what a guest typed into it",
            )

            chord(host, CTRL_P, b"k", settle=2.0)
            allowed = "M-ALLOWED-AGAIN"
            guest.run_in_shell(f"echo {allowed}")
            try:
                host.wait_for(allowed, timeout=30.0)
                check("unlocking the pane lets the remote peer type again", True)
            except AssertionError as failure:
                check(
                    "unlocking the pane lets the remote peer type again",
                    False,
                    str(failure)[:250],
                )

            # Unlock, and the same stranger should now get in.
            chord(host, CTRL_P, b"L", settle=2.0)
            try:
                host.wait_for(r"session unlocked", timeout=15.0)
                check("host reports the session as unlocked", True)
            except AssertionError as failure:
                check("host reports the session as unlocked", False, str(failure)[:250])

            stranger.close()
            readmitted = harness.spawn(
                "readmitted",
                ["join", ticket, "--name", "readmitted"],
                launcher=outsider.launcher(),
            )
            try:
                readmitted.wait_ready(timeout=45.0)
                readmitted.wait_for(r"Pane #\d+", timeout=60.0)
                check("the same peer joins once the host unlocks", True)
            except AssertionError as failure:
                check("the same peer joins once the host unlocks", False, str(failure)[:250])

            check("host still alive", host.alive)
            check("insider still alive", guest.alive)
    finally:
        insider.reap()
        insider.reset_network()

    failures = [label for label, passed, _ in checks if not passed]
    print(f"\n{len(checks) - len(failures)}/{len(checks)} checks passed", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
