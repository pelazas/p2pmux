"""Reusable end-to-end driver for p2pmux.

Every peer in a scenario is a real `target/release/p2pmux ...` process on its own PTY.
Nothing here mocks p2pmux: we write bytes into a master fd and read the bytes the real
binary rendered back out.

Design rules (non-negotiable, see docs/e2e-stress-log.md):
  * every read has a deadline -- no unbounded blocking anywhere;
  * every spawned process gets a hard kill (SIGTERM then SIGKILL) on scenario exit;
  * `Harness` is a context manager that reaps *everything*, including the detached
    `p2pmux __node` background processes, even when the scenario body raises.

Why the node reaping matters: `p2pmux create` / `p2pmux join` fork a background
"node" process with its own process group (see cli.rs launch_background_node), which
owns the PTYs and outlives the foreground client. Killing the peer we spawned does
*not* kill it. Orphans between iterations would poison later runs, so the harness
snapshots the pre-existing p2pmux pids and kills anything new at teardown.

Isolation: p2pmux resolves its session store, config file and join-code rendezvous
directory from $HOME. Each Harness gets a private scratch HOME, so a run can never
see, disturb, or kill the developer's real p2pmux sessions.
"""

from __future__ import annotations

import errno
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Iterable, Sequence

import pyte

REPO_ROOT = Path(__file__).resolve().parents[2]
BINARY = REPO_ROOT / "target" / "release" / "p2pmux"

DEFAULT_COLS = 100
DEFAULT_ROWS = 30

# Keystrokes p2pmux understands, so scenarios read like user actions.
KEYS = {
    "enter": b"\r",
    "escape": b"\x1b",
    "backspace": b"\x7f",
    "tab": b"\t",
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "right": b"\x1b[C",
    "left": b"\x1b[D",
    "ctrl_q": b"\x11",
    "ctrl_c": b"\x03",
    "ctrl_d": b"\x04",
    "ctrl_b": b"\x02",
}


class AltScreen(pyte.Screen):
    """pyte.Screen plus alternate-screen support.

    p2pmux enters the alt screen (`ESC [ ? 1049 h`) before drawing its TUI. Stock
    pyte ignores 1049, so anything printed to the terminal beforehand -- notably the
    multi-line TRUST WARNING that `create`/`join` print -- stays on the grid and
    bleeds through wherever the TUI does not repaint. That looks exactly like a
    garbled render, which is precisely the bug class these scenarios hunt for, so
    the harness has to model it correctly or it will manufacture false positives.
    """

    def set_mode(self, *modes: int, **kwargs: object) -> None:
        super().set_mode(*modes, **kwargs)
        if kwargs.get("private") and 1049 in modes:
            self.reset()

    def reset_mode(self, *modes: int, **kwargs: object) -> None:
        super().reset_mode(*modes, **kwargs)
        if kwargs.get("private") and 1049 in modes:
            self.reset()


class DeadlineExceeded(AssertionError):
    """A screen never reached the expected state before its deadline."""


class PeerDied(AssertionError):
    """A peer process exited while a scenario still needed it."""


