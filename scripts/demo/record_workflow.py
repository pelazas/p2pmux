"""Record the whole p2pmux workflow across three real machines, as a video.

The story: this Mac hosts a session. Two DigitalOcean droplets — New York and
Frankfurt — join it with the same ten-character code, and each opens a tab of
its own. Then, *from the Mac*, an agent is started in each of those tabs: real
`opencode` running on the droplet, on the droplet's files, paid for by the
droplet's provider key. Their hooks report back, the Mac's inbox says which one
is blocking a human, and pressing Enter on that row puts the Mac's keyboard
inside that agent's terminal in another datacenter.

Nothing here is a mock. Three `p2pmux` processes on three machines, two
`opencode` processes on two continents, one session. Every glyph inside a card
was drawn by one of them; the only thing this script adds is the frame and the
list of steps beside it.

A tab per machine, rather than a grid of panes, is not decoration. A pane's PTY
is sized from the smallest view of it, so three panes tiled across a 100-column
terminal would hand each agent 33 columns — on every machine, including the ones
with room. One pane per tab gives each agent the full width on all three.

Run:
    python3 scripts/demo/record_workflow.py                    # the full video
    python3 scripts/demo/record_workflow.py --still            # one PNG, no session
    python3 scripts/demo/record_workflow.py --replay CAP.pkl   # re-render a capture

Needs `scripts/e2e/provision_droplets.sh create` to have run, and both droplets
to have `opencode` installed with a provider key exported from /root/demorc.
"""

from __future__ import annotations

import argparse
import json
import os
import pickle
import re
import shlex
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

from PIL import Image, ImageDraw  # noqa: E402

import termcard  # noqa: E402
from driver import Harness  # noqa: E402
from remote import BASE_ENV, MANIFEST_PATH, RemoteHost  # noqa: E402

CTRL_O = b"\x0f"
CTRL_S = b"\x13"
CTRL_T = b"\x14"
ENTER = b"\r"
RIGHT = b"\x1b[C"
LEFT = b"\x1b[D"
# Ctrl+U claims a pane. Any keystroke does, but the one that wins the lease is
# consumed by the claim, so it has to cost nothing if it does reach the program:
# kill-line draws nothing on an empty prompt and nothing in a dialog.
CLAIM = b"\x15"

SESSION = "agents"
FPS = 12
# What the recorder samples at while nothing is happening but an agent thinking.
# The video plays at a constant FPS, so a minute of waiting becomes six seconds
# while the beats either side of it stay real time.
SLOW_FPS = 2

# Every card is this size, on purpose: a pane is rendered into a box scaled to
# each viewer's own terminal, and matching terminals mean every machine sees the
# agent at the same size the machine hosting it does.
COLS, ROWS = 100, 24

PAD = 20
GAP = 18
HEADER_H = 52
CHROME_H = 30


@dataclass
class Machine:
    key: str
    name: str
    where: str
    # config.rs DEFAULT_MEMBER_COLORS, by join order: the colour the session
    # itself gives this member, so the card and its pane border agree.
    color: str
    cell: tuple[int, int]
    peer: object = None
    origin: tuple[int, int] = (0, 0)


def machines() -> dict[str, Machine]:
    return {
        "laptop": Machine("laptop", "laptop", "· this MacBook, hosting", "#ff6a13", (0, 0)),
        "nyc3": Machine("nyc3", "nyc3", "· droplet · New York", "#7ed67e", (1, 0)),
        "fra1": Machine("fra1", "fra1", "· droplet · Frankfurt", "#5aa9ff", (0, 1)),
    }


CARD_W = COLS * termcard.CELL_W
CARD_H = ROWS * termcard.CELL_H + CHROME_H


def layout(cards: dict[str, Machine]) -> tuple[int, int]:
    """Three terminals in a 2×2, and the steps of the run in the fourth corner."""
    for card in cards.values():
        column, row = card.cell
        card.origin = (
            PAD + column * (CARD_W + GAP),
            HEADER_H + PAD + row * (CARD_H + GAP),
        )
    width = PAD * 2 + CARD_W * 2 + GAP
    height = HEADER_H + PAD * 2 + CARD_H * 2 + GAP
    return width, height


# --------------------------------------------------------------------- capture


