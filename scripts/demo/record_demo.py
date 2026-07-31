"""Record the README demo: two real p2pmux members, one shared session, one GIF.

Both members are real `target/release/p2pmux` processes on their own PTYs, started
inside the e2e harness's sandbox HOME (so this can never touch a developer's live
sessions). Their screens are parsed with pyte, rendered cell-by-cell with PIL into
two stacked terminal cards, and encoded to a GIF with ffmpeg.

Nothing here is staged output: every glyph in the GIF was drawn by the binary.

Run:
    python3 scripts/demo/record_demo.py            # full GIF
    python3 scripts/demo/record_demo.py --still    # one PNG of the final frame, fast
"""

from __future__ import annotations

import argparse
import json
import pickle
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "e2e"))

from PIL import Image, ImageDraw, ImageFont  # noqa: E402
from wcwidth import wcwidth  # noqa: E402

from driver import Harness  # noqa: E402

CTRL_P = b"\x10"
LEFT = b"\x1b[D"
RIGHT = b"\x1b[C"
ESCAPE = b"\x1b"

COLS, ROWS = 96, 14
FPS = 12

# ------------------------------------------------------------------ appearance

# Menlo at 15px is exactly 9x18 per cell with no rounding, and carries the ✓/✗/●/◆
# glyphs the agents overlay draws.
FONT_PATH = Path("/System/Library/Fonts/Menlo.ttc")
FONT_SIZE = 15
CELL_W, CELL_H = 9, 18
BASELINE_PAD = 0

PAGE_BG = "#07080b"
CARD_BG = "#0d1016"
CHROME_BG = "#171b24"
CHROME_TEXT = "#7d879c"
LABEL_TEXT = "#c3cbd9"

PAD = 18
GAP = 16
CHROME_H = 30

# xterm's first eight, warmed up slightly for a dark card.
ANSI = {
    "black": "#20242e",
    "red": "#ff6b63",
    "green": "#4ed58c",
    "brown": "#e4c05c",
    "blue": "#5aa9ff",
    "magenta": "#c78dff",
    "cyan": "#4fd6d6",
    "white": "#d5dbe6",
}
DEFAULT_FG = "#cbd3e1"
DEFAULT_BG = CARD_BG


@dataclass(frozen=True)
class Member:
    name: str
    role: str
    # config.rs DEFAULT_MEMBER_COLORS, by join order: this is the color the session
    # itself gives each member.
    color: str


MEMBERS = {
    "pelazas": Member("pelazas", "· hosting", "#4fc3f7"),
    "tis": Member("tis", "· joined", "#7ed67e"),
}


def color(value: str, fallback: str) -> str:
    if value == "default":
        return fallback
    if value in ANSI:
        return ANSI[value]
    return f"#{value}"


def load_fonts() -> tuple[ImageFont.FreeTypeFont, ImageFont.FreeTypeFont]:
    return (
        ImageFont.truetype(str(FONT_PATH), FONT_SIZE, index=0),
        ImageFont.truetype(str(FONT_PATH), FONT_SIZE, index=1),
    )


# Box drawing straight from the font leaves hairline seams between cells, and this UI
# is almost entirely borders. Draw the line glyphs geometrically instead, as
# (up, down, left, right) stroke weights: 0 none, 1 light, 2 heavy.
BOX = {
    "─": (0, 0, 1, 1), "━": (0, 0, 2, 2), "│": (1, 1, 0, 0), "┃": (2, 2, 0, 0),
    "┌": (0, 1, 0, 1), "┏": (0, 2, 0, 2), "┐": (0, 1, 1, 0), "┓": (0, 2, 2, 0),
    "└": (1, 0, 0, 1), "┗": (2, 0, 0, 2), "┘": (1, 0, 1, 0), "┛": (2, 0, 2, 0),
    "├": (1, 1, 0, 1), "┣": (2, 2, 0, 2), "┤": (1, 1, 1, 0), "┫": (2, 2, 2, 0),
    "┬": (0, 1, 1, 1), "┳": (0, 2, 2, 2), "┴": (1, 0, 1, 1), "┻": (2, 0, 2, 2),
    "┼": (1, 1, 1, 1), "╋": (2, 2, 2, 2),
    "╭": (0, 1, 0, 1), "╮": (0, 1, 1, 0), "╰": (1, 0, 0, 1), "╯": (1, 0, 1, 0),
    "╴": (0, 0, 1, 0), "╵": (1, 0, 0, 0), "╶": (0, 0, 0, 1), "╷": (0, 1, 0, 0),
}


