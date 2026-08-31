#!/usr/bin/env python3
"""Category A: when does a watching peer get told an agent finished?

The completion notification used to fire on any two seconds of pane silence, so a
real agent -- which pauses for much longer than that while waiting on a model
response or running a quiet tool -- announced completion repeatedly mid-task. This
drives a fake agent that pauses exactly like a real one and asserts the notification
count directly, rather than trusting the state machine to be described correctly.

Agent state is now hooks-only: nothing is inferred from output or from timing. The
fake agent therefore reports through `p2pmux notify`, the same path a real Claude
Code registration uses. An earlier version of this scenario rang the terminal bell
instead, which the old inference read as "done" -- a bell is just a byte in the
stream now, so that version asserted a mechanism the product no longer has.

The count is read from the UI debug log (`P2PMUX_DEBUG_UI`), where the client
records every announcement.

Run: python3 scripts/e2e/scenario_k_agent_notify.py [repeats]
"""

import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import (  # noqa: E402
    BINARY,
    DeadlineExceeded,
    Harness,
    agent_panel,
    p2pmux_pids,
    orphans_after,
)

CTRL_P = b"\x10"

# Long enough that the old two-second window would have fired on each one, short
# enough to keep a run under a minute. Must stay below the 20s default quiet window.
PAUSE_SECONDS = 6

# The pause before the completion is load-bearing: without it the completion lands in
# the same instant as the last step, and a sample taken "mid-task" would already include
# its announcement and report a false failure.
#
# The agent reports through `p2pmux notify`, not by ringing the terminal bell. Agent
# state is hooks-only: nothing is inferred from output or from timing, so a bell is just
# a byte in the stream now and a fake agent that only rings one is never "done". Hooks
# are what a real Claude Code registration sends, so this is also the path users run.
FAKE_AGENT = """
notify() {{ printf '{{}}' | {binary} notify claude --status "$1" >/dev/null 2>&1 || true; }}
echo AGENT-START
notify running
sleep {pause}
echo AGENT-STEP-1
sleep {pause}
echo AGENT-STEP-2
sleep {pause}
notify done
echo AGENT-DONE
sleep 300
""".strip()


def install_fake_agent(home: Path) -> str:
    """Write a script that pauses like a real agent, and the command that runs it as `claude`.

    Agent detection matches the process's own name, so the script cannot simply be
    called `claude`: a shebang script is reported as its interpreter (`sh`, `python3`)
    and is never classified. Copying /bin/sh under the name `claude` does not work
    either -- macOS kills a copy of a signed platform binary. Running the real
    /bin/sh with `exec -a` gives the process the right argv[0] with no such problem.
    """
    bindir = home / "fakebin"
    bindir.mkdir(parents=True, exist_ok=True)
    script = bindir / "agent.sh"
    script.write_text(FAKE_AGENT.format(pause=PAUSE_SECONDS, binary=BINARY) + "\n")
    return f"/bin/sh -c 'exec -a claude /bin/sh {script}'"


def peer_env(log: Path) -> dict[str, str]:
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "P2PMUX_DEBUG_UI": str(log),
    }