# The narration, in the order `perform` walks it. It is a list rather than a
# string per beat because the whole list is on screen the whole time: a viewer
# who joins halfway can see what has happened and what is coming.
STEPS = [
    "Three machines, one session. The laptop hosts; the droplets are in New York and Frankfurt.",
    "Each machine wires its own agent's hooks, once: p2pmux setup opencode",
    "Ctrl+S — one ten-character code, and nothing else to exchange.",
    "Both droplets join with it, from their own shells, on their own networks.",
    "Each droplet opens a tab of its own. A pane runs on the machine that opened it.",
    "Whoever opens a pane holds it. Nobody is forced out; thirty idle seconds frees it.",
    "From the laptop: start an agent on the New York machine.",
    "And a second one in Frankfurt. Another machine, another key, the same session.",
    "Two agents working on two continents. Neither of them runs on this laptop.",
    "They stop before running anything. Their hooks say so, and the tab bar counts them.",
    "Ctrl+O — every agent on every machine, sorted by which one is blocking you.",
    "Enter on that row: the keyboard is now inside that agent's terminal, in its datacenter.",
    "Answering from here runs it there — their machine, their files, their key, their bill.",
    "Ctrl+O goes back. The agents keep running on the machines that own them.",
]


class Narrator:
    """Where in `STEPS` the run is. The only thing on screen this script wrote."""

    def __init__(self) -> None:
        self.index = -1

    def next(self) -> None:
        self.index += 1
        print(f"\n--- [{self.index + 1}/{len(STEPS)}] {STEPS[self.index]}", flush=True)


class Recorder(threading.Thread):
    """Sample every peer at a rate the script can change as it goes."""

    def __init__(self, cards: dict[str, Machine], narrator: Narrator) -> None:
        super().__init__(daemon=True)
        self.cards = cards
        self.narrator = narrator
        self.frames: list[dict] = []
        self.fps = FPS
        self._stop = threading.Event()

    def rate(self, fps: int) -> None:
        self.fps = fps

    def run(self) -> None:
        while not self._stop.is_set():
            started = time.monotonic()
            self.frames.append(
                {
                    "screens": {
                        key: termcard.grab(card.peer)
                        for key, card in self.cards.items()
                        if card.peer is not None
                    },
                    "step": self.narrator.index,
                }
            )
            self._stop.wait(max(0.0, (1.0 / self.fps) - (time.monotonic() - started)))

    def stop(self) -> None:
        self._stop.set()
        self.join(timeout=3.0)


# ---------------------------------------------------------------------- render


def wrap(text: str, width: int) -> list[str]:
    lines, line = [], ""
    for word in text.split():
        candidate = f"{line} {word}".strip()
        if len(candidate) > width and line:
            lines.append(line)
            line = word
        else:
            line = candidate
    if line:
        lines.append(line)
    return lines


def draw_steps(draw: ImageDraw.ImageDraw, origin: tuple[int, int], current: int) -> None:
    """The fourth quadrant: what has happened, what is happening, what is next."""
    heading = termcard.load_ui_font(15, bold=True)
    body = termcard.load_ui_font(17)
    ox, oy = origin
    draw.text((ox + 6, oy + 8), "WHAT IS HAPPENING", font=heading, fill="#4a5464")
    y = oy + 40
    for index, step in enumerate(STEPS):
        done = index < current
        now = index == current
        colour = "#e8eefc" if now else ("#5c6577" if done else "#39414f")
        if now:
            draw.rectangle([ox, y - 2, ox + 3, y + 20], fill="#ff6a13")
        draw.text((ox + 16, y), f"{index + 1:2d}", font=body, fill="#39414f" if not now else "#ff6a13")
        for offset, line in enumerate(wrap(step, 78)):
            draw.text((ox + 46, y + offset * 20), line, font=body, fill=colour)
        y += 24 + 20 * (len(wrap(step, 78)) - 1)