def _set_winsize(fd: int, rows: int, cols: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


@dataclass
class Peer:
    """One p2pmux process on its own PTY, with a live pyte-rendered screen."""

    name: str
    args: Sequence[str]
    cols: int = DEFAULT_COLS
    rows: int = DEFAULT_ROWS
    env: dict[str, str] = field(default_factory=dict)
    cwd: Path | None = None

    process: subprocess.Popen | None = field(default=None, init=False)
    master_fd: int = field(default=-1, init=False)
    raw: bytearray = field(default_factory=bytearray, init=False)

    _screen: pyte.Screen = field(default=None, init=False)  # type: ignore[assignment]
    _stream: pyte.ByteStream = field(default=None, init=False)  # type: ignore[assignment]
    _lock: threading.Lock = field(default_factory=threading.Lock, init=False)
    _reader: threading.Thread | None = field(default=None, init=False)
    _stop: threading.Event = field(default_factory=threading.Event, init=False)
    _eof: threading.Event = field(default_factory=threading.Event, init=False)

    # ---------------------------------------------------------------- lifecycle

    def start(self) -> "Peer":
        if not BINARY.exists():
            raise FileNotFoundError(f"{BINARY} missing -- run: cargo build --release")

        self._screen = AltScreen(self.cols, self.rows)
        self._stream = pyte.ByteStream(self._screen)

        master_fd, slave_fd = pty.openpty()
        _set_winsize(slave_fd, self.rows, self.cols)

        def _become_session_leader() -> None:
            os.setsid()
            # Claim the slave as this process's controlling terminal, otherwise
            # crossterm's raw mode and any child shell job control misbehave.
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [str(BINARY), *self.args],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            preexec_fn=_become_session_leader,
            env={**os.environ, **self.env},
            cwd=str(self.cwd) if self.cwd else None,
            close_fds=True,
        )
        os.close(slave_fd)
        self.master_fd = master_fd
        os.set_blocking(master_fd, False)

        self._reader = threading.Thread(
            target=self._pump, name=f"peer-{self.name}", daemon=True
        )
        self._reader.start()
        return self

    def _pump(self) -> None:
        """Drain the PTY forever, feeding pyte. Never blocks longer than 100ms."""
        while not self._stop.is_set():
            try:
                ready, _, _ = select.select([self.master_fd], [], [], 0.1)
            except (OSError, ValueError):
                break
            if not ready:
                continue
            try:
                chunk = os.read(self.master_fd, 65536)
            except OSError as exc:
                # EIO is the normal "child closed the slave side" signal on Linux/macOS.
                if exc.errno in (errno.EIO, errno.EBADF):
                    self._eof.set()
                    break
                if exc.errno == errno.EAGAIN:
                    continue
                break
            if not chunk:
                self._eof.set()
                break
            with self._lock:
                self.raw.extend(chunk)
                self._stream.feed(chunk)

    def close(self, grace: float = 2.0) -> None:
        """Hard-kill this peer and its process group. Safe to call twice."""
        self._stop.set()
        process = self.process
        if process is not None and process.poll() is None:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(os.getpgid(process.pid), sig)
                except (ProcessLookupError, PermissionError):
                    break
                deadline = time.monotonic() + grace
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        break
                    time.sleep(0.02)
                if process.poll() is not None:
                    break
        if process is not None:
            try:
                process.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                pass
        if self._reader is not None:
            self._reader.join(timeout=grace)
        if self.master_fd >= 0:
            try:
                os.close(self.master_fd)
            except OSError:
                pass
            self.master_fd = -1

    @property
    def pid(self) -> int:
        if self.process is None:
            raise RuntimeError(f"peer {self.name} not started")
        return self.process.pid

    @property
    def alive(self) -> bool:
        return self.process is not None and self.process.poll() is None

    @property
    def exit_code(self) -> int | None:
        return None if self.process is None else self.process.poll()

    # ------------------------------------------------------------------- input

    def send(self, data: bytes | str) -> None:
        """Write raw bytes to the peer's PTY, exactly as a keyboard would."""
        if isinstance(data, str):
            data = data.encode()
        if self.master_fd < 0:
            raise PeerDied(f"peer {self.name} has no live PTY")
        total = 0
        deadline = time.monotonic() + 5.0
        while total < len(data):
            if time.monotonic() > deadline:
                raise DeadlineExceeded(f"peer {self.name}: PTY write stalled")
            _, writable, _ = select.select([], [self.master_fd], [], 0.25)
            if not writable:
                continue
            total += os.write(self.master_fd, data[total:])

    def key(self, name: str, times: int = 1, delay: float = 0.02) -> None:
        """Send a named key (see KEYS) one or more times."""
        for _ in range(times):
            self.send(KEYS[name])
            time.sleep(delay)

    def type(self, text: str, per_key_delay: float = 0.01) -> None:
        """Type text one byte at a time, like a human, not one big paste."""
        for char in text:
            self.send(char.encode())
            time.sleep(per_key_delay)

    def run_in_shell(self, command: str) -> None:
        """Type a shell command into the focused pane and press Enter."""
        self.type(command)
        time.sleep(0.05)
        self.send(KEYS["enter"])

    def resize(self, cols: int, rows: int) -> None:
        """Resize this peer's terminal, delivering a real SIGWINCH."""
        self.cols, self.rows = cols, rows
        _set_winsize(self.master_fd, rows, cols)
        with self._lock:
            self._screen.resize(rows, cols)

    def signal(self, sig: int) -> None:
        os.kill(self.pid, sig)

    # ------------------------------------------------------------------ output

    def snapshot(self) -> str:
        """The currently rendered screen as text, trailing blanks stripped."""
        with self._lock:
            lines = [line.rstrip() for line in self._screen.display]
        return "\n".join(lines)

    def cursor(self) -> tuple[int, int]:
        """(row, col) of the rendered cursor."""
        with self._lock:
            return self._screen.cursor.y, self._screen.cursor.x

    def raw_text(self) -> str:
        with self._lock:
            return self.raw.decode("utf-8", errors="replace")

    def wait_for(
        self,
        pattern: str | re.Pattern[str],
        timeout: float = 10.0,
        poll: float = 0.05,
    ) -> str:
        """Block until the rendered screen matches, or raise. Never waits forever."""
        regex = re.compile(pattern) if isinstance(pattern, str) else pattern
        return self.wait_until(
            lambda screen: bool(regex.search(screen)),
            timeout=timeout,
            poll=poll,
            what=f"screen matching /{regex.pattern}/",
        )

    def wait_until(
        self,
        predicate: Callable[[str], bool],
        timeout: float = 10.0,
        poll: float = 0.05,
        what: str = "predicate",
    ) -> str:
        deadline = time.monotonic() + timeout
        screen = self.snapshot()
        while time.monotonic() < deadline:
            screen = self.snapshot()
            if predicate(screen):
                return screen
            if not self.alive and self._eof.is_set():
                # One last look: output may have landed in the same instant it exited.
                screen = self.snapshot()
                if predicate(screen):
                    return screen
                raise PeerDied(
                    f"peer {self.name} exited (code {self.exit_code}) while waiting for "
                    f"{what}\n--- last screen ---\n{screen}"
                )
            time.sleep(poll)
        raise DeadlineExceeded(
            f"peer {self.name}: timed out after {timeout}s waiting for {what}\n"
            f"--- last screen ---\n{screen}"
        )

    def wait_ready(self, timeout: float = 15.0) -> str:
        """Wait for the first non-blank frame. A peer that has drawn nothing yet is
        not 'settled', it just has not started -- see settle(require_content=)."""
        return self.wait_until(
            lambda screen: bool(screen.strip()),
            timeout=timeout,
            what="first non-blank frame",
        )

    def settle(
        self,
        quiet_for: float = 0.4,
        timeout: float = 5.0,
        require_content: bool = True,
    ) -> str:
        """Wait until the screen stops changing for `quiet_for` seconds.

        `require_content` (default) refuses to call an all-blank screen settled:
        a peer that has not drawn its first frame yet is quiet for the boring
        reason, and returning that blank frame silently breaks assertions.
        """
        deadline = time.monotonic() + timeout
        last, last_change = self.snapshot(), time.monotonic()
        while time.monotonic() < deadline:
            time.sleep(0.05)
            current = self.snapshot()
            if current != last:
                last, last_change = current, time.monotonic()
                continue
            if require_content and not current.strip():
                continue
            if time.monotonic() - last_change >= quiet_for:
                return current
        return self.snapshot()