def announcements(log: Path) -> int:
    """How many completion announcements the client has made so far."""
    if not log.exists():
        return 0
    return sum(1 for line in log.read_text(errors="replace").split("\n") if "agent_completion" in line)


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail else ""))

    with Harness(f"k-agent-notify-{index}") as harness:
        agent_command = install_fake_agent(harness.home)
        guest_log = harness.home / "guest-ui.log"
        host_log = harness.home / "host-ui.log"

        host = harness.spawn(
            "host",
            # Pinned so `wait_for_ticket("host")` below has a name to match; `create`
            # otherwise names the session after a random world city.
            ["create", "--name", "host", "--session-name", "host"],
            env=peer_env(host_log),
        )
        host.wait_ready(timeout=25)
        ticket = harness.wait_for_ticket("host", timeout=25)

        guest = harness.spawn(
            "guest",
            ["join", ticket, "--name", "guest"],
            env=peer_env(guest_log),
        )
        guest.wait_ready(timeout=30)
        guest.wait_for(r"Pane #\d+", timeout=30)

        # The guest must not be looking at the agent's pane, or its notification is
        # suppressed by design and the scenario would pass without testing anything.
        guest.send(CTRL_P)
        time.sleep(0.3)
        guest.send(b"n")
        time.sleep(2.0)
        check(
            "guest owns a second pane and is focused on it",
            "Pane #2" in guest.snapshot(),
            guest.snapshot()[:200],
        )

        # The agent starts in the host's pane and is detected within a sampler tick.
        host.run_in_shell(agent_command)
        try:
            host.wait_for("AGENT-START", timeout=15)
            guest.wait_for("AGENT-START", timeout=15)
            check("the agent's output reaches the watching guest", True)
        except (DeadlineExceeded, AssertionError) as error:
            check("the agent's output reaches the watching guest", False, str(error)[:300])

        # Mid-task pauses. Each one is longer than the old window and shorter than the
        # new one, so a correct implementation stays silent through all of them.
        guest.wait_for("AGENT-STEP-1", timeout=PAUSE_SECONDS + 12)
        first = announcements(guest_log)
        guest.wait_for("AGENT-STEP-2", timeout=PAUSE_SECONDS + 12)
        second = announcements(guest_log)
        # Sampled inside the final pause, while the agent is quiet but has not yet rung.
        time.sleep(PAUSE_SECONDS - 2)
        third = announcements(guest_log)
        check(
            "a mid-task pause is not announced as a completion",
            first == 0 and second == 0 and third == 0,
            f"samples after each pause: {first}, {second}, {third}",
        )

        # The `done` hook is the agent saying it has finished, so the announcement must
        # not wait out the silence window.
        guest.wait_for("AGENT-DONE", timeout=15)
        reported_at = time.monotonic()
        while announcements(guest_log) == 0 and time.monotonic() - reported_at < 12:
            time.sleep(0.5)
        after_done = announcements(guest_log)
        check(
            "the done hook announces the completion",
            after_done == 1,
            f"{after_done} announcements, {time.monotonic() - reported_at:.1f}s after the hook",
        )

        # The agent repaints after ringing and then goes quiet for longer than the
        # silence window. Neither may produce a second announcement for the same work.
        time.sleep(25)
        settled = announcements(guest_log)
        check(
            "the completion is announced exactly once",
            settled == 1,
            f"{settled} announcements after settling",
        )

        # Looking at the pane clears the unread star; it must not re-arm the sound.
        guest.send(CTRL_P)
        time.sleep(0.3)
        guest.send(b"\x1b[D")
        time.sleep(3.0)
        check(
            "focusing the finished pane does not re-announce it",
            announcements(guest_log) == 1,
            f"{announcements(guest_log)} announcements after focusing",
        )

        # The host has the agent's pane focused, so it is suppressed there by design.
        check(
            "the peer viewing the pane is not notified",
            announcements(host_log) == 0,
            f"{announcements(host_log)} announcements on the host",
        )

        # Last, because it needs the overlay: confirm the classification the whole
        # scenario depends on, so a detection failure is not reported as a timing bug.
        host.send(b"\x01")
        time.sleep(1.5)
        # Scoped to the panel: the pane underneath is running `exec -a claude ...`, so
        # searching the whole screen would match the shell's own command line and pass
        # whether or not detection fired. The overlay prints the lowercase kind, not the
        # display label. See the panel drawing in USAGE.md.
        overlay = host.snapshot()
        panel = agent_panel(overlay)
        check(
            "the fake agent is classified as a claude agent",
            "claude" in panel,
            overlay[:400],
        )

        for peer in (host, guest):
            noise = [
                line
                for line in peer.raw_text().split("\n")
                if "panicked at" in line or "RUST_BACKTRACE" in line
            ]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:300])

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    all_runs: list[list[tuple[str, bool, str]]] = []

    print(f"Scenario K: agent completion is announced once, when the agent finishes ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        all_runs.append(run_once(index, verbose=(repeats == 1)))

    names = [name for name, _, _ in all_runs[0]]
    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, name in enumerate(names):
        failures = sum(1 for run in all_runs if not run[position][1])
        if failures:
            any_failure = True
            print(f"    {failures}/{len(all_runs)}  FAIL  {name}")
        else:
            print(f"    0/{len(all_runs)}  pass  {name}")

    leaked = orphans_after(baseline)
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
