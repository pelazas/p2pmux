"""Record the README demo: two real p2pmux members, one shared session, one GIF.

The story, in about twelve seconds: userA is hosting and has the share panel open; userB
types `p2pmux join <code>` into their own shell, lands in the same layout, opens a pane
of their own and runs a command in it, then crosses to userA's pane and runs one there.

Both commands are the same beat told twice, and that is deliberate: a pane held by userB
is drawn in userB's member color wherever it is hosted, so the second half of the GIF is
green on userA's machine for exactly the same reason the first half was green on userB's.

Both members are real `target/release/p2pmux` processes on their own PTYs, started inside
the e2e harness's sandbox (so this can never touch a developer's live sessions). Their
screens are parsed with pyte, rendered cell-by-cell with PIL into two stacked terminal
cards, and encoded to a GIF with ffmpeg. Every glyph in the GIF was drawn by the binary.

One thing in the frame is dressed, and only one: each member gets a private HOME with its
own `.zshrc`, so the two panes show `userA@mac %` and `userB@desktop %`. The homes really
are different directories, but both shells run on this one Mac -- the honest evidence of
ownership is the pane title (`Pane #2 host: userB`), which the session maintains itself.

Run:
    python3 scripts/demo/record_demo.py                    # full GIF
    python3 scripts/demo/record_demo.py --still            # first/last PNG, no encode
    python3 scripts/demo/record_demo.py --replay CAP.pkl   # re-render a capture
"""

from __future__ import annotations

import argparse
import json
import os
import pickle
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "e2e"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from PIL import Image, ImageDraw, ImageFont  # noqa: E402

# The camera — cell capture, glyph rendering, ffmpeg — is shared with
# record_workflow.py. What stays here is this GIF's own layout and its story.
from termcard import (  # noqa: E402
    CARD_BG,
    CELL_H,
    CELL_W,
    CHROME_BG,
    CHROME_TEXT,
    PAGE_BG,
    draw_terminal,
    encode_gif,
    grab,
    load_fonts,
)

from driver import Harness  # noqa: E402

CTRL_P = b"\x10"
CTRL_S = b"\x13"
LEFT = b"\x1b[D"
RIGHT = b"\x1b[C"
ENTER = b"\r"
ESCAPE = b"\x1b"
# Option+Shift+Left moves focus without entering pane mode. Going through `Ctrl+P ← Esc`
# works too, but pane mode paints the focused border chord-red on the way, and a red
# frame between two green ones reads as three states when only one thing happened.
ALT_SHIFT_LEFT = b"\x1b[1;4D"
# Ctrl+U, spent to claim a pane. Any keystroke claims one, but the keystroke that wins
# the lease is consumed by the claim, so it has to be one that costs nothing if it does
# reach the shell: kill-line on an already-empty prompt draws nothing either way.
CLAIM = b"\x15"

REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE_DIR = REPO_ROOT / "target" / "release"

COLS, ROWS = 96, 14
FPS = 12

# This GIF's own frame: two stacked cards, and the padding around them.
PAD = 18
GAP = 16
CHROME_H = 30


@dataclass(frozen=True)
class Member:
    name: str
    role: str
    # config.rs DEFAULT_MEMBER_COLORS, by join order: this is the color the session
    # itself gives each member.
    color: str
    prompt: str
    # Dressing for this member's private HOME, so the two panes are looking at genuinely
    # different directories. Trailing slash means directory. Nothing on camera lists the
    # home itself: p2pmux keeps its session store under `~/Library`, and a demo that
    # shows its own scaffolding in the `ls` output is a demo about the recorder.
    files: tuple[str, ...]


MEMBERS = {
    "userA": Member(
        "userA",
        "· session host",
        "#ff6a13",
        "userA@mac %%",
        ("Desktop/", "Documents/", "Projects/", "notes.md"),
    ),
    "userB": Member(
        "userB",
        "· guest",
        "#7ed67e",
        "userB@desktop %%",
        ("Desktop/", "code/api/", "code/web/", "code/README.md", "todo.txt"),
    ),
}


# --------------------------------------------------------------------- capture