# ------------------------------------------------------------------ memory/procs


def _ps_table() -> list[tuple[int, int, str]]:
    """(pid, ppid, command) for every process visible to this user."""
    out = subprocess.run(
        ["ps", "-Ao", "pid=,ppid=,command="],
        capture_output=True,
        text=True,
        timeout=15,
    ).stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 3:
            continue
        try:
            rows.append((int(parts[0]), int(parts[1]), parts[2]))
        except ValueError:
            continue
    return rows


def descendants(pid: int) -> list[int]:
    """Every transitive child of pid."""
    children: dict[int, list[int]] = {}
    for child, parent, _ in _ps_table():
        children.setdefault(parent, []).append(child)
    found, queue = [], [pid]
    while queue:
        current = queue.pop()
        for child in children.get(current, []):
            found.append(child)
            queue.append(child)
    return found


def rss_kb(pid: int, include_children: bool = True) -> int:
    """Resident set size in KiB for a process, optionally plus all its children."""
    pids = [pid] + (descendants(pid) if include_children else [])
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", ",".join(str(p) for p in pids)],
        capture_output=True,
        text=True,
        timeout=15,
    ).stdout
    return sum(int(value) for value in out.split() if value.isdigit())


def p2pmux_pids() -> set[int]:
    """Every live p2pmux process (foreground clients and detached __node workers)."""
    return {
        pid
        for pid, _, command in _ps_table()
        if "p2pmux" in command and "ps -Ao" not in command
    }


