"""Draw a live p2pmux peer's screen as a terminal card, and encode a sequence of them.

Both demo recorders point a camera at the same thing — a `driver.Peer`'s pyte screen —
and differ only in what they arrange on the canvas and what story they drive. This module
is the camera: cell capture, glyph rendering, and the ffmpeg call at the end.

Every glyph it draws came out of the binary. Nothing here invents UI.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont
from wcwidth import wcwidth

# Menlo at 15px is exactly 9x18 per cell with no rounding, and carries the ✓/✗/●/◆
# glyphs the inbox draws.
FONT_PATH = Path("/System/Library/Fonts/Menlo.ttc")
FONT_SIZE = 15
CELL_W, CELL_H = 9, 18
BASELINE_PAD = 0

PAGE_BG = "#07080b"
CARD_BG = "#0d1016"
CHROME_BG = "#171b24"
CHROME_TEXT = "#7d879c"
LABEL_TEXT = "#c3cbd9"

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


def load_ui_font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont:
    """A proportional face for the parts of the frame that are not a terminal.

    Captions in Menlo read as more terminal, which is the one thing they must not
    be mistaken for: everything in a monospace cell came from the binary.
    """
    candidates = [
        "/System/Library/Fonts/SFNSDisplay.ttf",
        "/System/Library/Fonts/Supplemental/HelveticaNeue.ttc",
        "/System/Library/Fonts/Helvetica.ttc",
    ]
    for path in candidates:
        if Path(path).exists():
            try:
                font = ImageFont.truetype(path, size)
            except OSError:
                continue
            if bold:
                try:
                    return ImageFont.truetype(path, size, index=1)
                except OSError:
                    return font
            return font
    return ImageFont.truetype(str(FONT_PATH), size)


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


def draw_terminal(
    draw: ImageDraw.ImageDraw,
    frame: dict,
    origin: tuple[int, int],
    fonts: tuple[ImageFont.FreeTypeFont, ImageFont.FreeTypeFont],
) -> None:
    """Paint one captured screen. Its size is the frame's own, not a global."""
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
    rows = frame["rows"]
    if not cursor_hidden and cursor_y < len(rows) and cursor_x < len(rows[cursor_y]):
        left, top = ox + cursor_x * CELL_W, oy + cursor_y * CELL_H
        draw.rectangle([left, top, left + CELL_W - 1, top + CELL_H - 1], fill="#e8eefc")
        cell = rows[cursor_y][cursor_x]
        if cell and cell[0] != " ":
            draw.text((left, top + BASELINE_PAD), cell[0], font=regular, fill=CARD_BG)


def encode_gif(frames_dir: Path, output: Path, fps: int) -> None:
    filters = (
        "[0:v]split[a][b];"
        "[a]palettegen=max_colors=160:stats_mode=diff[p];"
        "[b][p]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle"
    )
    subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-framerate", str(fps),
            "-i", str(frames_dir / "f%05d.png"),
            "-filter_complex", filters,
            "-loop", "0",
            str(output),
        ],
        check=True,
    )


def encode_mp4(frames_dir: Path, output: Path, fps: int) -> None:
    """H.264, and `-pix_fmt yuv420p` because everything else refuses to play it.

    `scale=…:trunc(…/2)*2` is not optional either: yuv420p needs even dimensions and
    a card grid lands on an odd one about half the time.
    """
    subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-framerate", str(fps),
            "-i", str(frames_dir / "f%05d.png"),
            "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-c:v", "libx264",
            "-preset", "slow",
            "-crf", "20",
            "-pix_fmt", "yuv420p",
            "-movflags", "+faststart",
            str(output),
        ],
        check=True,
    )