def render_frame(frame: dict, cards: dict[str, Machine], size: tuple[int, int]) -> Image.Image:
    fonts = termcard.load_fonts()
    title_font = termcard.load_ui_font(21, bold=True)
    small_font = termcard.load_ui_font(14)

    image = Image.new("RGB", size, termcard.PAGE_BG)
    draw = ImageDraw.Draw(image)
    draw.text((PAD + 2, 16), "p2pmux", font=title_font, fill="#e8eefc")
    draw.text(
        (PAD + 92, 21),
        "one session · three machines · an agent on each of the two that are not this one",
        font=small_font,
        fill=termcard.CHROME_TEXT,
    )

    for key, card in cards.items():
        ox, oy = card.origin
        draw.rounded_rectangle(
            [ox, oy, ox + CARD_W - 1, oy + CHROME_H + 6], radius=8, fill=termcard.CHROME_BG
        )
        centre = oy + CHROME_H // 2
        draw.ellipse([ox + 16, centre - 4, ox + 24, centre + 4], fill=card.color)
        draw.text((ox + 34, oy + 8), card.name, font=fonts[1], fill=card.color)
        draw.text(
            (ox + 34 + termcard.CELL_W * (len(card.name) + 2), oy + 8),
            card.where,
            font=fonts[0],
            fill=termcard.CHROME_TEXT,
        )
        body_top = oy + CHROME_H
        draw.rectangle(
            [ox, body_top, ox + CARD_W - 1, body_top + ROWS * termcard.CELL_H - 1],
            fill=termcard.CARD_BG,
        )
        screen = frame["screens"].get(key)
        if screen:
            termcard.draw_terminal(draw, screen, (ox, body_top), fonts)

    draw_steps(
        draw,
        (PAD + CARD_W + GAP, HEADER_H + PAD + CARD_H + GAP),
        frame["step"],
    )
    return image


def render_video(
    frames: list[dict], cards: dict[str, Machine], output: Path, frames_dir: Path
) -> None:
    size = layout(cards)
    if frames_dir.exists():
        shutil.rmtree(frames_dir)
    frames_dir.mkdir(parents=True)
    for index, frame in enumerate(frames):
        render_frame(frame, cards, size).save(frames_dir / f"f{index:05d}.png")
        if index and index % 250 == 0:
            print(f"  rendered {index}/{len(frames)}", flush=True)
    termcard.encode_mp4(frames_dir, output, FPS)
    print(
        f"{output}  ({len(frames)} frames, {len(frames) / FPS:.0f}s, "
        f"{output.stat().st_size / 1_000_000:.1f} MB)",
        flush=True,
    )


# ----------------------------------------------------------------------- setup


def droplets() -> list[RemoteHost]:
    """The provisioned pair, each with its own sandbox HOME on the droplet."""
    manifest = json.loads(MANIFEST_PATH.read_text())
    return [
        RemoteHost(
            alias=entry["alias"],
            hostname=entry["hostname"],
            identity=manifest["key_file"],
            binary_path=manifest["binary"],
            # A pane starts in the home directory of whoever hosts it, so the
            # home *is* the project the agents are given: anywhere else and both
            # of them open in a directory with nothing in it.
            home="/root/work",
        )
        for entry in manifest["hosts"]
    ]


def shell_launcher(remote: RemoteHost, name: str) -> list[str]:
    """An interactive shell on the droplet, not the binary.

    The video has to show `p2pmux join` being typed, which means there has to be
    somewhere to type it. `/root/demorc` sets the prompt, exports the agent's
    provider key and lands in the work directory.
    """
    environment = {**BASE_ENV, "HOME": remote.home}
    assignments = " ".join(f"{key}={shlex.quote(value)}" for key, value in environment.items())
    return [
        "ssh",
        "-tt",
        *remote.ssh_flags(),
        remote.target,
        f"env {assignments} /bin/bash --rcfile /root/demorc -i",
    ]