class Recorder(threading.Thread):
    """Sample both peers at a fixed rate on a thread, so the script can drive keys."""

    def __init__(self, peers: dict[str, object], fps: int = FPS) -> None:
        super().__init__(daemon=True)
        self.peers = peers
        self.interval = 1.0 / fps
        self.frames: list[dict[str, dict]] = []
        self._stop = threading.Event()

    def run(self) -> None:
        next_at = time.monotonic()
        while not self._stop.is_set():
            self.frames.append({name: grab(peer) for name, peer in self.peers.items()})
            next_at += self.interval
            time.sleep(max(0.0, next_at - time.monotonic()))

    def stop(self) -> None:
        self._stop.set()
        self.join(timeout=2.0)


# ----------------------------------------------------------------------- render


def card_size() -> tuple[int, int]:
    return COLS * CELL_W, ROWS * CELL_H


def canvas_size() -> tuple[int, int]:
    body_w, body_h = card_size()
    return body_w + PAD * 2, (body_h + CHROME_H) * 2 + GAP + PAD * 2


def draw_chrome(
    draw: ImageDraw.ImageDraw,
    member: "Member",
    origin: tuple[int, int],
    width: int,
    fonts: tuple[ImageFont.FreeTypeFont, ImageFont.FreeTypeFont],
) -> None:
    ox, oy = origin
    draw.rounded_rectangle(
        [ox, oy, ox + width - 1, oy + CHROME_H + 6],
        radius=8,
        fill=CHROME_BG,
    )
    # No macOS traffic lights. They are three more colors in a frame whose entire job is
    # to make two member colors legible, and one of them is the same green the session
    # gives userB. The only dot on the card is the member's own presence color -- the one
    # the session draws on their tab dot and pane marker -- and the name is tinted to
    # match, so "userA is blue, userB is green" survives a glance at a looping GIF.
    cy = oy + CHROME_H // 2
    draw.ellipse([ox + 18, cy - 4, ox + 26, cy + 4], fill=member.color)
    draw.text((ox + 36, oy + 8), member.name, font=fonts[1], fill=member.color)
    draw.text(
        (ox + 36 + 9 * (len(member.name) + 2), oy + 8),
        member.role,
        font=fonts[0],
        fill=CHROME_TEXT,
    )


def render_frame(
    frame: dict[str, dict],
    fonts: tuple[ImageFont.FreeTypeFont, ImageFont.FreeTypeFont],
    members: dict[str, "Member"],
) -> Image.Image:
    body_w, body_h = card_size()
    image = Image.new("RGB", canvas_size(), PAGE_BG)
    draw = ImageDraw.Draw(image)
    for index, (name, screen) in enumerate(frame.items()):
        top = PAD + index * (body_h + CHROME_H + GAP)
        draw_chrome(draw, members[name], (PAD, top), body_w, fonts)
        body_top = top + CHROME_H
        draw.rectangle([PAD, body_top, PAD + body_w - 1, body_top + body_h - 1], fill=CARD_BG)
        draw_terminal(draw, screen, (PAD, body_top), fonts)
    return image


# ------------------------------------------------------------------- the script


def pause(seconds: float) -> None:
    time.sleep(seconds)


SESSION = "demo"


def member_home(harness: Harness, member: Member, display_name: str | None = None) -> Path:
    """A private HOME per member: session store, shell prompt, and some files.

    Separate stores matter for more than tidiness. Two records of the same session name
    in one store are deduplicated to `demo-2`, and the tab bar would then show the two
    members a different name for the same session.
    """
    home = harness.home / member.name
    (home / "Library" / "Application Support" / "p2pmux").mkdir(parents=True, exist_ok=True)
    (home / ".config" / "p2pmux").mkdir(parents=True, exist_ok=True)
    # zsh reads /etc/zshrc first, which sets the stock macOS prompt, so this has to be
    # the user's own rc file to win.
    (home / ".zshrc").write_text(f"PROMPT='{member.prompt} '\n")
    if display_name:
        (home / ".config" / "p2pmux" / "config.toml").write_text(
            f'display_name = "{display_name}"\n'
        )
    for entry in member.files:
        target = home / entry.rstrip("/")
        if entry.endswith("/"):
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(f"{member.name}\n")
    return home