# ----------------------------------------------------------------------- harness


class Harness:
    """Context manager owning a scenario's peers, scratch HOME, and cleanup.

    Guarantees on exit -- including when the scenario body raises -- that every peer
    and every detached p2pmux node started during the run is dead.
    """

    def __init__(self, scenario: str, keep_home: bool = False) -> None:
        self.scenario = scenario
        self.keep_home = keep_home
        self.peers: list[Peer] = []
        self.home = Path(tempfile.mkdtemp(prefix="p2pmux-e2e-"))
        self._baseline_pids: set[int] = set()
        self.killed_orphans: list[int] = []

    def __enter__(self) -> "Harness":
        self._baseline_pids = p2pmux_pids()
        (self.home / ".config" / "p2pmux").mkdir(parents=True, exist_ok=True)
        (self.home / "Library" / "Application Support" / "p2pmux").mkdir(
            parents=True, exist_ok=True
        )
        return self

    def spawn(
        self,
        name: str,
        args: Sequence[str],
        cols: int = DEFAULT_COLS,
        rows: int = DEFAULT_ROWS,
        env: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> Peer:
        """Start one p2pmux peer on its own PTY inside this harness's sandbox HOME."""
        peer_env = {
            "HOME": str(self.home),
            "TERM": "xterm-256color",
            "SHELL": "/bin/sh",
            # Keep the child shell's prompt boring so assertions are stable.
            "PS1": "$ ",
            **(env or {}),
        }
        peer = Peer(
            name=name,
            args=list(args),
            cols=cols,
            rows=rows,
            env=peer_env,
            cwd=cwd or REPO_ROOT,
        )
        self.peers.append(peer)
        peer.start()
        return peer

    def create_room(
        self,
        name: str = "host",
        cols: int = DEFAULT_COLS,
        rows: int = DEFAULT_ROWS,
        timeout: float = 25.0,
    ) -> tuple[Peer, str]:
        """Start a host peer and return it with its live join code."""
        host = self.spawn(name, ["create", "--name", name], cols=cols, rows=rows)
        host.wait_ready(timeout=timeout)
        screen = host.wait_for(r"p2pmux join [A-Za-z0-9]{6,}", timeout=timeout)
        return host, join_code_from(screen)

    def join_room(
        self,
        name: str,
        code: str,
        cols: int = DEFAULT_COLS,
        rows: int = DEFAULT_ROWS,
        timeout: float = 30.0,
    ) -> Peer:
        """Join an existing room and wait until the shared layout has rendered."""
        guest = self.spawn(name, ["join", code, "--name", name], cols=cols, rows=rows)
        guest.wait_ready(timeout=timeout)
        guest.wait_for(r"Pane #\d+", timeout=timeout)
        return guest

    def session_descriptors(self) -> list[dict]:
        """The session records p2pmux wrote inside this sandbox HOME."""
        store = self.home / "Library" / "Application Support" / "p2pmux" / "sessions"
        found = []
        for path in sorted(store.glob("*.json")):
            try:
                found.append(json.loads(path.read_text()))
            except (OSError, json.JSONDecodeError):
                continue
        return found

    def node_pids(self) -> dict[str, int]:
        """{session_id: pid} for the detached `__node` workers owned by this sandbox.

        The foreground peer is only a renderer; the node holds the PTYs and the session.
        Killing a *peer* is a detach, killing its *node* is the real disconnect, so
        failure scenarios need to address them separately.
        """
        ids = {descriptor["id"] for descriptor in self.session_descriptors()}
        found: dict[str, int] = {}
        for pid, _, command in _ps_table():
            if "__node" not in command:
                continue
            for session_id in ids:
                if session_id in command:
                    found[session_id] = pid
        return found

    def node_pid_for_role(self, role: str) -> int | None:
        """pid of the node whose descriptor has this role, e.g. 'Coordinator'."""
        pids = self.node_pids()
        for descriptor in self.session_descriptors():
            if str(descriptor.get("role", "")).lower() == role.lower():
                return pids.get(descriptor["id"])
        return None

    def peer(self, name: str) -> Peer:
        for peer in self.peers:
            if peer.name == name:
                return peer
        raise KeyError(name)

    def total_rss_kb(self) -> int:
        return sum(rss_kb(peer.pid) for peer in self.peers if peer.alive)

    def __exit__(self, exc_type, exc, tb) -> bool:
        for peer in reversed(self.peers):
            try:
                peer.close()
            except Exception as cleanup_error:  # never mask the scenario's failure
                print(f"[harness] closing {peer.name} failed: {cleanup_error}", file=sys.stderr)

        # Detached `p2pmux __node` workers survive their foreground client by design.
        # Anything that appeared after we started is ours; the developer's real
        # sessions were in the baseline and are left strictly alone.
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            orphans = p2pmux_pids() - self._baseline_pids
            if not orphans:
                break
            for pid in orphans:
                self.killed_orphans.append(pid)
                for sig in (signal.SIGTERM, signal.SIGKILL):
                    try:
                        os.kill(pid, sig)
                    except ProcessLookupError:
                        break
                    time.sleep(0.15)
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        break
            time.sleep(0.2)

        leaked = p2pmux_pids() - self._baseline_pids
        if leaked:
            print(f"[harness] WARNING leaked p2pmux pids: {sorted(leaked)}", file=sys.stderr)

        if not self.keep_home:
            shutil.rmtree(self.home, ignore_errors=True)
        return False  # never swallow scenario exceptions


def join_code_from(screen: str) -> str:
    """Pull the 10-character join code out of a host's `p2pmux create` output."""
    match = re.search(r"p2pmux join ([A-Za-z0-9]{6,})", screen)
    if not match:
        raise AssertionError(f"no join code on screen:\n{screen}")
    return match.group(1)


def diff_screens(label_a: str, a: str, label_b: str, b: str) -> str:
    """Human-readable per-line diff of two rendered screens."""
    rows_a, rows_b = a.split("\n"), b.split("\n")
    lines = []
    for index in range(max(len(rows_a), len(rows_b))):
        left = rows_a[index] if index < len(rows_a) else "<missing>"
        right = rows_b[index] if index < len(rows_b) else "<missing>"
        if left != right:
            lines.append(f"  row {index:>2} | {label_a}: {left!r}")
            lines.append(f"         | {label_b}: {right!r}")
    return "\n".join(lines) if lines else "  (identical)"