def draw_box(draw: ImageDraw.ImageDraw, char: str, left: int, top: int, fill: str) -> None:
    up, down, west, east = BOX[char]
    mid_x, mid_y = left + CELL_W // 2, top + CELL_H // 2
    right, bottom = left + CELL_W, top + CELL_H
    for weight, box in (
        (up, [mid_x, top, mid_x, mid_y]),
        (down, [mid_x, mid_y, mid_x, bottom]),
        (west, [left, mid_y, mid_x, mid_y]),
        (east, [mid_x, mid_y, right, mid_y]),
    ):
        if weight:
            draw.line(box, fill=fill, width=weight)


# --------------------------------------------------------------------- capture


def grab(peer) -> dict:
    """One frame of a peer: every cell's glyph and attributes, plus the cursor."""
    with peer._lock:
        screen = peer._screen
        rows = []
        for y in range(screen.lines):
            source = screen.buffer[y]
            cells: list[tuple | None] = []
            x = 0
            while x < screen.columns:
                char = source[x]
                data = char.data or " "
                cells.append((data, char.fg, char.bg, char.bold, char.reverse))
                width = 2 if wcwidth(data[0]) == 2 else 1
                if width == 2:
                    cells.append(None)
                x += width
            rows.append(cells[: screen.columns])
        cursor = (screen.cursor.x, screen.cursor.y, screen.cursor.hidden)
    return {"rows": rows, "cursor": cursor}


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


