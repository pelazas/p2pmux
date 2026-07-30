"""Scenario N -- one session, two remote machines, panes and control going both ways.

Scenario L put this Mac on one side of the wire and a droplet on the other, and only the
droplet's pane ever streamed. That leaves the product's actual claim untested: *two*
people on *two* machines, each hosting their own terminals, each able to take control of
the other's. This scenario removes the Mac from the session entirely -- it holds two ssh
reins and nothing else -- and runs the whole thing between a droplet in nyc3 and a
droplet in fra1.

What each check is really asking:

  join            can a peer on another continent get into the room at all;
  path badge      did it holepunch, or is it on a relay, and do both sides agree;
  own panes       does each machine end up hosting a PTY the other one can see;
  locality        does `hostname` in a pane print the *pane host's* name, not the
                  typist's -- the whole "your keys stay on your machine" claim;
  cross-control   can each peer claim the other's pane and drive it;
  alongside       do two peers typing at the same moment both get through.

Run:  scripts/e2e/provision_droplets.sh create
      python3 scripts/e2e/scenario_n_two_remotes.py
      python3 scripts/e2e/scenario_n_two_remotes.py --force-relay
      python3 scripts/e2e/scenario_n_two_remotes.py --delay-ms=200
      scripts/e2e/provision_droplets.sh destroy

`--force-relay` drops UDP between the two droplets so holepunching cannot succeed and
the session has to fall back to a relay -- the path most real users behind carrier NAT
will actually get, and the one two public-IP droplets would otherwise never exercise.
`--delay-ms` shapes egress with netem, which widens every round-trip-shaped window in
the protocol; it is the cheap way to check that a race fixed at 85ms stays fixed at 300.
"""

from __future__ import annotations

import re
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteError, hosts_from_manifest, spawn_remote  # noqa: E402

CTRL_P = b"\x10"

COLS, ROWS = 120, 36

# Mirrors lease::IDLE_AFTER: how long a controller may stay quiet before the pane is
# claimable by somebody else.
IDLE_AFTER = 30.0

# `direct 85ms`, `relayed 140ms ×2`, `locked · direct <1ms`.
LINK_BADGE = re.compile(r"\b(direct|relayed|other)\b(?:\s+(<1|\d+)ms)?")

checks: list[tuple[str, bool, str]] = []


def check(label: str, passed: bool, detail: str = "") -> None:
    checks.append((label, passed, detail))
    mark = "PASS" if passed else "FAIL"
    print(f"  [{mark}] {label}{(' -- ' + detail) if detail else ''}", flush=True)


def guard(label: str, action, detail_on_success: str = "") -> bool:
    """Run an assertion-raising action and record it as one check."""
    try:
        action()
    except AssertionError as failure:
        check(label, False, str(failure)[:400])
        return False
    check(label, True, detail_on_success)
    return True


def link_badge(screen: str) -> str | None:
    match = LINK_BADGE.search(screen.split("\n", 1)[0])
    return match.group(1) if match else None


def chord(peer, lead: bytes, letter: bytes, settle: float = 1.5) -> None:
    peer.send(lead)
    time.sleep(0.35)
    peer.send(letter)
    time.sleep(settle)


def pane_body_cell(screen: str, host: str) -> tuple[int, int]:
    """A 1-based (col, row) inside the pane whose header reads `host: <host>`.

    Clicking is how a user picks a pane they do not own, and it is deliberately the
    focus-only gesture -- no control is claimed until a key is pressed. Locating the
    click by the header text rather than by a fixed coordinate keeps the scenario honest
    when the split axis or pane order changes.
    """
    for row, line in enumerate(screen.split("\n")):
        index = line.find(f"host: {host}")
        if index == -1:
            continue
        left = line.rfind("┌", 0, index)
        left = left if left != -1 else 0
        return left + 3, row + 3
    raise AssertionError(f"no pane hosted by {host!r} on screen:\n{screen}")


def pane_header(screen: str, host: str) -> str:
    for line in screen.split("\n"):
        if f"host: {host}" in line:
            return line
    return ""


def wait_for_free_pane(peer, host: str, timeout: float = IDLE_AFTER + 20.0) -> str:
    """Wait until the pane hosted by `host` reports no controller.

    p2pmux protects whoever is actively typing: a pane only becomes claimable again
    after about thirty seconds of controller silence (`lease::IDLE_AFTER`). A scenario
    that types the instant it wants the pane is testing nothing but its own impatience.
    """
    return peer.wait_until(
        lambda screen: f"host: {host} control: free" in screen,
        timeout=timeout,
        what=f"pane hosted by {host} to go free",
    )


