#!/usr/bin/env python3
"""The whole story, on two real machines, recorded as it happens.

Not an assertion scenario. This drives the workflow a person actually walks --
pair two machines, wire an agent's hooks, share a session, watch the agent's
state cross the internet into the inbox -- and writes down every command, every
piece of stdout and every rendered frame, so the result can be read rather than
trusted.

The second machine is a DigitalOcean droplet stood up by `provision_droplets.sh`
(tag `p2pmux-itest`). This Mac is the coordinator; the droplet is the guest and
the machine the agent runs on, which is the direction that used to be broken.

Both sides run in a sandbox HOME. That matters more than usual here: `p2pmux
pair` writes a permanent pairing record, and a throwaway droplet has no business
in anybody's real fleet.

The agent is a stub named `claude` rather than Claude Code itself, for the
obvious licensing reason. It is not a mock of the reporting path: it is launched
as `exec -a claude`, which is what the process scanner matches on, and it reports
through `p2pmux notify claude --status ...` -- the same binary, socket and wire
message the installed hooks use. What the stub stands in for is the agent's
judgement about its own state, not the machinery that carries it.

Run: python3 scripts/e2e/workflow_agents_cross.py [output.md]
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import BINARY, Harness  # noqa: E402
from remote import MANIFEST_PATH, RemoteHost, spawn_remote  # noqa: E402

CTRL_O = b"\x0f"
CTRL_P = b"\x10"
CTRL_Q = b"\x11"
CTRL_F = b"\x06"

AGENT_DIR = "/root/agentwork"
MAC_NAME = "macbook"
DROPLET_NAME = "droplet-nyc3"

transcript: list[tuple[str, str, str]] = []
checks: list[tuple[str, bool, str]] = []


def note(title: str, body: str, kind: str = "text") -> None:
    """Record one step, and echo it so a live run is watchable."""
    transcript.append((title, kind, body.rstrip()))
    print(f"\n=== {title} ===\n{body.rstrip()}", flush=True)


def check(name: str, ok: bool, detail: str = "") -> None:
    checks.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" -- {detail}" if detail else ""), flush=True)


def mac(home: Path, args: str, timeout: float = 60.0) -> str:
    """A p2pmux command on this Mac, inside the run's sandbox HOME."""
    result = subprocess.run(
        [str(BINARY), *args.split()],
        env={
            "HOME": str(home),
            "TERM": "xterm-256color",
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        },
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return (result.stdout + result.stderr).strip()


def manifest_host() -> RemoteHost:
    manifest = json.loads(MANIFEST_PATH.read_text())
    entry = manifest["hosts"][0]
    return RemoteHost(
        alias=entry["alias"],
        hostname=entry["hostname"],
        identity=manifest["key_file"],
        binary_path=manifest["binary"],
        home="/root/workflow-home",
    )


def record(out: Path) -> int:
    remote = manifest_host()
    remote.reset_home()

    # ---------------------------------------------------------------- act one
    note("The two machines", "\n".join([
        f"$ doctl compute droplet list --tag-name p2pmux-itest",
        subprocess.run(
            ["doctl", "compute", "droplet", "list", "--tag-name", "p2pmux-itest",
             "--format", "Name,Region,Status,PublicIPv4"],
            capture_output=True, text=True, timeout=60).stdout.strip(),
    ]), kind="console")

    with Harness("workflow") as harness:
        home = harness.home

        # The display name is the first thing a fresh install asks for, and every
        # later surface — member labels, the machine strip, `p2pmux machines` —
        # is showing it back to you.
        mac(home, f"config set name {MAC_NAME}")
        remote.cli(f"config set name {DROPLET_NAME}")

        versions = "\n".join([
            "mac    $ p2pmux --version",
            "       " + mac(home, "--version"),
            f"{remote.alias} $ p2pmux --version",
            "       " + remote.cli("--version").strip(),
        ])
        note("Same build on both machines", versions, kind="console")
        check("both machines report the same version",
              mac(home, "--version").split()[-1] == remote.cli("--version").strip().split()[-1])
        # `--version` cannot tell work in progress from the release it sits on
        # top of: anything merged since the last tag ships under the tag's own
        # number, so both machines agree whether or not the droplet has it. Ask
        # it something only the code under test answers.
        droplet_probe = remote.cli("machines", timeout=60)
        check("the droplet is running the code under test, not the release under it",
              "No other machines paired yet" in droplet_probe, droplet_probe)

        # ---- #66: a fleet of one is still a fleet
        before = mac(home, "machines")
        note("`p2pmux machines` before anything is paired  (issue #66)", "\n".join([
            "mac $ p2pmux machines", before,
        ]), kind="console")
        check("the unpaired machine list still names this machine",
              "(this machine)" in before, before)
        check("and says what to do about the empty fleet",
              "p2pmux pair" in before, before)

        # ---- pairing
        offer = remote.cli("pair --accept-work", timeout=180)
        code = ""
        for line in offer.splitlines():
            if line.startswith("pairing code:"):
                code = line.split(":", 1)[1].strip()
        note(f"Pairing: the droplet offers a code", "\n".join([
            f"{remote.alias} $ p2pmux pair --accept-work",
            offer,
        ]), kind="console")
        check("the droplet minted a pairing code", bool(code), offer[-300:])
        if not code:
            raise SystemExit("no pairing code; cannot continue")

        accepted = mac(home, f"pair {code} --no-accept-work", timeout=180)
        note("Pairing: the Mac takes it", "\n".join([
            f"mac $ p2pmux pair {code} --no-accept-work",
            accepted,
        ]), kind="console")

        after = mac(home, "machines")
        note("`p2pmux machines` once they are paired", "\n".join([
            "mac $ p2pmux machines", after,
        ]), kind="console")
        check("the Mac now lists two machines", len(
            [line for line in after.splitlines()[1:] if line.strip()]) >= 2, after)
        check("and still marks which one is this one", "(this machine)" in after, after)

        # ---------------------------------------------------------- act two
        note("The agent, before its hooks are wired", "\n".join([
            f"{remote.alias} $ p2pmux doctor", remote.cli("doctor", timeout=60),
        ]), kind="console")

        # A stand-in for Claude Code: detected the same way, reports the same
        # way. Each step is the exact command the hook `setup` just registered,
        # with the JSON Claude Code pipes into it on stdin — not a `--status`
        # flag on its own, which is a shortcut the real agent never takes and
        # which would skip the payload rules that decide what reaches the badge.
        remote.run(f"mkdir -p {AGENT_DIR} && mkdir -p /root/workflow-home/.claude")
        remote.run(
            "cat > /root/agent.sh <<'SH'\n"
            "#!/bin/bash\n"
            "hook() { echo \"$2\" | " + remote.binary + " notify claude --status \"$1\"; }\n"
            "echo 'agent: picked up a task'\n"
            'hook running \'{"hook_event_name":"UserPromptSubmit","cwd":"' + AGENT_DIR + '"}\'\n'
            "sleep 25\n"
            "# A Notification carrying only Claude\\'s placeholder text. It says\n"
            "# nothing about what is being asked, and the inbox drops it on\n"
            "# purpose rather than train you to ignore the state that matters.\n"
            "echo 'agent: (generic notification)'\n"
            'hook pending \'{"hook_event_name":"Notification","message":"Claude needs attention","cwd":"'
            + AGENT_DIR + '"}\'\n'
            "sleep 25\n"
            "echo 'agent: needs a human to approve a command'\n"
            'hook pending \'{"hook_event_name":"Notification","message":"Claude needs your permission to run: cargo test --release","cwd":"'
            + AGENT_DIR + '"}\'\n'
            "sleep 900\n"
            "SH\n"
            "chmod +x /root/agent.sh"
        )

        setup = remote.cli("setup claude", timeout=60)
        note("`p2pmux setup claude` wires the hooks  (agent setup)", "\n".join([
            f"{remote.alias} $ p2pmux setup claude", setup,
        ]), kind="console")

        settings = remote.run(
            "cat /root/workflow-home/.claude/settings.json 2>/dev/null || echo '(no settings file)'",
            check=False)
        note("What it wrote into `~/.claude/settings.json`", settings.strip(), kind="json")
        check("the hooks landed in the agent's settings file",
              "p2pmux" in settings and "PreToolUse" in settings, settings[:200])

        doctor = remote.cli("doctor", timeout=60)
        note("`p2pmux doctor` after wiring", "\n".join([
            f"{remote.alias} $ p2pmux doctor", doctor,
        ]), kind="console")

        # ---------------------------------------------------------- act three
        host, ticket = harness.create_room(MAC_NAME, cols=110, rows=34, timeout=60)
        host.wait_for(r"Pane #\d+", timeout=60)
        note("The Mac's session, freshly created", host.settle(quiet_for=1.0, timeout=20),
             kind="screen")

        guest = spawn_remote(harness, remote, DROPLET_NAME,
                             ["join", ticket, "--name", DROPLET_NAME], cols=110, rows=34)
        guest.wait_ready(timeout=90)
        guest.wait_for(r"Pane #\d+", timeout=90)
        note(f"{remote.alias} joined, seen from the droplet",
             guest.settle(quiet_for=1.0, timeout=20), kind="screen")

        # The droplet hosts a pane of its own; the agent runs in it.
        guest.send(CTRL_P)
        time.sleep(0.5)
        guest.send(b"n")
        time.sleep(5.0)
        check("the droplet hosts a pane of its own", "Pane #2" in guest.snapshot())

        guest.send(f"bash -c 'cd {AGENT_DIR} && exec -a claude /bin/bash /root/agent.sh'\r".encode())
        guest.wait_for("picked up a task", timeout=45)
        note("The agent, running on the droplet", guest.settle(quiet_for=1.0, timeout=20),
             kind="screen")

        # `snapshot()` rather than `settle()` from here on. Settling waits for the
        # pane to go quiet, and the pane is an agent on a timer: one settle call
        # sailed clean over every phase this section exists to look at.
        # Detection samples the process tree on a timer; the hook lands sooner.
        time.sleep(14)
        bar = host.snapshot().splitlines()[0]
        note("The Mac's tab bar while the droplet's agent works  (issue #67)", bar, kind="screen")
        check("the badge sits centred in the cell its width reserves",
              "│  inbox  │" in bar, bar)

        # The placeholder Notification goes by without lighting anything, which
        # is the whole reason `derive_claude` filters it: a badge that fires for
        # "Claude needs attention" teaches you to ignore the badge.
        guest.wait_for(r"generic notification", timeout=90)
        time.sleep(9)
        bar_generic = host.snapshot().splitlines()[0]
        note("A Notification with nothing in it does not raise the badge", bar_generic,
             kind="screen")
        check("a placeholder notification is not treated as needing you",
              "inbox 1" not in bar_generic, bar_generic)

        guest.wait_for("approve a command", timeout=120)
        time.sleep(12)
        bar_blocked = host.snapshot().splitlines()[0]
        note("…and one that says what it is asking does  (issue #67)", bar_blocked,
             kind="screen")
        check("the blocked agent reaches the coordinator's badge",
              "inbox 1" in bar_blocked, bar_blocked)
        check("the count fills the same cell, so the bar never shifts",
              "│ inbox 1 │" in bar_blocked, bar_blocked)

        host.send(CTRL_O)
        time.sleep(2.0)
        inbox = host.settle(quiet_for=1.0, timeout=20)
        note("The inbox on the Mac: an agent on another machine  (issues #66, #67)",
             inbox, kind="screen")
        check("the droplet's agent has a row in the Mac's inbox",
              "droplet" in inbox and "claude" in inbox, inbox[-400:])
        # The strip is the last non-blank line above the key bar.
        strip = [line for line in inbox.splitlines()[:-1] if line.strip()][-1]
        check("the machine strip lists both machines  (issue #66)",
              "droplet" in strip and "mac" in strip, strip)

        # ---- #65: the badge is a door with a handle on both sides
        # SGR mouse coordinates are 1-based, so a screen column c is column c+1
        # and the tab bar -- screen row 0 -- is row 1.
        badge_column = bar_blocked.index("inbox") + 3
        host.click(badge_column, 1)
        time.sleep(1.5)
        back = host.settle(quiet_for=1.0, timeout=20)
        note("Clicking the badge a second time returns to the tabs  (issue #65)",
             back, kind="screen")
        check("a second click on the badge leaves the inbox",
              "Pane #1" in back, back[:300])

        # ---- #64: zoom
        host.send(CTRL_P)
        time.sleep(0.5)
        host.send(b"n")
        time.sleep(4.0)
        two_panes = host.settle(quiet_for=1.0, timeout=20)
        check("the Mac has two panes to zoom between", two_panes.count("Pane #") >= 2)

        host.send(CTRL_P)
        time.sleep(0.5)
        host.send(b"z")
        time.sleep(1.5)
        zoomed = host.settle(quiet_for=1.0, timeout=20)
        note("`Ctrl+P` `z` gives the focused pane the screen  (issue #64)", zoomed, kind="screen")
        check("zoom hides the siblings", zoomed.count("Pane #") == 1, zoomed[:200])
        check("and says so on the border", "zoom" in zoomed, zoomed[:400])

        host.send(CTRL_P)
        time.sleep(0.5)
        host.send(b"z")
        time.sleep(1.5)
        unzoomed = host.settle(quiet_for=1.0, timeout=20)
        check("and gives the space back", unzoomed.count("Pane #") >= 2)

        # ---- #59: Ctrl+F passes through pane mode
        host.send(CTRL_P)
        time.sleep(0.8)
        in_mode = host.snapshot()
        check("pane mode is up", "PANE MODE" in in_mode)
        host.send(CTRL_F)
        time.sleep(0.7)
        after_ctrl_f = host.snapshot()
        note("Ctrl+F inside PANE MODE  (issue #59)",
             "\n".join(after_ctrl_f.splitlines()[-1:]), kind="screen")
        check("Ctrl+F closes no pane", after_ctrl_f.count("Pane #") == unzoomed.count("Pane #"),
              f"{after_ctrl_f.count('Pane #')} vs {unzoomed.count('Pane #')}")
        check("and no longer takes pane mode with it", "PANE MODE" in after_ctrl_f,
              after_ctrl_f.splitlines()[-1])
        time.sleep(2.6)
        check("the idle timeout still clears the mode",
              "PANE MODE" not in host.snapshot(), host.snapshot().splitlines()[-1])

        # ---- #63: the quit prompt
        host.send(CTRL_Q)
        time.sleep(1.5)
        prompt = host.settle(quiet_for=1.0, timeout=20)
        note("Ctrl+Q asks which leaving was meant  (issue #63)", prompt, kind="screen")
        check("the prompt names both answers",
              "Leave this session?" in prompt and "detach" in prompt and "kill" in prompt,
              prompt[:400])

        host.send(b"\x1b")
        time.sleep(1.0)
        check("Esc backs out of it", "Leave this session?" not in host.snapshot())

        # Detach the guest cleanly, then kill the Mac's session from the prompt.
        guest.send(CTRL_Q)
        time.sleep(0.6)
        guest.send(b"d")
        time.sleep(4.0)
        note("The droplet's session survives its client detaching", "\n".join([
            f"{remote.alias} $ p2pmux ls", remote.cli("ls", timeout=30).strip(),
        ]), kind="console")

        host.send(CTRL_Q)
        time.sleep(0.8)
        host.send(b"k")
        time.sleep(5.0)
        killed = mac(home, "ls")
        note("…and `k` ends the Mac's for good", "\n".join([
            "mac $ p2pmux ls", killed or "(no live sessions)",
        ]), kind="console")
        # Only the shared session is gone. `p2pmux pair` left a session of its
        # own behind on this machine -- that is what being paired *is* -- and
        # killing the one you were looking at must not take it with it.
        check("kill removed the session that was killed",
              not any(line.split()[:1] == [MAC_NAME] for line in killed.splitlines()[1:] if line.strip()),
              killed)

        remote.reap()

    passed = sum(1 for _, ok, _ in checks if ok)
    write_report(out, passed)
    print(f"\n{passed}/{len(checks)} checks passed -- report: {out}", flush=True)
    return 0 if passed == len(checks) else 1


def main() -> int:
    """Run the workflow, and write the report whatever happens to it.

    A run that dies halfway has still recorded everything up to the point it
    died, and that transcript is the most useful thing in the room when it does.
    """
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("workflow.md")
    try:
        return record(out)
    except BaseException as error:
        check("the run reached the end", False, f"{type(error).__name__}: {error}")
        note("Where it stopped", f"{type(error).__name__}: {error}", kind="text")
        write_report(out, sum(1 for _, ok, _ in checks if ok))
        print(f"\nrun failed; partial report: {out}", flush=True)
        raise


def write_report(out: Path, passed: int) -> None:
    lines = [
        "# p2pmux across two machines: the whole workflow",
        "",
        "Recorded live by `scripts/e2e/workflow_agents_cross.py`. Every block below is real "
        "output from a real run: this Mac as coordinator, a DigitalOcean droplet as the guest "
        "and the machine the agent runs on.",
        "",
        f"**{passed}/{len(checks)} checks passed.**",
        "",
        "## Checks",
        "",
        "| | check |",
        "|---|---|",
    ]
    for name, ok, detail in checks:
        mark = "✅" if ok else "❌"
        suffix = "" if ok else f"<br><sub>{detail[:160]}</sub>"
        lines.append(f"| {mark} | {name}{suffix} |")
    lines += ["", "## Transcript", ""]
    fence = {"console": "console", "screen": "text", "json": "json", "text": ""}
    for title, kind, body in transcript:
        lines += [f"### {title}", "", f"```{fence.get(kind, '')}", body, "```", ""]
    out.write_text("\n".join(lines))


if __name__ == "__main__":
    raise SystemExit(main())
