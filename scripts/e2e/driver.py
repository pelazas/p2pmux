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
*not* kill it. Orphans between iterations would poison later runs, so at teardown the
harness kills the nodes it can trace back to its own peers -- by the `node_pid` each
one recorded in this run's scratch session store, and by the parent link captured
before the peers die.

Isolation: p2pmux resolves its session store and config file from $HOME. Each Harness
gets a private scratch HOME, so a run can never see, disturb, or kill the developer's
real p2pmux sessions -- including one started
*during* the run, which "anything that is not in the starting pid snapshot" would
have swept up.
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
from wcwidth import wcwidth

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
    # The four mux mode keys the manual is written around. Without them a scenario can
    # reach a pane's shell but not the multiplexer wrapped around it -- no split, no tab,
    # no share panel, no agents overlay.
    "ctrl_a": b"\x01",
    "ctrl_p": b"\x10",
    "ctrl_s": b"\x13",
    "ctrl_t": b"\x14",
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


# What a p2pmux pane puts in its children's environment, and what a hook reads to
# decide it is running in one.
PANE_ENV = ("P2PMUX_PANE_ID", "P2PMUX_SOCK")


def sandbox_environ() -> dict[str, str]:
    """This process's environment with any live pane's markers taken out.

    The suite is routinely run from a terminal inside p2pmux -- that is what
    dogfooding is -- and every process it starts inherited that pane's id and
    socket. A hook fired by a scenario's "loose" agent therefore reported to the
    developer's own session instead of writing the machine-local record the
    scenario was about, and two checks in scenario Y failed on a developer's
    machine while passing in a bare shell. Scenarios that want these set pass
    them explicitly.
    """
    return {key: value for key, value in os.environ.items() if key not in PANE_ENV}