def draw_terminal(
    draw: ImageDraw.ImageDraw,
    frame: dict,
    origin: tuple[int, int],
    fonts: tuple[ImageFont.FreeTypeFont, ImageFont.FreeTypeFont],
) -> None:
    regular, bold = fonts
    ox, oy = origin
    cursor_x, cursor_y, cursor_hidden = frame["cursor"]
    for y, row in enumerate(frame["rows"]):
        top = oy + y * CELL_H
        for x, cell in enumerate(row):
            if cell is None:
                continue
            data, fg_name, bg_name, is_bold, reverse = cell
            left = ox + x * CELL_W
            fg = color(fg_name, DEFAULT_FG)
            bg = color(bg_name, DEFAULT_BG)
            if reverse:
                fg, bg = bg, fg
            span = CELL_W * (2 if wcwidth(data[0]) == 2 else 1)
            if bg != DEFAULT_BG:
                draw.rectangle([left, top, left + span - 1, top + CELL_H - 1], fill=bg)
            if data in BOX:
                draw_box(draw, data, left, top, fg)
            elif data != " ":
                draw.text(
                    (left, top + BASELINE_PAD),
                    data,
                    font=bold if is_bold else regular,
                    fill=fg,
                )
    if not cursor_hidden and cursor_y < ROWS and cursor_x < COLS:
        left, top = ox + cursor_x * CELL_W, oy + cursor_y * CELL_H
        draw.rectangle([left, top, left + CELL_W - 1, top + CELL_H - 1], fill="#e8eefc")
        cell = frame["rows"][cursor_y][cursor_x]
        if cell and cell[0] != " ":
            draw.text((left, top + BASELINE_PAD), cell[0], font=regular, fill=CARD_BG)


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
    for index, dot in enumerate(("#ff6159", "#ffbd2e", "#28c941")):
        cx = ox + 16 + index * 15
        cy = oy + CHROME_H // 2
        draw.ellipse([cx - 4, cy - 4, cx + 4, cy + 4], fill=dot)
    # The member's own presence color, the same one the session draws on their tab
    # dot and pane marker, so a viewer can tell the two windows apart at a glance.
    cy = oy + CHROME_H // 2
    draw.ellipse([ox + 74, cy - 4, ox + 82, cy + 4], fill=member.color)
    draw.text((ox + 92, oy + 8), member.name, font=fonts[1], fill=LABEL_TEXT)
    draw.text(
        (ox + 92 + 9 * (len(member.name) + 2), oy + 8),
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


def member_home(harness: Harness, name: str) -> Path:
    """A private HOME per member.

    Both members share one Mac here, but they must not share a session store: a second
    record for the same name is deduplicated to `demo-2`, and the tab bar would then
    show the two members a different session name for the same session.
    """
    home = harness.home / name
    (home / "Library" / "Application Support" / "p2pmux").mkdir(parents=True, exist_ok=True)
    (home / ".config" / "p2pmux").mkdir(parents=True, exist_ok=True)
    return home


def wait_for_ticket(home: Path, timeout: float = 25.0) -> str:
    """The join ticket the coordinator published, read off its own session record.

    Not `Harness.create_room`: that looks the ticket up by the *display* name, while a
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
            if descriptor.get("ticket"):
                return descriptor["ticket"]
        time.sleep(0.1)
    raise AssertionError(f"no ticket appeared in {store} within {timeout}s")


def build_scene(harness: Harness) -> tuple[object, object]:
    """Everything that happens before the camera rolls: a session with two members,
    each hosting one pane."""
    host_home = member_home(harness, "pelazas")
    guest_home = member_home(harness, "tis")

    host = harness.spawn(
        "pelazas",
        ["create", "--name", "pelazas", "--session-name", SESSION],
        cols=COLS,
        rows=ROWS,
        env={"HOME": str(host_home)},
    )
    host.wait_ready(timeout=25)
    ticket = wait_for_ticket(host_home)
    guest = harness.spawn(
        "tis",
        ["join", ticket, "--name", "tis"],
        cols=COLS,
        rows=ROWS,
        env={"HOME": str(guest_home)},
    )
    guest.wait_for(r"Pane #1", timeout=30)

    host.settle(quiet_for=0.5, timeout=8)
    # tis opens their own pane, hosted on tis's Mac, beside pelazas's.
    guest.send(CTRL_P)
    pause(0.4)
    guest.send(b"r")
    guest.wait_for(r"Pane #2", timeout=15)
    host.wait_for(r"Pane #2", timeout=15)
    pause(0.8)
    guest.run_in_shell("uname -sm")
    pause(0.8)

    # Give each pane something on screen, so the recording does not open empty.
    host.run_in_shell('echo "hey tis, grab pane 1"')
    pause(1.0)

    # pelazas typing took the control lease on his own pane, and a lease only clears
    # after ~30 idle seconds (lease::IDLE_AFTER). The recording is about tis claiming a
    # *free* pane, so wait the lease out rather than filming a rejected keystroke.
    print("waiting for pane #1's control lease to expire ...", flush=True)
    for peer in (host, guest):
        peer.wait_for(r"Pane #1 host: pelazas control: free", timeout=60)
    host.settle(quiet_for=0.4, timeout=5)
    guest.settle(quiet_for=0.4, timeout=5)
    return host, guest


def perform(host, guest) -> None:
    """The recorded five seconds."""
    pause(0.4)
    # tis moves focus onto pelazas's pane. Pane mode is sticky, so leave it before
    # typing -- otherwise `e` is read as the rename command, not as a keystroke.
    guest.send(CTRL_P)
    pause(0.3)
    guest.send(LEFT)
    pause(0.45)
    guest.send(ESCAPE)
    pause(0.35)
    # Typing claims the free pane: tis is now driving a shell on pelazas's Mac.
    guest.type('echo "on it"', per_key_delay=0.09)
    pause(0.35)
    guest.send(b"\r")
    pause(1.4)


def encode(frames_dir: Path, output: Path) -> None:
    filters = (
        "[0:v]split[a][b];"
        "[a]palettegen=max_colors=160:stats_mode=diff[p];"
        "[b][p]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle"
    )
    subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-framerate", str(FPS),
            "-i", str(frames_dir / "f%05d.png"),
            "-filter_complex", filters,
            "-loop", "0",
            str(output),
        ],
        check=True,
    )


def render_gif(frames: list[dict], output: Path, frames_dir: Path) -> None:
    fonts = load_fonts()
    if frames_dir.exists():
        shutil.rmtree(frames_dir)
    frames_dir.mkdir(parents=True)
    for index, frame in enumerate(frames):
        render_frame(frame, fonts, MEMBERS).save(frames_dir / f"f{index:05d}.png")
    encode(frames_dir, output)
    seconds = len(frames) / FPS
    size_mb = output.stat().st_size / 1_000_000
    print(f"{output}  ({len(frames)} frames, {seconds:.1f}s, {size_mb:.2f} MB)")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--still", action="store_true", help="render two PNGs, no GIF")
    parser.add_argument("--out", default="docs/media/p2pmux-demo.gif")
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
        host, guest = build_scene(harness)

        if args.still:
            fonts = load_fonts()
            before = {"pelazas": grab(host), "tis": grab(guest)}
            perform(host, guest)
            after = {"pelazas": grab(host), "tis": grab(guest)}
            render_frame(before, fonts, MEMBERS).save(output.with_suffix(".before.png"))
            render_frame(after, fonts, MEMBERS).save(output.with_suffix(".after.png"))
            print(output.with_suffix(".before.png"))
            print(output.with_suffix(".after.png"))
            return 0

        recorder = Recorder({"pelazas": host, "tis": guest})
        recorder.start()
        perform(host, guest)
        recorder.stop()

    capture = Path(tempfile.gettempdir()) / "p2pmux-demo.capture.pkl"
    with open(capture, "wb") as handle:
        pickle.dump(recorder.frames, handle)
    render_gif(recorder.frames, output, frames_dir)
    print(f"capture: {capture}  (re-render with --replay)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
