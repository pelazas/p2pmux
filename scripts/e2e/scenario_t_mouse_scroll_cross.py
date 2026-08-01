#!/usr/bin/env python3
"""Mouse forwarding and scrollback into a pane hosted on another machine.

Two documented claims that only ever had single-machine coverage:

  USAGE.md, mouse: "When the focused pane's program turns on xterm mouse reporting ...
  clicks, drags, and the wheel go to that program instead". Scenario F proves that with
  both peers on one Mac. Here the *program* is on Linux and the *click* is on macOS, so
  the report has to survive the whole path and arrive with pane-relative coordinates.

  USAGE.md, scrollback: "Remote-hosted panes return an empty scrollback window in this v1
  implementation." A stated limitation, so the pass condition is that it matches the doc
  -- and above all is not a hang or a corrupted view.

Both hold. The value here is the regression net, and two traps worth keeping:
a pane parked in history renders a frozen viewport, so anything typed into it runs
invisibly and looks exactly like a broken input path; and mouse reports must be asserted
against the *rendered screen*, since escape sequences split the text in the raw stream.

Requires the `p2pmux-lab` droplet (see remote.py).

Run: python3 scripts/e2e/scenario_t_mouse_scroll_cross.py
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import RemoteHost, spawn_remote  # noqa: E402

CTRL_P = b"\x10"

# Turns on SGR mouse reporting and prints what it is sent. Deterministic, unlike asking
# an editor what it thought a click meant.
READER = r'''
import sys, os, tty, termios
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)
sys.stdout.write("\x1b[?1000h\x1b[?1006h")
sys.stdout.write("MOUSE-READER-READY\r\n")
sys.stdout.flush()
buf = ""
try:
    while True:
        ch = os.read(fd, 1).decode("latin-1")
        buf += ch
        if ch in "Mm":
            if buf.startswith("\x1b[<"):
                sys.stdout.write("GOT " + buf[3:].replace("\x1b", "") + "\r\n")
                sys.stdout.flush()
            buf = ""
        elif ch == "q":
            break
        elif len(buf) > 32:
            buf = ""
finally:
    sys.stdout.write("\x1b[?1006l\x1b[?1000l")
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
'''


def main() -> int:
    results: list[bool] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append(bool(ok))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" -- {detail}" if detail and not ok else ""))

    def wait_free(peer, timeout: float = 45.0) -> bool:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if "control: free" in peer.snapshot():
                return True
            time.sleep(0.5)
        return False

    remote = RemoteHost()
    remote.reset_home()
    remote.reset_network()
    remote.run("cat > /root/mouse_reader.py <<'PYEOF'\n" + READER + "\nPYEOF")
    print(f"== scenario T: mouse + scrollback across {remote.alias} ==", flush=True)

    right_col = 95
    try:
        with Harness("t-mouse-scroll-cross") as harness:
            host, ticket = harness.create_room("mac", cols=120, rows=36)
            guest = spawn_remote(
                harness, remote, "droplet", ["join", ticket, "--name", "droplet"],
                cols=120, rows=36,
            )
            guest.wait_ready(timeout=60)
            guest.wait_for(r"Pane #\d+", timeout=60)

            guest.send(CTRL_P)
            time.sleep(0.5)
            guest.send(b"n")
            time.sleep(4.0)
            guest.send(b"for i in $(seq 1 400); do echo remote-line-$i; done\r")
            host.wait_for("remote-line-400", timeout=40)
            check("the remote pane's flood reached the coordinator", True)

            # ------------------------------------------------------------ scrollback
            host.click(right_col, 10)
            time.sleep(1.0)
            for _ in range(6):
                host.wheel_up(right_col, 10)
                time.sleep(0.3)
            time.sleep(2.0)
            after = host.settle(quiet_for=1.0, timeout=20)
            check("scrolling a remote-hosted pane does not hang the client", host.alive)
            check("scrolling a remote-hosted pane does not corrupt the layout",
                  len(re.findall(r"Pane #\d+", after)) >= 2, after[:300])
            check("no panic from the scroll", "panicked" not in host.raw_text())

            # Back to the live edge. A pane parked in history renders a frozen viewport,
            # so anything typed below would run and simply not be visible -- which looks
            # exactly like the input path being broken.
            for _ in range(12):
                host.wheel_down(right_col, 10)
                time.sleep(0.2)
            time.sleep(2.0)
            host.send(b"echo BACK-AT-LIVE-EDGE\r")
            try:
                host.wait_for("BACK-AT-LIVE-EDGE", timeout=30)
                check("scrolling back down returns the remote pane to the live edge", True)
            except Exception as error:
                check("scrolling back down returns the remote pane to the live edge",
                      False, str(error)[:200])

            # ------------------------------------------------------------ mouse
            check("the remote pane frees up before the Mac drives it", wait_free(host))
            host.send(b"python3 /root/mouse_reader.py\r")
            try:
                host.wait_for("MOUSE-READER-READY", timeout=30)
                check("a mouse-reporting program is running on the droplet", True)
            except Exception as error:
                check("a mouse-reporting program is running on the droplet", False,
                      str(error)[:200])

            time.sleep(2.0)
            # Several rows apart, so the coordinates are checkable rather than one point
            # that could be a coincidence.
            for offset in range(3):
                host.click(right_col, 12 + offset)
                time.sleep(1.0)
            time.sleep(2.5)

            got_guest = re.findall(r"GOT ([0-9;]+[Mm])", guest.snapshot())
            got_host = re.findall(r"GOT ([0-9;]+[Mm])", host.snapshot())
            check("the click reached the program on the other machine", bool(got_guest),
                  f"the droplet's own pane shows no report: {guest.snapshot()[-300:]!r}")
            check("the coordinator renders the remote program's response to its own click",
                  bool(got_host), f"mac pane: {host.snapshot()[-300:]!r}")
            if got_guest:
                parsed = [g.rstrip("Mm").split(";") for g in got_guest if g.count(";") == 2]
                cols = {int(p[1]) for p in parsed}
                rows = [int(p[2]) for p in parsed]
                check("coordinates are translated into the pane, not the whole terminal",
                      bool(cols) and max(cols) < right_col, f"columns reported: {sorted(cols)}")
                check("each click lands on its own row", len(set(rows)) >= 3,
                      f"rows reported: {rows}")

            host.send(b"q")
            time.sleep(1.5)
            check("both peers still alive", host.alive and guest.alive)
    finally:
        remote.reap()

    print(f"\n{sum(results)}/{len(results)} checks passed")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
