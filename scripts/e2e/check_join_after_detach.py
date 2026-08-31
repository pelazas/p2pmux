#!/usr/bin/env python3
"""Issue #124: joining a detached local member attaches instead of adding another.

Run: python3 scripts/e2e/check_join_after_detach.py
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402


def wait_for_descriptor_count(read_descriptors, expected: int) -> None:
    deadline = time.monotonic() + 10
    descriptors = read_descriptors()
    while len(descriptors) != expected and time.monotonic() < deadline:
        time.sleep(0.1)
        descriptors = read_descriptors()
    assert len(descriptors) == expected, descriptors


def main() -> int:
    first_marker = "JOIN-AFTER-DETACH-FIRST-71D9"
    second_marker = "JOIN-AFTER-DETACH-SECOND-4C2E"

    # README dogfood: two terminals on one machine are a coordinator and a new
    # member. Only a detached member is reusable by a later `join`.
    with Harness("join-after-detach-same-home") as harness:
        _, ticket = harness.create_room("host")
        guest = harness.join_room("guest", ticket)
        wait_for_descriptor_count(harness.session_descriptors, 2)

        guest.key("ctrl_q")
        guest.type("d")
        deadline = time.monotonic() + 10
        while guest.alive and time.monotonic() < deadline:
            time.sleep(0.1)
        assert not guest.alive, "guest did not detach"

        harness.join_room("guest-rejoin", ticket)
        wait_for_descriptor_count(harness.session_descriptors, 2)

    # Separate HOMEs model the real two-machine report. A member keeps its
    # joined ticket after detaching, so it is found and reattached on rejoin.
    with Harness("join-after-detach-host") as host_harness, Harness(
        "join-after-detach-guest"
    ) as guest_harness:
        host, ticket = host_harness.create_room("host")
        guest = guest_harness.join_room("guest", ticket)

        host.run_in_shell(f"echo {first_marker}")
        guest.wait_for(first_marker, timeout=20)

        guest.key("ctrl_p")
        guest.type("n")
        time.sleep(0.5)
        guest.run_in_shell(f"echo {second_marker}")
        host.wait_for(second_marker, timeout=20)

        guest.key("ctrl_q")
        guest.type("d")
        deadline = time.monotonic() + 10
        while guest.alive and time.monotonic() < deadline:
            time.sleep(0.1)
        assert not guest.alive, "guest did not detach"

        rejoined = guest_harness.spawn("guest-rejoin", ["join", ticket, "--name", "guest"])
        rejoined.wait_ready(timeout=30)
        rejoined.wait_for(r"Pane #\d+", timeout=30)

        def session_descriptors() -> list[dict]:
            return host_harness.session_descriptors() + guest_harness.session_descriptors()

        wait_for_descriptor_count(session_descriptors, 2)

        screen = rejoined.wait_until(
            lambda current: first_marker in current and second_marker in current,
            timeout=20,
            what="both pane markers",
        )
        assert "waiting for pane snapshot" not in screen.lower(), screen

    print("join after detach: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
