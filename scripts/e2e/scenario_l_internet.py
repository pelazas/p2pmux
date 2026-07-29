"""Scenario L -- first real cross-machine session: this Mac plus a DigitalOcean droplet.

Every earlier scenario ran both peers on one Mac, so the transport never left the
loopback path. This one mints a ticket here, carries it to a droplet in ams3 by ssh, and
asks the droplet to join over the public internet. It is the smallest test that can fail
for a network reason.

What it deliberately does NOT prove: the droplet has a public IP and no NAT, so a success
here says holepunching works in the easy case. The hard case (carrier NAT) needs a Mac on
a phone hotspot and is a hands-on test.

Run:  python3 scripts/e2e/scenario_l_internet.py
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

# The connectivity badge p2pmux draws right-aligned in the tab bar, e.g. `direct 35ms`
# or `relayed 120ms ×3`.
LINK_BADGE = re.compile(r"\b(direct|relayed|other)\b(?:\s+(\d+)ms)?(?:\s+×(\d+))?")

checks: list[tuple[str, bool, str]] = []


def link_badge(screen: str) -> str | None:
    """The path kind p2pmux is reporting, read off the rendered tab bar."""
    match = LINK_BADGE.search(screen.split("\n", 1)[0])
    return match.group(1) if match else None


def link_rtt_ms(screen: str) -> int | None:
    match = LINK_BADGE.search(screen.split("\n", 1)[0])
    return int(match.group(2)) if match and match.group(2) else None


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    """Send a p2pmux chord (Ctrl+P then a letter) like a user would."""
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    mark = "PASS" if passed else "FAIL"
    print(f"  [{mark}] {label}{(' -- ' + detail) if detail else ''}", flush=True)


def full_ticket(harness: Harness, code: str) -> str:
    """Ask the local binary for the pasteable ticket behind a short join code.

    The short code is a same-Mac rendezvous file, so it is meaningless on the droplet;
    the ticket is the thing that travels.
    """
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
    remote = RemoteHost()
    force_relay = "--force-relay" in sys.argv
    delay_ms = None
    for argument in sys.argv[1:]:
        if argument.startswith("--delay-ms="):
            delay_ms = int(argument.split("=", 1)[1])

    mode = "forced relay" if force_relay else "whatever path it gets"
    print(f"== scenario L: {remote.alias} ({mode}) ==", flush=True)
    if not remote.binary_ready():
        print(f"droplet has no release binary yet at {remote.binary}", file=sys.stderr)
        return 2

    baseline = remote.ping_ms()
    print(f"   icmp baseline to droplet: {baseline} ms", flush=True)
    remote.reset_network()
    remote.reset_home()
    if delay_ms is not None:
        remote.netem(delay_ms=delay_ms)
        print(f"   netem: +{delay_ms}ms one-way egress on the droplet", flush=True)
    if force_relay:
        # Applied before the join, so the direct path never succeeds even briefly.
        mac_ip = remote.local_public_ip()
        remote.block_direct_udp(mac_ip)
        print(f"   UDP to/from {mac_ip} dropped: direct path cannot form", flush=True)

    try:
        with Harness("scenario_l_internet") as harness:
            host, code = harness.create_room("mac")
            check("mac created a session", bool(code), f"join code {code}")

            ticket = full_ticket(harness, code)
            check("ticket minted for transport", len(ticket) > 20, f"{len(ticket)} chars")

            guest = harness.spawn(
                "droplet",
                ["join", ticket, "--name", "droplet"],
                launcher=remote.launcher(),
            )

            joined = True
            try:
                guest.wait_ready(timeout=45.0)
                guest.wait_for(r"Pane #\d+", timeout=60.0)
            except AssertionError as failure:
                joined = False
                check("droplet joined over the internet", False, str(failure)[:300])
            if joined:
                check("droplet joined over the internet", True, "shared layout rendered")

            if joined:
                # A joiner that has created nothing is invisible by design: the Mac's
                # screen names *pane hosts*, and presence-without-a-pane is Spike 4 work
                # that does not exist yet. So make the droplet host something real --
                # that is also the harder path, since the pane's PTY now lives in
                # Amsterdam and its frames must stream back here.
                chord(guest, CTRL_P, b"n")
                guest.settle(quiet_for=1.0, timeout=15.0)

                try:
                    host.wait_for(r"host: droplet", timeout=45.0)
                    check("droplet's pane reached the mac's layout", True)
                except AssertionError as failure:
                    check("droplet's pane reached the mac's layout", False, str(failure)[:300])

                # Bytes typed here execute in Amsterdam, on the droplet's own shell.
                marker = "L-INTERNET-OK"
                guest.run_in_shell(f"echo {marker}")
                try:
                    guest.wait_for(marker, timeout=30.0)
                    check("keystrokes reach the droplet's PTY", True)
                except AssertionError as failure:
                    check("keystrokes reach the droplet's PTY", False, str(failure)[:300])

                # The payoff assertion: a PTY frame produced in ams3, rendered here.
                try:
                    host.wait_for(marker, timeout=45.0)
                    check("droplet's PTY output streams to the mac", True)
                except AssertionError as failure:
                    check("droplet's PTY output streams to the mac", False, str(failure)[:300])

                # Spike 4A: both peers must now *say* which path they are on, and say the
                # same thing. Before this existed, every run above was "it worked" with no
                # way to know whether holepunching had actually happened.
                expected = "relayed" if force_relay else "direct"
                for peer in (host, guest):
                    try:
                        peer.wait_until(
                            lambda screen: link_badge(screen) == expected,
                            timeout=30.0,
                        )
                        check(
                            f"{peer.name} reports the path as {expected}",
                            True,
                            link_badge(peer.snapshot()) or "",
                        )
                    except AssertionError:
                        check(
                            f"{peer.name} reports the path as {expected}",
                            False,
                            f"badge said {link_badge(peer.snapshot())!r}",
                        )

                rtt = link_rtt_ms(host.snapshot())
                # netem shapes one-way egress, so it adds roughly its delay to the RTT.
                floor = (baseline or 0) + (delay_ms or 0)
                check(
                    "reported RTT is consistent with the measured path",
                    rtt is not None and rtt >= floor * 0.5,
                    f"badge {rtt}ms vs icmp {baseline}ms +{delay_ms or 0}ms shaping",
                )

                guest.settle(quiet_for=1.0, timeout=10.0)
                check("droplet peer still alive", guest.alive)
                check("mac peer still alive", host.alive)

            if not joined:
                print("\n--- droplet screen ---", flush=True)
                print(guest.snapshot(), flush=True)
                print("\n--- mac screen ---", flush=True)
                print(host.snapshot(), flush=True)
    finally:
        remote.reap()
        remote.reset_network()

    failures = [label for label, passed, _ in checks if not passed]
    print(f"\n{len(checks) - len(failures)}/{len(checks)} checks passed", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
