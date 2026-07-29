"""Drive a p2pmux peer on a second, real machine over ssh.

Everything in driver.py assumes both peers are on this Mac, which is exactly the
limitation docs/e2e-stress-log.md flags as its standing caveat: localhost proves the
protocol, never the network. This module removes that caveat by running one peer on a
DigitalOcean droplet and driving it through the same `Peer` PTY machinery.

How it works: `ssh -tt` allocates a PTY on the droplet and propagates our local window
size to it, so the remote p2pmux renders into a terminal of the size we asked for and
its bytes come back down the local PTY that `Peer` already pumps into pyte. A remote
peer is therefore assert-able with the same `wait_for` / `snapshot` calls as a local
one; only the launcher differs.

Two things a remote peer does NOT inherit from `Harness`:
  * the sandbox HOME -- the remote side gets its own, passed explicitly via `env`;
  * process reaping -- killing the local ssh does not kill the remote node, so
    scenarios must call `reap()` in a finally block.

The droplet also serves as the network-impairment lab: `netem` shapes latency and loss,
`block_direct_udp` forces the session onto a relay. Neither is possible on macOS without
touching the developer's own network interface, which the e2e rules forbid.
"""

from __future__ import annotations

import shlex
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Sequence

REPO_ROOT = Path(__file__).resolve().parents[2]

DEFAULT_ALIAS = "p2pmux-lab"
DEFAULT_REMOTE_REPO = "/root/p2pmux"
DEFAULT_REMOTE_HOME = "/root/e2e-home"

# The remote peer must never inherit a saved display name, for the same reason the local
# sandbox HOME must not: `create`/`join` block on a name prompt when one is missing, and
# a blocked peer looks exactly like a connectivity failure.
BASE_ENV = {
    "TERM": "xterm-256color",
    "SHELL": "/bin/sh",
    "PS1": "$ ",
}


class RemoteError(RuntimeError):
    pass