def wait_for_join_code(home: Path, timeout: float = 25.0) -> str:
    """The short code the coordinator published, read off its own session record.

    Not `Harness.create_room`: that looks a ticket up by the *display* name, while a
    record is keyed by the session's memorable name, so it only ever matched by luck.
    """
    store = home / "Library" / "Application Support" / "p2pmux" / "sessions"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for path in sorted(store.glob("*.json")):
            try:
                descriptor = json.loads(path.read_text())
            except (OSError, json.JSONDecodeError):
                continue
            if descriptor.get("join_code"):
                return descriptor["join_code"]
        time.sleep(0.1)
    raise AssertionError(f"no join code appeared in {store} within {timeout}s")


def build_scene(harness: Harness) -> tuple[object, object, str]:
    """Everything that happens before the camera rolls: userA hosting, share panel open,
    and userB sitting at their own shell prompt with nothing but the code to type."""
    host_member, guest_member = MEMBERS["userA"], MEMBERS["userB"]
    host_home = member_home(harness, host_member)
    # userB's name comes from their config, so the command they type on camera is just
    # `p2pmux join <code>` -- no --name flag padding out the line.
    guest_home = member_home(harness, guest_member, display_name=guest_member.name)

    host = harness.spawn(
        host_member.name,
        ["create", "--name", host_member.name, "--session-name", SESSION],
        cols=COLS,
        rows=ROWS,
        env={"HOME": str(host_home), "SHELL": "/bin/zsh"},
    )
    host.wait_for(r"userA@mac %", timeout=25)
    code = wait_for_join_code(host_home)

    # userA has the share panel up, which is where the code on screen comes from.
    host.send(CTRL_S)
    host.wait_for(r"Share this session", timeout=10)
    host.wait_for(re.escape(code), timeout=10)

    # userB's window is a plain login shell, not the binary: the recording has to show
    # the join being typed, so there has to be somewhere to type it.
    guest = harness.spawn(
        guest_member.name,
        [],
        cols=COLS,
        rows=ROWS,
        env={
            "HOME": str(guest_home),
            "SHELL": "/bin/zsh",
            "PATH": f"{RELEASE_DIR}:{os.environ['PATH']}",
        },
        launcher=["/bin/zsh", "-i"],
    )
    guest.wait_for(r"userB@desktop %", timeout=15)

    host.settle(quiet_for=0.4, timeout=5)
    guest.settle(quiet_for=0.4, timeout=5)
    return host, guest, code


def claim(guest, host, title: str) -> None:
    """Take the focused pane, and wait until both clients agree that it is taken.

    Control is authoritative shared state, so the assertion that matters is not "the
    guest thinks it holds this" but "both windows draw the same title" -- which is also
    exactly the frame the GIF needs before the next command starts typing.
    """
    guest.send(CLAIM)
    for peer in (host, guest):
        peer.wait_for(title, timeout=10)


