#!/bin/sh

set -eu

ghostty=/Applications/Ghostty.app/Contents/MacOS/ghostty

probe() {
    mode=$1
    result=$2
    python3 - "$mode" "$result" <<'PY'
import os
import select
import sys
import termios
import tty

mode, result = sys.argv[1:]
fd = sys.stdin.fileno()
try:
    saved = termios.tcgetattr(fd)
    tty.setraw(fd)
except termios.error:
    saved = None
try:
    if mode == "before-alt":
        sequence = b"\x1b[>1u\x1b[?1049h"
    elif mode == "after-alt":
        sequence = b"\x1b[?1049h\x1b[>1u"
    else:
        raise SystemExit(f"unknown probe mode: {mode}")
    os.write(fd, sequence + b"\x1b[?u")
    ready, _, _ = select.select([fd], [], [], 2)
    reply = os.read(fd, 64) if ready else b""
finally:
    os.write(fd, b"\x1b[<1u\x1b[?1049l")
    if saved is not None:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)

with open(result, "w", encoding="ascii") as output:
    output.write(reply.hex() + "\n")
PY
}

if [ "${1:-}" = --probe ]; then
    probe "$2" "$3"
    exit 0
fi

if [ ! -x "$ghostty" ]; then
    echo "SKIP: Ghostty is not installed at $ghostty"
    exit 0
fi

if [ ! -t 0 ] || [ ! -t 1 ]; then
    echo "SKIP: run this script from an interactive Ghostty terminal"
    exit 0
fi

case ${TERM_PROGRAM:-} in
    Ghostty|ghostty) ;;
    *)
        echo "SKIP: run this script from an interactive Ghostty terminal"
        exit 0
        ;;
esac

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/p2pmux-ghostty-order.XXXXXX")
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

probe before-alt "$tmpdir/before-alt"
probe after-alt "$tmpdir/after-alt"
before=$(cat "$tmpdir/before-alt")
after=$(cat "$tmpdir/after-alt")

printf 'Ghostty keyboard flags: before-alt=%s after-alt=%s\n' "$before" "$after"
[ "$before" = 1b5b3f3075 ] || {
    echo "FAIL: expected before-alt reply ESC[?0u" >&2
    exit 1
}
[ "$after" = 1b5b3f3175 ] || {
    echo "FAIL: expected after-alt reply ESC[?1u" >&2
    exit 1
}