@dataclass
class Peer:
    """One p2pmux process on its own PTY, with a live pyte-rendered screen."""

    name: str
    args: Sequence[str]
    cols: int = DEFAULT_COLS
    rows: int = DEFAULT_ROWS
    env: dict[str, str] = field(default_factory=dict)
    cwd: Path | None = None
    # When set, argv becomes [*launcher, *args] instead of [BINARY, *args], and this
    # process's env is *not* forwarded: a remote launcher carries its own environment.
    # See remote.py -- this is how a peer runs on another machine over ssh.
    launcher: Sequence[str] | None = None

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
        if self.launcher is None and not BINARY.exists():
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

        argv = (
            [*self.launcher, *self.args]
            if self.launcher is not None
            else [str(BINARY), *self.args]
        )
        self.process = subprocess.Popen(
            argv,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            preexec_fn=_become_session_leader,
            env={**sandbox_environ(), **self.env},
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

    # SGR (1006) mouse reports, which is what the client enables via EnableMouseCapture.
    # Coordinates are 1-based, matching what a real terminal emits.
    def _sgr_mouse(self, code: int, col: int, row: int, release: bool = False) -> None:
        self.send(f"\x1b[<{code};{col};{row}{'m' if release else 'M'}".encode())

    def wheel_up(self, col: int, row: int, times: int = 1, delay: float = 0.12) -> None:
        for _ in range(times):
            self._sgr_mouse(64, col, row)
            time.sleep(delay)

    def wheel_down(self, col: int, row: int, times: int = 1, delay: float = 0.12) -> None:
        for _ in range(times):
            self._sgr_mouse(65, col, row)
            time.sleep(delay)

    def click(self, col: int, row: int, delay: float = 0.15) -> None:
        self._sgr_mouse(0, col, row)
        time.sleep(delay)
        self._sgr_mouse(0, col, row, release=True)
        time.sleep(delay)

    def resize(self, cols: int, rows: int) -> None:
        """Resize this peer's terminal, delivering a real SIGWINCH."""
        self.cols, self.rows = cols, rows
        _set_winsize(self.master_fd, rows, cols)
        with self._lock:
            self._screen.resize(rows, cols)

    def signal(self, sig: int) -> None:
        os.kill(self.pid, sig)

    # ------------------------------------------------------------------ output

    def _render_row(self, y: int) -> str:
        """One rendered row, wide-char aware.

        Deliberately not pyte's own `Screen.display`: that does `char[0]` on every cell
        and raises IndexError when it meets the empty continuation cell that wide
        characters and emoji leave behind (pyte/screens.py:241). Since these scenarios
        stream CJK and emoji through panes on purpose, the harness has to render them
        itself rather than crash on the exact content it is meant to be checking.
        """
        row = self._screen.buffer[y]
        out: list[str] = []
        x = 0
        while x < self._screen.columns:
            data = row[x].data
            if not data:
                out.append(" ")
                x += 1
                continue
            out.append(data)
            x += 2 if wcwidth(data[0]) == 2 else 1
        return "".join(out)

    def snapshot(self) -> str:
        """The currently rendered screen as text, trailing blanks stripped."""
        with self._lock:
            lines = [self._render_row(y).rstrip() for y in range(self._screen.lines)]
        return "\n".join(lines)

    def cursor(self) -> tuple[int, int]:
        """(row, col) of the rendered cursor."""
        with self._lock:
            return self._screen.cursor.y, self._screen.cursor.x

    def cursor_hidden(self) -> bool:
        """Whether the caret is off, which is a claim in its own right.

        p2pmux hides it deliberately -- a pane scrolled into history is not
        showing where its program's cursor is, and a dialog that has taken the
        keyboard has taken the caret with it -- so a scenario needs to be able
        to assert the absence and not only the position.
        """
        with self._lock:
            return bool(self._screen.cursor.hidden)

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
    """Every live p2pmux process (foreground clients and detached __node workers).

    Keyed on the executable, not on "p2pmux" appearing anywhere in the command line:
    the repo path is `.../p2pmux`, so every `cargo` and `rustc` process building it --
    plus the developer's editor, grep, and shell -- matches a substring test.
    """
    return {
        pid
        for pid, _, command in _ps_table()
        if command and os.path.basename(command.split(None, 1)[0]) == "p2pmux"
    }


def orphans_after(baseline: set[int], settle: float = 4.0) -> set[int]:
    """p2pmux processes still alive `settle` seconds after teardown.

    Sampling the instant a scenario ends reports transients as leaks. Not every p2pmux
    process is a session: `p2pmux notify` is spawned per agent hook, runs for
    milliseconds and exits, and one caught mid-flight looks identical to a leaked node.
    A node that genuinely leaked is still there after the grace window -- it lives until
    something kills it -- so waiting costs nothing and removes the false positive.
    """
    deadline = time.monotonic() + settle
    leaked = p2pmux_pids() - baseline
    while leaked and time.monotonic() < deadline:
        time.sleep(0.25)
        leaked = p2pmux_pids() - baseline
    return leaked


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
        launcher: Sequence[str] | None = None,
    ) -> Peer:
        """Start one p2pmux peer on its own PTY inside this harness's sandbox HOME.

        With `launcher` set the peer runs on another machine (see remote.py); the
        sandbox HOME below is this Mac's and does not apply to it.
        """
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
            launcher=list(launcher) if launcher is not None else None,
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
        """Start a host peer and return it with its join ticket.

        The ticket is read off the session record rather than the screen: invite material
        lives behind Ctrl+S now, and a ~200-character ticket would never have fitted in a
        footer anyway.

        `--session-name` is not decoration. `create` now hands every session a random
        world-city name, and `--name` sets only the *display* name -- so looking the record
        up by the peer's name found nothing, and every scenario built on `create_room` died
        at its first step. Pinning the session name is what makes the record addressable.
        """
        host = self.spawn(
            name,
            ["create", "--name", name, "--session-name", name],
            cols=cols,
            rows=rows,
        )
        host.wait_ready(timeout=timeout)
        return host, self.wait_for_ticket(name, timeout=timeout)

    def wait_for_ticket(self, name: str, timeout: float = 25.0) -> str:
        """The ticket the coordinator's node published for session `name`.

        `name` is the *session* name (`--session-name`), not the peer's display name.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for descriptor in self.session_descriptors():
                if descriptor.get("name") == name and descriptor.get("ticket"):
                    return descriptor["ticket"]
            time.sleep(0.1)
        raise AssertionError(f"no ticket appeared for session {name!r} within {timeout}s")

    def join_room(
        self,
        name: str,
        ticket: str,
        cols: int = DEFAULT_COLS,
        rows: int = DEFAULT_ROWS,
        timeout: float = 30.0,
    ) -> Peer:
        """Join an existing room and wait until the shared layout has rendered."""
        guest = self.spawn(name, ["join", ticket, "--name", name], cols=cols, rows=rows)
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

    def _own_node_pids(self) -> set[int]:
        """Node pids this harness's peers created, from their scratch session store.

        Every peer runs with `HOME=self.home`, so this store lists exactly the sessions
        this run is responsible for, and each descriptor records its detached node's pid.
        """
        store = self.home / "Library" / "Application Support" / "p2pmux" / "sessions"
        pids: set[int] = set()
        for path in store.glob("*.json"):
            try:
                node_pid = json.loads(path.read_text()).get("node_pid")
            except (OSError, ValueError):
                continue  # a descriptor being written right now is not worth failing on
            if isinstance(node_pid, int) and node_pid > 0:
                pids.add(node_pid)
        return pids

    def __exit__(self, exc_type, exc, tb) -> bool:
        # Snapshot what we own *before* closing anything: once a foreground client dies
        # its detached node is reparented, and the parent link that identifies it is gone.
        owned = self._own_node_pids()
        for peer in self.peers:
            if peer.process is not None:
                owned.add(peer.pid)
                owned.update(descendants(peer.pid))

        for peer in reversed(self.peers):
            try:
                peer.close()
            except Exception as cleanup_error:  # never mask the scenario's failure
                print(f"[harness] closing {peer.name} failed: {cleanup_error}", file=sys.stderr)

        # Detached `p2pmux __node` workers survive their foreground client by design, so
        # they need an explicit kill. Only pids traced back to this run's own peers are
        # touched: a session the developer starts in another terminal *while* a scenario
        # is running is also absent from the baseline, and killing it makes p2pmux look
        # like it died on its own.
        deadline = time.monotonic() + 5.0
        clean_passes = 0
        while time.monotonic() < deadline:
            # Re-read the store every pass: a node still starting when the scenario blew
            # up registers itself a moment later, and one snapshot would miss it. The
            # store lives in this run's scratch HOME, so it can only ever name our own.
            # Two clean passes in a row before leaving, for the same reason.
            owned |= self._own_node_pids()
            orphans = owned & p2pmux_pids()
            if not orphans:
                clean_passes += 1
                if clean_passes >= 2:
                    break
                time.sleep(0.25)
                continue
            clean_passes = 0
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

        live = p2pmux_pids()
        leaked = owned & live
        if leaked:
            print(f"[harness] WARNING leaked p2pmux pids: {sorted(leaked)}", file=sys.stderr)

        # Reported, never killed -- most likely the developer's own session, but if a
        # scenario really does leak a node we cannot trace, this is where it shows up.
        strays = live - self._baseline_pids - owned
        if strays:
            print(
                f"[harness] note: p2pmux pids appeared during this run that are not "
                f"ours; left running: {sorted(strays)}",
                file=sys.stderr,
            )

        if not self.keep_home:
            shutil.rmtree(self.home, ignore_errors=True)
        return False  # never swallow scenario exceptions


# `direct 55ms`, `relayed 120ms ×3`, `locked · direct <1ms` -- the connectivity badge
# p2pmux draws at the right edge of the tab bar.
LINK_BADGE = re.compile(r"(?:locked · )?(?:direct|relayed|other)(?: (?:<1|\d+)ms)?(?: ×\d+)?")


def agent_panel(screen: str) -> str:
    """Just the rows inside the Agents overlay panel.

    Any assertion about a detected agent has to be scoped to the panel. The scenarios
    launch their fake agent as `exec -a claude ...`, often after `cd`-ing into the
    directory they then look for, so the kind *and* the working directory both appear in
    the shell's own command line on the screen behind the overlay. A bare
    `"claude" in screen` passes whether or not detection ever fired -- the exact opposite
    of what those scenarios exist to prove.

    Note the panel prints the lowercase kind (`claude`, `codex`), not the display label:
    `AgentKind::display_label` is no longer what the overlay renders. The panel drawing
    in docs/USAGE.md is the reference.
    """
    rows: list[str] = []
    inside = False
    for line in screen.split("\n"):
        if not inside and "Agents" in line and "┌" in line:
            inside = True
            continue
        if inside:
            if "└" in line:
                break
            rows.append(line)
    return "\n".join(rows)


def mask_link_badge(screen: str) -> str:
    """Blank the tab bar's connectivity badge so two screens can be compared.

    The badge carries a live RTT, so it legitimately differs between any two snapshots
    taken seconds apart. Any assertion that compares whole rendered screens has to mask
    it or it reports a phantom failure every time the network jitters by 5ms -- the same
    class of oracle bug as keying on `Pane #N`, which is a display ordinal.

    Only the first line is touched: the badge lives in the tab bar and nowhere else.
    """
    rows = screen.split("\n")
    if rows:
        rows[0] = LINK_BADGE.sub(lambda match: " " * len(match.group(0)), rows[0])
    return "\n".join(rows)


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