@dataclass
class RemoteHost:
    """A droplet that can run p2pmux peers and shape its own network."""

    alias: str = DEFAULT_ALIAS
    repo: str = DEFAULT_REMOTE_REPO
    home: str = DEFAULT_REMOTE_HOME
    env: dict[str, str] = field(default_factory=dict)

    @property
    def binary(self) -> str:
        return f"{self.repo}/target/release/p2pmux"

    # ------------------------------------------------------------------ plumbing

    def run(self, command: str, timeout: float = 60.0, check: bool = True) -> str:
        """Run a shell command on the droplet and return its stdout."""
        result = subprocess.run(
            ["ssh", "-o", "BatchMode=yes", self.alias, command],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if check and result.returncode != 0:
            raise RemoteError(
                f"{self.alias}: `{command}` exited {result.returncode}\n{result.stderr.strip()}"
            )
        return result.stdout

    def public_ip(self) -> str:
        return self.run("curl -s -4 ifconfig.me || hostname -I | awk '{print $1}'").strip()

    @staticmethod
    def local_public_ip() -> str:
        """This Mac's public address, as the droplet would see it after NAT."""
        return subprocess.run(
            ["curl", "-s", "-4", "https://ifconfig.me"],
            capture_output=True,
            text=True,
            timeout=20,
        ).stdout.strip()

    def ping_ms(self, count: int = 5) -> float | None:
        """Round-trip time from this Mac to the droplet, as a latency baseline.

        A p2pmux direct path should land near this number; a relayed path will not.
        """
        try:
            output = subprocess.run(
                ["ping", "-c", str(count), self.host_ip()],
                capture_output=True,
                text=True,
                timeout=30,
            ).stdout
        except (subprocess.TimeoutExpired, RemoteError):
            return None
        for line in output.splitlines():
            if "avg" in line and "/" in line:
                # e.g. round-trip min/avg/max/stddev = 33.4/35.1/38.9/1.9 ms
                numbers = line.rsplit("=", 1)[-1].strip().split()[0].split("/")
                if len(numbers) >= 2:
                    return float(numbers[1])
        return None

    def host_ip(self) -> str:
        """The address ssh actually dials, read from the local ssh config."""
        output = subprocess.run(
            ["ssh", "-G", self.alias], capture_output=True, text=True, timeout=15
        ).stdout
        for line in output.splitlines():
            if line.startswith("hostname "):
                return line.split(None, 1)[1].strip()
        raise RemoteError(f"no hostname for ssh alias {self.alias!r}")

    # ------------------------------------------------------------------- peers

    def launcher(self, extra_env: dict[str, str] | None = None) -> list[str]:
        """The argv prefix that turns `Peer.args` into a command on the droplet.

        `-tt` forces PTY allocation even though our stdin is already a PTY, which is what
        makes the remote p2pmux draw a real TUI instead of detecting a pipe.
        """
        environment = {**BASE_ENV, "HOME": self.home, **self.env, **(extra_env or {})}
        assignments = " ".join(f"{k}={shlex.quote(v)}" for k, v in environment.items())
        return [
            "ssh",
            "-tt",
            "-o",
            "BatchMode=yes",
            self.alias,
            f"env {assignments} {shlex.quote(self.binary)}",
        ]

    def reset_home(self) -> None:
        """Fresh sandbox HOME on the droplet, so a run never sees a stale session."""
        self.reap()
        self.run(f"rm -rf {shlex.quote(self.home)} && mkdir -p {shlex.quote(self.home)}")

    def reap(self) -> list[int]:
        """Kill every p2pmux process on the droplet, including detached `__node`s.

        The droplet is a dedicated lab box, so a blanket kill is correct here -- unlike
        on the developer's Mac, where the harness must pid-diff to spare real sessions.
        """
        before = self._p2pmux_pids()
        if before:
            self.run("pkill -f 'target/release/p2pmux' || true", check=False)
            self.run("sleep 0.5; pkill -9 -f 'target/release/p2pmux' || true", check=False)
        return before

    def _p2pmux_pids(self) -> list[int]:
        output = self.run("pgrep -f 'target/release/p2pmux' || true", check=False)
        return [int(line) for line in output.split() if line.isdigit()]

    # ------------------------------------------------------- build / deployment

    def sync(self) -> None:
        """Mirror this working tree onto the droplet (source only, never target/)."""
        result = subprocess.run(
            [
                "rsync",
                "-az",
                "--delete",
                "--exclude",
                "target",
                "--exclude",
                ".git",
                f"{REPO_ROOT}/",
                f"{self.alias}:{self.repo}/",
            ],
            capture_output=True,
            text=True,
            timeout=300,
        )
        if result.returncode != 0:
            raise RemoteError(f"rsync to {self.alias} failed:\n{result.stderr.strip()}")

    def build(self, timeout: float = 1800.0) -> str:
        return self.run(
            f"cd {shlex.quote(self.repo)} && ~/.cargo/bin/cargo build --release 2>&1 | tail -5",
            timeout=timeout,
        )

    def binary_ready(self) -> bool:
        return bool(self.run(f"test -x {shlex.quote(self.binary)} && echo yes", check=False).strip())

    # -------------------------------------------------------- network impairment

    def clear_netem(self) -> None:
        self.run("tc qdisc del dev eth0 root 2>/dev/null || true", check=False)

    def netem(self, delay_ms: int | None = None, loss_pct: float | None = None) -> None:
        """Shape the droplet's egress: the cheap, repeatable stand-in for a bad path.

        Applied on Linux so the Mac's own interface is never touched (e2e standing rule).
        Note this is one-way shaping, so an RTT grows by roughly `delay_ms`, not twice it.
        """
        self.clear_netem()
        if delay_ms is None and loss_pct is None:
            return
        parts = ["tc qdisc add dev eth0 root netem"]
        if delay_ms is not None:
            parts.append(f"delay {delay_ms}ms")
        if loss_pct is not None:
            parts.append(f"loss {loss_pct}%")
        self.run(" ".join(parts))

    def block_direct_udp(self, peer_ip: str) -> None:
        """Drop UDP to and from one peer address, forcing that session onto a relay.

        The obvious rule -- drop all inbound UDP above 1024 -- is wrong: the relay path
        arrives on an ephemeral port too, so it kills the fallback it is meant to test
        and the run fails for the wrong reason. Filtering on the *peer's* address is the
        precise instrument: hole-punched datagrams from that Mac vanish, while every
        relay server stays fully reachable.
        """
        self.run(f"iptables -I INPUT -p udp -s {peer_ip} -j DROP")
        self.run(f"iptables -I OUTPUT -p udp -d {peer_ip} -j DROP")

    def unblock_direct_udp(self) -> None:
        self.run("iptables -F INPUT; iptables -F OUTPUT", check=False)

    def reset_network(self) -> None:
        self.clear_netem()
        self.unblock_direct_udp()


def spawn_remote(
    harness,
    remote: RemoteHost,
    name: str,
    args: Sequence[str],
    cols: int = 100,
    rows: int = 30,
):
    """`Harness.spawn`, but the peer runs on `remote`."""
    return harness.spawn(
        name,
        list(args),
        cols=cols,
        rows=rows,
        launcher=remote.launcher(),
    )
