"""Scenario AF -- a machine nobody is looking at is not watching your pane.

Presence answers one question: where is everyone. A member that has a terminal
open has an answer; a member that is a node and nothing else does not, and the
protocol has always said so -- `attached: false`, location zeroed. Nothing ever
sent it. Every node asserted it was attached and named whatever pane its layout
started on, so a droplet that joined a session and was never opened put a dot on
the coordinator's tab bar and lit one of its pane borders as watched.

This is the two-machine version of that, because the one-machine version cannot
fail the same way: the interesting member is a real node on another box, joined
over the network, whose client has gone.

  attached      the droplet, with its terminal open, is drawn as watching --
                otherwise this test would pass on a build where presence never
                arrives at all;
  detached      the droplet leaves its node running and closes its terminal.
                The panes stay up. The dot goes away.

The second half is exactly the state a machine that followed a fleet invitation
is in permanently: a node, no terminal, nobody there.

Run:  python3 scripts/e2e/scenario_af_unattended_presence.py

It needs `mybotvm` reachable over ssh with a release binary built there, and a
release binary here. Both sides run inside sandbox HOMEs.
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost  # noqa: E402

# Overridable so the same scenario can be pointed at an older build on the
# droplet, which is how you check that this test can still fail. Never point it
# at a binary the machine's real sessions are running: the harness kills every
# process started from the path it is given.
REMOTE_BINARY = os.environ.get(
    "P2PMUX_AF_REMOTE_BINARY", "/home/pelazas/p2pmux-src/target/release/p2pmux"
)
REMOTE_HOME = os.environ.get("P2PMUX_AF_REMOTE_HOME", "/home/pelazas/p2pmux-af-home")

# The glyph the tab bar draws for a member watching a tab. Kept in step with
# `PRESENCE_WATCHING` in src/tui/render/panes.rs by this comment and nothing
# else, which is the right trade for a screen-scraping test: if the glyph
# changes, this fails loudly rather than passing vacuously.
WATCHING = "●"


def section(title: str) -> None:
    print(f"\n== {title}")


def step(message: str) -> None:
    print(f"   {message}")


def tab_bar(peer) -> str:
    """The first row of the rendered screen, where tabs and their dots live."""
    return peer.snapshot().splitlines()[0]


def wait_for_tab_bar(peer, wanted: bool, timeout: float = 30.0) -> str:
    """Wait until the tab bar does or does not carry a watcher dot."""
    deadline = time.monotonic() + timeout
    row = ""
    while time.monotonic() < deadline:
        row = tab_bar(peer)
        if (WATCHING in row) == wanted:
            return row
        time.sleep(0.5)
    return row


def main() -> int:
    remote = RemoteHost(
        alias="mybotvm",
        home=REMOTE_HOME,
        binary_path=REMOTE_BINARY,
    )
    section("preflight")
    step(f"droplet binary: {remote.run(f'{REMOTE_BINARY} --version').strip()}")
    remote.reset_home()

    with Harness("scenario-af-presence") as harness:
        section("a session on this Mac")
        host, ticket = harness.create_room("mac")
        step("created a session with one pane")
        assert WATCHING not in tab_bar(host), (
            "nobody else is here yet, so nothing may be drawn as watching:\n"
            f"{tab_bar(host)}"
        )

        section("the droplet joins, with somebody at it")
        droplet = harness.spawn(
            "droplet",
            ["join", ticket, "--name", "droplet"],
            launcher=remote.launcher(),
        )
        droplet.wait_for(r"Pane #\d+", timeout=90.0)
        step("the droplet has the shared layout on screen")
        row = wait_for_tab_bar(host, wanted=True, timeout=60.0)
        assert WATCHING in row, (
            "a member with its terminal open must be drawn as watching a tab; "
            f"the tab bar says:\n{row}"
        )
        step("the Mac draws the droplet as watching")

        section("the droplet's terminal goes away, its node stays")
        # Closing the client is a detach and nothing more: the node -- and every
        # pane on it -- keeps running, which is the whole point. Killing the
        # node would be the droplet leaving, and would prove nothing about
        # presence.
        droplet.close()
        step("detached: the droplet's node is still in the session")

        row = wait_for_tab_bar(host, wanted=False, timeout=60.0)
        assert WATCHING not in row, (
            "a node with no terminal on it is looking at nothing, and must not "
            f"be drawn watching a pane; the tab bar says:\n{row}"
        )
        step("the dot is gone, and the droplet's panes are still there")

        # The session itself must have survived the detach, or the assertion
        # above passes for the wrong reason entirely.
        sessions = remote.cli("list", timeout=60.0)
        assert "mac" in sessions or "member" in sessions, (
            f"the droplet's node should still be in the session:\n{sessions}"
        )
        step(f"droplet still lists it:\n      {sessions.strip()}")

    remote.reap()
    print("\nscenario AF: every check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