def project(remote: RemoteHost) -> None:
    """Something real for the agents to read, run and fix.

    A git repository rather than a loose directory, because opencode reports the
    enclosing checkout as its worktree and a directory outside one reports `/`.
    """
    remote.run(
        f"mkdir -p {remote.home} && cd {remote.home} && "
        "cat > stats.py <<'PY'\n"
        "def mean(values):\n"
        "    return sum(values) / len(values)\n"
        "\n"
        "\n"
        "def median(values):\n"
        "    ordered = sorted(values)\n"
        "    return ordered[len(ordered) // 2]\n"
        "\n"
        "\n"
        "if __name__ == '__main__':\n"
        "    print('mean', mean([1, 2, 3, 4]))\n"
        "    print('median', median([1, 2, 3, 4]))\n"
        "PY\n"
        "cat > README.md <<'MD'\n"
        "# stats\n"
        "\n"
        "`median()` is wrong for an even number of values: it returns one side of\n"
        "the middle pair instead of the average of both. `median([1, 2, 3, 4])`\n"
        "should be 2.5.\n"
        "MD\n"
        "git init -q 2>/dev/null; git add -A 2>/dev/null; "
        "git -c user.email=demo@p2pmux -c user.name=demo commit -qm 'stats' 2>/dev/null; true"
    )


def prepare(remote: RemoteHost, name: str) -> None:
    """Everything on the droplet that is not worth filming.

    A fresh sandbox HOME, this machine's display name, an opencode that asks
    before it runs anything — the demo needs a `needs you`, and the honest way to
    get one is to leave the permission prompt where it is — and the shell rc the
    recording's prompt comes from.
    """
    remote.reset_home()
    project(remote)
    remote.cli(f"config set name {name}")
    remote.run(f"mkdir -p {remote.home}/.config/opencode")
    config = json.dumps(
        {
            "$schema": "https://opencode.ai/config.json",
            "model": "deepseek/deepseek-chat",
            "permission": {"bash": "ask", "edit": "ask", "webfetch": "ask"},
        },
        indent=2,
    )
    remote.run(f"cat > {remote.home}/.config/opencode/opencode.json <<'JSON'\n{config}\nJSON")
    remote.run(
        "cat > /root/demorc <<'RC'\n"
        f"export HOME={remote.home}\n"
        "export PS1='\\[\\e[38;5;108m\\]root@" + name + "\\[\\e[0m\\]:~$ '\n"
        ". /root/deepseek.env\n"
        f"cd {remote.home}\n"
        "RC"
    )
    print(remote.cli("setup opencode", timeout=60).strip(), flush=True)


def wait_for_join_code(home: Path, timeout: float = 30.0) -> str:
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


# ------------------------------------------------------------------- the story


def new_tab(peer, others, host: str, tab: int) -> None:
    """Open a tab with a pane of this machine's own, and wait for every client.

    No Esc afterwards: the chord times out by itself, and an Esc typed here would
    land *in* the new pane — making this client the pane's active typist and
    holding its lease for another thirty seconds.
    """
    peer.send(CTRL_T)
    time.sleep(0.7)
    peer.send(b"n")
    peer.wait_for(rf"host: {host}", timeout=45)
    # The others stay where they are; what has to reach them is the tab itself.
    for other in others:
        other.wait_for(rf"Tab #{tab}", timeout=30)


def claim(driver, others, title: str, timeout: float = 30.0) -> None:
    """Take the focused pane, and wait until every window agrees it is taken.

    Control is authoritative shared state, so the assertion that matters is not
    "this client thinks it holds the pane" but "both windows draw the same
    title" — which is also the frame the video needs before anything is typed.
    """
    driver.send(CLAIM)
    for peer in (driver, *others):
        peer.wait_for(title, timeout=timeout)


def start_agent(laptop, droplet, host: str, prompt: str, recorder: Recorder) -> None:
    """Launch and drive an agent in a pane hosted on another machine.

    Every keystroke here is typed on the laptop. `opencode` starts on the droplet
    because that is where the pane's PTY lives — the laptop never runs it, never
    sees the provider key, and could not have run it if it wanted to.
    """
    claim(laptop, [droplet], rf"host: {host} control: laptop")
    laptop.type("opencode\r", per_key_delay=0.045)
    # opencode's TUI takes about half a minute to paint on a droplet this size,
    # and a prompt typed before it does is thrown away. Both machines watch it
    # appear at the same moment, which is its own small proof.
    recorder.rate(SLOW_FPS)
    laptop.wait_for(r"Ask anything", timeout=240)
    droplet.wait_for(r"Ask anything", timeout=60)
    recorder.rate(FPS)
    time.sleep(2.0)
    laptop.type(prompt, per_key_delay=0.022)
    time.sleep(0.8)
    laptop.send(ENTER)


