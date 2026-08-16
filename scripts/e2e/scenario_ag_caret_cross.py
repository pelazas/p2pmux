#!/usr/bin/env python3
"""Where the caret is, with one of the panes on another machine.

The caret is the only mark on screen that says where typing goes, and three
rules govern it. Each was broken in a way no unit test could see, because each
depends on state the client assembles from somewhere else:

  * a pane scrolled into history is not showing where its program's cursor is,
    so it must not keep the caret. A client keeps no scrollback of its own -- it
    renders history by swapping in a viewport the node built, whose own offset
    is always zero -- so a gate that asked the *screen* how far back it was
    scrolled always answered "at the live edge";
  * a dialog that takes the keyboard takes the caret with it, or the panel gets
    the typing while the caret goes on blinking in the pane behind it;
  * anything that throws those fetched viewports away -- a resize, or a node
    answering that the history is gone -- drops the pane at its live edge, so
    its offset has to come back to zero with them. Otherwise the pane reads as
    scrolled while showing the newest output: no caret, and the next several
    notches of wheel-down spent walking back through rows that are not there.

Every assertion is made on the Mac, which is the client under test. The droplet
is here to host a pane: a remote-hosted pane's screen arrives over Iroh, its
cursor position is the other machine's, and its history is the case the node
answers "unavailable" for -- which is the third rule's other trigger.

Requires the provisioned lab (`scripts/e2e/provision_droplets.sh create`).

Run: python3 scripts/e2e/scenario_ag_caret_cross.py
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness  # noqa: E402
from remote import hosts_from_manifest, spawn_remote  # noqa: E402

CTRL_P = b"\x10"
ESC = b"\x1b"

COLS = 120
ROWS = 36
# Clear of every border: the left pane is the Mac's, the right one the droplet's.
LEFT_COL = 20
RIGHT_COL = 95
PANE_ROW = 10


def main() -> int:
    results: list[bool] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append(bool(ok))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" -- {detail}" if detail and not ok else ""))

    def wheel(peer, col: int, up: bool, times: int) -> None:
        for _ in range(times):
            (peer.wheel_up if up else peer.wheel_down)(col, PANE_ROW)
            time.sleep(0.25)
        time.sleep(1.5)

    remote = hosts_from_manifest()[0]
    remote.reset_home()
    print(f"== scenario AG: the caret, with a pane on {remote.alias} ==", flush=True)

    try:
        with Harness("ag-caret-cross") as harness:
            host, ticket = harness.create_room("mac", cols=COLS, rows=ROWS)
            guest = spawn_remote(
                harness, remote, "droplet", ["join", ticket, "--name", "droplet"],
                cols=COLS, rows=ROWS,
            )
            guest.wait_ready(timeout=60)
            guest.wait_for(r"Pane #\d+", timeout=60)

            # The droplet splits, so the right-hand pane's PTY lives on Linux.
            guest.send(CTRL_P)
            time.sleep(0.5)
            guest.send(b"n")
            time.sleep(4.0)
            # By title, not by number: "Pane #2" appears the moment the layout
            # arrives, while what this scenario needs is a pane whose PTY is on
            # the other machine.
            host.wait_for(r"host: droplet", timeout=60)

            # ------------------------------------------- a pane on the other machine
            host.click(RIGHT_COL, PANE_ROW)
            time.sleep(1.5)
            check(
                "a focused pane hosted on the droplet carries the caret",
                not host.cursor_hidden(),
                f"caret at {host.cursor()}",
            )

            # ------------------------------------------- a dialog takes it
            host.send(CTRL_P)
            time.sleep(0.5)
            host.send(b"e")
            time.sleep(1.5)
            host.send(b"buildbox")
            time.sleep(2.0)
            renaming = host.snapshot().splitlines()
            caret_row, caret_col = host.cursor()
            field_row = next(
                (index for index, line in enumerate(renaming) if "buildbox" in line), None
            )
            check("the rename panel is up with what was typed in it", field_row is not None,
                  "\n".join(renaming)[:400])
            check(
                "the caret is in the field being typed into, not the pane behind it",
                field_row is not None and caret_row == field_row,
                f"caret on row {caret_row}, field on row {field_row}",
            )
            check(
                "and it sits just past the last character typed",
                field_row is not None and renaming[field_row][:caret_col].endswith("buildbox"),
                f"row up to the caret reads {renaming[field_row][:caret_col]!r}"
                if field_row is not None
                else "no field",
            )
            host.send(ESC)
            time.sleep(1.5)
            check("closing the panel gives the caret back to the pane",
                  not host.cursor_hidden())

            # ------------------------------------------- history that is not reachable
            check("the droplet is still in the session", guest.alive)
            guest.send(b"for i in $(seq 1 400); do echo remote-line-$i; done\r")
            host.wait_for("remote-line-400", timeout=60)
            host.click(RIGHT_COL, PANE_ROW)
            time.sleep(1.0)
            wheel(host, RIGHT_COL, up=True, times=6)
            after_remote_wheel = host.snapshot()
            # v1: only the node that hosts a pane can serve its scrollback. The
            # pane therefore stays where it is -- and, having never left the live
            # edge, keeps its caret.
            check(
                "a remote-hosted pane that cannot serve history keeps its caret",
                not host.cursor_hidden(),
                f"caret hidden; screen: {after_remote_wheel[-300:]!r}",
            )
            check(
                "and stays at the live edge rather than freezing on a view it never got",
                "remote-line-400" in after_remote_wheel,
                after_remote_wheel[-300:],
            )
            check("no panic from scrolling a remote pane", "panicked" not in host.raw_text())

            # ------------------------------------------- history that is reachable
            host.click(LEFT_COL, PANE_ROW)
            time.sleep(1.0)
            host.send(b"for i in $(seq 1 400); do echo mac-line-$i; done\r")
            host.wait_for("mac-line-400", timeout=45)
            time.sleep(1.0)
            wheel(host, LEFT_COL, up=True, times=6)
            scrolled = host.snapshot()
            check(
                "the Mac's own pane really does move into history",
                "mac-line-400" not in scrolled,
                "the live edge is still on screen",
            )
            check(
                "scrolling into history takes the caret off the pane",
                host.cursor_hidden(),
                f"caret still shown at {host.cursor()}",
            )

            wheel(host, LEFT_COL, up=False, times=10)
            check("returning to the live edge gives the caret back", not host.cursor_hidden())

            # ------------------------------------------- resized mid-history
            wheel(host, LEFT_COL, up=True, times=6)
            check("scrolled back again", host.cursor_hidden())
            host.resize(COLS - 12, ROWS - 4)
            time.sleep(3.0)
            check(
                "a resize mid-history lands at the live edge, caret and all",
                not host.cursor_hidden(),
                f"caret still hidden; screen: {host.snapshot()[-300:]!r}",
            )

            check("both peers still alive", host.alive and guest.alive)
            check(
                "no panic anywhere",
                "panicked" not in host.raw_text() and "panicked" not in guest.raw_text(),
            )
    finally:
        remote.reap()

    print(f"\n{sum(results)}/{len(results)} checks passed")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