def type_line_and_check(peer, other, command: str, expect: str, label: str) -> bool:
    """Type one command into the focused pane and hold it to every byte.

    Asserting on the command *as the remote shell echoed it back* is the point: a
    marker-only assertion passes even when the middle of the line was eaten, which is
    exactly the failure the claim-by-typing epoch race produced over a real link.
    """
    peer.run_in_shell(command)
    intact = guard(
        f"{label}: the whole line reached the pane's shell",
        lambda: peer.wait_for(re.escape(command), timeout=45.0),
        command,
    )
    guard(
        f"{label}: it ran on the pane host's machine",
        lambda: peer.wait_for(re.escape(expect), timeout=45.0),
        expect,
    )
    guard(
        f"{label}: the other member saw it too",
        lambda: other.wait_for(re.escape(expect), timeout=45.0),
    )
    return intact


def main() -> int:
    try:
        alpha_host, beta_host = hosts_from_manifest()[:2]
    except (RemoteError, ValueError) as error:
        print(error, file=sys.stderr)
        return 2

    force_relay = "--force-relay" in sys.argv
    delay_ms = None
    for argument in sys.argv[1:]:
        if argument.startswith("--delay-ms="):
            delay_ms = int(argument.split("=", 1)[1])

    mode = "forced relay" if force_relay else "whatever path it gets"
    if delay_ms is not None:
        mode += f", +{delay_ms}ms egress each way"
    print(f"== scenario N: {alpha_host.alias} + {beta_host.alias} ({mode}) ==", flush=True)
    for host in (alpha_host, beta_host):
        if not host.binary_ready():
            print(f"{host.alias} has no binary at {host.binary}", file=sys.stderr)
            return 2

    alpha_box = alpha_host.run("hostname").strip()
    beta_box = beta_host.run("hostname").strip()
    print(f"   coordinator on {alpha_box}, guest on {beta_box}", flush=True)

    for host in (alpha_host, beta_host):
        host.reset_network()
        host.reset_home()
    if delay_ms is not None:
        for host in (alpha_host, beta_host):
            host.netem(delay_ms=delay_ms)
    if force_relay:
        # Blocked on both sides and before the join, so the direct path never forms even
        # briefly. Filtering on the peer's address rather than on UDP as a whole leaves
        # every relay server reachable, which is the fallback under test.
        alpha_host.block_direct_udp(beta_host.host_ip())
        beta_host.block_direct_udp(alpha_host.host_ip())
        print("   UDP between the droplets dropped: only a relay path can form", flush=True)

    try:
        with Harness("scenario_n_two_remotes") as harness:
            alpha = spawn_remote(
                harness,
                alpha_host,
                "alpha",
                ["create", "--name", "alpha", "--session-name", "itest"],
                cols=COLS,
                rows=ROWS,
            )
            alpha.wait_ready(timeout=60.0)
            started = guard(
                "coordinator came up on a remote machine",
                lambda: alpha.wait_for(r"Pane #\d+", timeout=60.0),
                alpha_box,
            )
            if not started:
                print(alpha.snapshot(), flush=True)
                return 1

            # The ticket has to be minted where the coordinator lives. A short join code
            # would be useless here: it resolves through a cache on the machine that
            # created it, and beta is on another continent.
            ticket = alpha_host.cli("ticket").strip().splitlines()[-1].strip()
            check(
                "coordinator minted a portable ticket",
                ticket.startswith("p2pmux-v"),
                f"{len(ticket)} chars",
            )

            beta = spawn_remote(
                harness,
                beta_host,
                "beta",
                ["join", ticket, "--name", "beta"],
                cols=COLS,
                rows=ROWS,
            )
            beta.wait_ready(timeout=60.0)
            joined = guard(
                "second machine joined over the internet",
                lambda: beta.wait_for(r"Pane #\d+", timeout=90.0),
                f"{beta_box} -> {alpha_box}",
            )
            if not joined:
                print(beta.snapshot(), flush=True)
                return 1

            expected_path = "relayed" if force_relay else "direct"
            for peer in (alpha, beta):
                guard(
                    f"{peer.name} reports the path as {expected_path}",
                    lambda peer=peer: peer.wait_until(
                        lambda screen: link_badge(screen) == expected_path,
                        timeout=60.0,
                        what=f"a {expected_path} connectivity badge",
                    ),
                    link_badge(peer.snapshot()) or "",
                )
            check(
                "both machines agree on the path kind",
                link_badge(alpha.snapshot()) == link_badge(beta.snapshot()),
                f"alpha {link_badge(alpha.snapshot())!r} beta {link_badge(beta.snapshot())!r}",
            )

            # --- each machine hosts its own terminal ---------------------------------
            chord(beta, CTRL_P, b"n")
            beta.settle(quiet_for=1.0, timeout=20.0)
            guard(
                "guest's own pane appears on its screen",
                lambda: beta.wait_for(r"host: beta", timeout=30.0),
            )
            guard(
                "guest's own pane reaches the coordinator's layout",
                lambda: alpha.wait_for(r"host: beta", timeout=45.0),
            )

            # --- a process in each pane runs on that pane's machine -------------------
            alpha.click(*pane_body_cell(alpha.snapshot(), "alpha"))
            alpha.run_in_shell("echo N-ALPHA-$(hostname)")
            guard(
                "coordinator's shell ran on the coordinator's machine",
                lambda: alpha.wait_for(rf"N-ALPHA-{alpha_box}", timeout=30.0),
                alpha_box,
            )
            guard(
                "that output streamed to the guest",
                lambda: beta.wait_for(rf"N-ALPHA-{alpha_box}", timeout=45.0),
            )

            beta.click(*pane_body_cell(beta.snapshot(), "beta"))
            beta.run_in_shell("echo N-BETA-$(hostname)")
            guard(
                "guest's shell ran on the guest's machine",
                lambda: beta.wait_for(rf"N-BETA-{beta_box}", timeout=30.0),
                beta_box,
            )
            guard(
                "that output streamed to the coordinator",
                lambda: alpha.wait_for(rf"N-BETA-{beta_box}", timeout=45.0),
            )

            # --- an actively typing member is not shoved aside ------------------------
            # beta typed into its own pane a moment ago, so it holds that lease. alpha
            # asking for it now must change nothing: this is the rule that makes a shared
            # terminal usable at all.
            alpha.click(*pane_body_cell(alpha.snapshot(), "beta"))
            alpha.run_in_shell("echo N-STOLEN")
            time.sleep(4.0)
            check(
                "an active controller is not displaced",
                "control: beta" in pane_header(alpha.snapshot(), "beta")
                and "N-STOLEN" not in beta.snapshot(),
                pane_header(alpha.snapshot(), "beta").strip()[:120],
            )

            # --- typing into the other machine's pane ---------------------------------
            # The claim under test: bytes typed here execute over there, on that
            # machine's hardware, and everyone sees the result.
            guard(
                "the guest's pane frees itself after its controller goes quiet",
                lambda: wait_for_free_pane(alpha, "beta"),
                f"{IDLE_AFTER}s idle timeout",
            )
            alpha.click(*pane_body_cell(alpha.snapshot(), "beta"))
            type_line_and_check(
                alpha,
                beta,
                "echo N-CROSS-A2B-$(hostname)",
                f"N-CROSS-A2B-{beta_box}",
                "coordinator drives the guest's terminal",
            )
            header = pane_header(alpha.snapshot(), "beta")
            check(
                "guest's pane names the coordinator as controller",
                "control: alpha" in header,
                header.strip()[:120],
            )

            guard(
                "the coordinator's pane frees itself after its controller goes quiet",
                lambda: wait_for_free_pane(beta, "alpha"),
                f"{IDLE_AFTER}s idle timeout",
            )
            beta.click(*pane_body_cell(beta.snapshot(), "alpha"))
            type_line_and_check(
                beta,
                alpha,
                "echo N-CROSS-B2A-$(hostname)",
                f"N-CROSS-B2A-{alpha_box}",
                "guest drives the coordinator's terminal",
            )
            header = pane_header(beta.snapshot(), "alpha")
            check(
                "coordinator's pane names the guest as controller",
                "control: beta" in header,
                header.strip()[:120],
            )

            # --- working alongside each other -----------------------------------------
            # Each member now holds the *other* machine's pane, which is the arrangement
            # worth stress-testing: two people typing in the same instant, in opposite
            # directions, across the Atlantic. Serialised typing would not catch a
            # coordinator that applies one member's input while dropping another's.
            def type_marker(peer, marker: str) -> None:
                peer.run_in_shell(f"echo {marker}-$(hostname)")

            threads = [
                threading.Thread(target=type_marker, args=(alpha, "N-BOTH-A")),
                threading.Thread(target=type_marker, args=(beta, "N-BOTH-B")),
            ]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()

            # alpha is typing into beta's box and beta into alpha's, so the markers land
            # on the opposite hostnames from the ones that typed them.
            for peer in (alpha, beta):
                guard(
                    f"{peer.name} sees both members' simultaneous work",
                    lambda peer=peer: peer.wait_until(
                        lambda screen: f"N-BOTH-A-{beta_box}" in screen
                        and f"N-BOTH-B-{alpha_box}" in screen,
                        timeout=60.0,
                        what="both simultaneous markers",
                    ),
                )

            check("coordinator still alive", alpha.alive)
            check("guest still alive", beta.alive)

            if any(not passed for _, passed, _ in checks):
                print("\n--- alpha screen ---", flush=True)
                print(alpha.snapshot(), flush=True)
                print("\n--- beta screen ---", flush=True)
                print(beta.snapshot(), flush=True)
    finally:
        for host in (alpha_host, beta_host):
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