def switch_tab(peer, times: int = 1, back: bool = False) -> None:
    for _ in range(times):
        peer.send(CTRL_T)
        time.sleep(0.5)
        peer.send(LEFT if back else RIGHT)
        time.sleep(0.9)


def perform(cards: dict[str, Machine], code: str, recorder: Recorder, narrator: Narrator) -> None:
    laptop = cards["laptop"].peer
    nyc3 = cards["nyc3"].peer
    fra1 = cards["fra1"].peer

    narrator.next()
    time.sleep(3.5)

    narrator.next()
    for peer in (nyc3, fra1):
        peer.type("p2pmux setup opencode\r", per_key_delay=0.02)
    for peer in (nyc3, fra1):
        peer.wait_for(r"wrote the p2pmux plugin", timeout=30)
    time.sleep(3.0)

    narrator.next()
    laptop.send(CTRL_S)
    laptop.wait_for(r"Share this session", timeout=15)
    time.sleep(4.0)

    narrator.next()
    for peer in (nyc3, fra1):
        peer.type(f"p2pmux join {code}\r", per_key_delay=0.035)
    for peer in (nyc3, fra1):
        peer.wait_for(r"Pane #\d+", timeout=120)
    laptop.send(b"\x1b")
    laptop.wait_until(
        lambda screen: "Share this session" not in screen,
        timeout=10,
        what="the share panel to close",
    )
    time.sleep(2.5)

    narrator.next()
    new_tab(nyc3, [laptop, fra1], "nyc3", tab=2)
    time.sleep(1.5)
    new_tab(fra1, [laptop, nyc3], "fra1", tab=3)
    time.sleep(2.5)

    narrator.next()
    # `lease::IDLE_AFTER` is thirty seconds, and it is the reason the laptop
    # cannot simply take a pane the moment somebody else opens one.
    switch_tab(laptop)
    recorder.rate(SLOW_FPS)
    laptop.wait_for(r"host: nyc3 control: free", timeout=90)
    recorder.rate(FPS)
    time.sleep(1.5)

    narrator.next()
    start_agent(
        laptop, nyc3, "nyc3",
        "run stats.py with python3 and tell me whether median is correct",
        recorder,
    )
    time.sleep(2.5)

    narrator.next()
    switch_tab(laptop)
    recorder.rate(SLOW_FPS)
    laptop.wait_for(r"host: fra1 control: free", timeout=90)
    recorder.rate(FPS)
    time.sleep(1.0)
    start_agent(
        laptop, fra1, "fra1",
        "read README.md, then fix median in stats.py",
        recorder,
    )
    time.sleep(2.5)

    narrator.next()
    # Back to the laptop's own tab, which is the emptiest thing in the frame and
    # the whole point: the work is happening on the other two cards.
    switch_tab(laptop, times=2, back=True)
    time.sleep(2.0)

    narrator.next()
    # Waiting on the badge rather than on a clock is what keeps this caption
    # honest: the video only says an agent is blocked once one says so.
    recorder.rate(SLOW_FPS)
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        if re.search(r"inbox \d", laptop.snapshot().splitlines()[0]):
            break
        time.sleep(1.0)
    recorder.rate(FPS)
    time.sleep(4.0)

    narrator.next()
    laptop.send(CTRL_O)
    time.sleep(1.5)
    laptop.wait_for(r"opencode", timeout=25)
    time.sleep(6.0)

    narrator.next()
    laptop.send(ENTER)
    time.sleep(4.0)
    claim(laptop, [], r"control: laptop", timeout=30)
    time.sleep(2.0)

    narrator.next()
    # Answer it, and keep answering: "Allow always" whitelists one command
    # pattern rather than the turn, so an agent working through a task asks
    # again for the next command. This is a person sitting with an agent for a
    # minute, which is what the pane is for.
    recorder.rate(4)
    deadline = time.monotonic() + 180
    answered = 0
    while time.monotonic() < deadline:
        screen = laptop.snapshot()
        blocked = "Permission required" in screen
        # 2.5 is the median it was asked about, and a number it can only print
        # by having run the file on the droplet. Not while a dialog is up: an
        # agent that writes `assert median([1,2,3,4]) == 2.5` puts the number on
        # screen in the command it is still asking permission to run, and
        # stopping there ends the video on the question rather than the answer.
        if "2.5" in screen and not blocked:
            break
        if blocked:
            # A lease that went idle while the agent thought means the first
            # keystroke buys the pane back rather than answering the dialog.
            if "control: laptop" not in screen:
                laptop.send(CLAIM)
                time.sleep(2.0)
            laptop.send(ENTER)
            answered += 1
            time.sleep(7.0)
        else:
            time.sleep(2.0)
    print(f"  (answered {answered} permission prompts)", flush=True)
    recorder.rate(FPS)
    time.sleep(6.0)

    narrator.next()
    laptop.send(CTRL_O)
    time.sleep(8.0)