def perform(host, guest, code: str) -> None:
    """The recorded ten seconds.

    Every wait for a state change is a real deadline-bounded wait, not a sleep sized to
    what the last run happened to take: the pauses are pacing, the waits are the product.
    """
    pause(0.8)

    # 1. userB joins from their own shell, with the ten characters on userA's screen.
    guest.type(f"p2pmux join {code}", per_key_delay=0.055)
    pause(0.3)
    guest.send(ENTER)
    guest.wait_for(r"Pane #1 host: userA", timeout=30)
    pause(0.7)

    # 2. userA sees someone arrive and closes the share panel.
    host.send(ESCAPE)
    host.wait_until(
        lambda screen: "Share this session" not in screen,
        timeout=5,
        what="the share panel to close",
    )
    pause(0.7)

    # 3. userB opens a pane of their own, hosted on userB's machine. Pane mode is sticky,
    # so leave it before typing -- otherwise `l` is read as the lock command.
    guest.send(CTRL_P)
    pause(0.4)
    guest.send(b"r")
    for peer in (host, guest):
        peer.wait_for(r"Pane #2 host: userB", timeout=15)
    guest.send(ESCAPE)
    guest.wait_until(
        lambda screen: "PANE MODE" not in screen,
        timeout=5,
        what="pane mode to exit",
    )
    # A pane's control state starts unknown on the client that created it and settles a
    # beat later, once the coordinator's view comes back. Nothing is wrong with showing
    # that, but it paints the border `unknown_focused` yellow, and a yellow that means
    # "still asking" is indistinguishable on a GIF from a yellow that means something.
    # Wait it out before rolling on.
    guest.wait_until(
        lambda screen: "control: …" not in screen,
        timeout=10,
        what="pane #2's control state to settle",
    )
    pause(0.5)

    # 4. userB takes their own pane and runs something in it. Typing is what claims a
    # pane, and the keystroke that wins the lease is spent on the claim rather than
    # reaching the shell -- so spend a bare Return on it and the command that follows
    # arrives whole. From here the border is userB's green on both screens, the same
    # green as their name on the card.
    claim(guest, host, r"Pane #2 host: userB control: userB")
    guest.type("ls ~/code", per_key_delay=0.1)
    pause(0.25)
    guest.send(ENTER)
    guest.wait_for(r"README\.md", timeout=10)
    pause(1.3)

    # 5. Focus crosses to userA's pane. No chord, so no red on the way.
    guest.send(ALT_SHIFT_LEFT)
    pause(0.6)

    # 6. The same claim, on a pane userA hosts: userB is now driving a shell on the other
    # machine, and both screens draw that border in the same green.
    claim(guest, host, r"Pane #1 host: userA control: userB")
    guest.type("echo userB", per_key_delay=0.085)
    pause(0.3)
    guest.send(ENTER)
    for peer in (host, guest):
        peer.wait_for(r"│userB\s", timeout=10)
    pause(1.8)


def render_gif(frames: list[dict], output: Path, frames_dir: Path) -> None:
    fonts = load_fonts()
    if frames_dir.exists():
        shutil.rmtree(frames_dir)
    frames_dir.mkdir(parents=True)
    for index, frame in enumerate(frames):
        render_frame(frame, fonts, MEMBERS).save(frames_dir / f"f{index:05d}.png")
    encode_gif(frames_dir, output, FPS)
    seconds = len(frames) / FPS
    size_mb = output.stat().st_size / 1_000_000
    print(f"{output}  ({len(frames)} frames, {seconds:.1f}s, {size_mb:.2f} MB)")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--still", action="store_true", help="render two PNGs, no GIF")
    parser.add_argument("--out", default="assets/demo.gif")
    parser.add_argument("--frames-dir", default=None)
    parser.add_argument(
        "--replay",
        metavar="CAPTURE.pkl",
        help="re-render a previous capture instead of running a new session",
    )
    args = parser.parse_args()

    output = Path(args.out).expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    # Frames are scaffolding, not output: keep them out of the repo unless asked for.
    frames_dir = (
        Path(args.frames_dir)
        if args.frames_dir
        else Path(tempfile.gettempdir()) / "p2pmux-demo-frames"
    )

    # Re-rendering a capture is how the look gets tuned: no session, no 30s lease wait.
    if args.replay:
        with open(args.replay, "rb") as handle:
            render_gif(pickle.load(handle), output, frames_dir)
        return 0

    with Harness("demo") as harness:
        host, guest, code = build_scene(harness)

        if args.still:
            fonts = load_fonts()
            before = {"userA": grab(host), "userB": grab(guest)}
            perform(host, guest, code)
            after = {"userA": grab(host), "userB": grab(guest)}
            render_frame(before, fonts, MEMBERS).save(output.with_suffix(".before.png"))
            render_frame(after, fonts, MEMBERS).save(output.with_suffix(".after.png"))
            print(output.with_suffix(".before.png"))
            print(output.with_suffix(".after.png"))
            return 0

        recorder = Recorder({"userA": host, "userB": guest})
        recorder.start()
        started = time.monotonic()
        perform(host, guest, code)
        recorder.stop()
        print(f"performance took {time.monotonic() - started:.1f}s")

    capture = Path(tempfile.gettempdir()) / "p2pmux-demo.capture.pkl"
    with open(capture, "wb") as handle:
        pickle.dump(recorder.frames, handle)
    render_gif(recorder.frames, output, frames_dir)
    print(f"capture: {capture}  (re-render with --replay)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