# ------------------------------------------------------------------------ main


def build_scene(harness: Harness, cards: dict[str, Machine], hosts: list[RemoteHost]) -> str:
    subprocess.run(
        ["./target/release/p2pmux", "config", "set", "name", "laptop"],
        env={"HOME": str(harness.home), "PATH": "/usr/bin:/bin"},
        capture_output=True,
        check=False,
    )
    cards["laptop"].peer = harness.spawn(
        "laptop",
        ["create", "--name", "laptop", "--session-name", SESSION],
        cols=COLS,
        rows=ROWS,
    )
    cards["laptop"].peer.wait_for(r"Pane #\d+", timeout=60)
    code = wait_for_join_code(harness.home)

    for card, remote in zip((cards["nyc3"], cards["fra1"]), hosts):
        card.peer = harness.spawn(
            card.key,
            [],
            cols=COLS,
            rows=ROWS,
            launcher=shell_launcher(remote, card.key),
        )
        card.peer.wait_for(rf"root@{card.key}:~", timeout=45)
    return code


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", default="assets/workflow.mp4")
    parser.add_argument("--still", action="store_true", help="one PNG of the layout, no session")
    parser.add_argument("--replay", metavar="CAPTURE.pkl", help="re-render a previous capture")
    parser.add_argument("--frames-dir", default=None)
    args = parser.parse_args()

    # A `Peer` inherits this process's environment, and this script is often run
    # from inside a p2pmux pane — which would hand the recorded session the
    # recording developer's own pane id and node socket.
    for leaked in ("P2PMUX_PANE_ID", "P2PMUX_SOCK"):
        os.environ.pop(leaked, None)

    cards = machines()
    output = Path(args.out).expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    frames_dir = (
        Path(args.frames_dir)
        if args.frames_dir
        else Path(tempfile.gettempdir()) / "p2pmux-workflow-frames"
    )
    capture_path = Path(tempfile.gettempdir()) / "p2pmux-workflow.capture.pkl"

    if args.replay:
        with open(args.replay, "rb") as handle:
            render_video(pickle.load(handle), cards, output, frames_dir)
        return 0

    if args.still:
        size = layout(cards)
        render_frame({"screens": {}, "step": 6}, cards, size).save(output.with_suffix(".layout.png"))
        print(output.with_suffix(".layout.png"))
        return 0

    hosts = droplets()
    narrator = Narrator()
    recorder = None
    failure = None
    try:
        for remote, name in zip(hosts, ("nyc3", "fra1")):
            prepare(remote, name)
        with Harness("workflow-video") as harness:
            code = build_scene(harness, cards, hosts)
            recorder = Recorder(cards, narrator)
            recorder.start()
            started = time.monotonic()
            try:
                perform(cards, code, recorder, narrator)
            finally:
                recorder.stop()
                print(
                    f"\nperformance: {time.monotonic() - started:.0f}s, "
                    f"{len(recorder.frames)} frames",
                    flush=True,
                )
            for remote in hosts:
                remote.reap()
    except BaseException as error:  # noqa: BLE001 — the footage is the point
        failure = error
        # A run that dies halfway has still recorded everything up to the moment
        # it died, and that is exactly when the footage is worth most.
        print(f"\nrun failed: {type(error).__name__}: {error}", flush=True)

    if recorder and recorder.frames:
        with open(capture_path, "wb") as handle:
            pickle.dump(recorder.frames, handle)
        print(f"capture: {capture_path}  (re-render with --replay)", flush=True)
        render_video(recorder.frames, cards, output, frames_dir)
    if failure is not None:
        raise failure
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
