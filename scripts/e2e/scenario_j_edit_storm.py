#!/usr/bin/env python3
"""Category B x F: three peers racing structural edits while output streams.

Scenario-bank entries combined:
  B. multiplayer structural semantics under genuine concurrency
  F. "type fast while output streams" -- here, *edit* fast while output streams

The layout is a revisioned tree owned by the coordinator, and every structural request
carries a `base_revision` (`layout.rs:546`). Three peers issuing splits at the same
instant means most requests are racing a revision that moved under them, so the
coordinator must reject the losers with `StaleRevision` rather than applying them twice
or corrupting the tree.

The invariant that matters is **convergence**: whatever survives the storm, all three
peers must end up believing the same thing. Peers that disagree about the layout is the
worst multiplayer outcome short of a crash -- a guest deleting "pane 2" would then hit a
different pane than the one it can see.

Run: python3 scripts/e2e/scenario_j_edit_storm.py [repeats]
"""

import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, diff_screens, orphans_after, p2pmux_pids, rss_kb  # noqa: E402

CTRL_P = b"\x10"
CTRL_T = b"\x14"
PANE_HEADER = re.compile(r"Pane #(\d+) host: (\S+)")
ROUNDS = 4


def pane_owners(screen: str) -> tuple:
    """Owners of the panes on this peer's *currently visible* tab, in order.

    Only comparable across peers while they are all looking at the same tab. Which tab a
    peer is viewing is per-peer UI state, exactly like focus, so a peer that creates a tab
    and switches to it is not diverging -- compare `tab_list` for that.
    """
    return tuple(owner for _, owner in PANE_HEADER.findall(screen))


def tab_list(screen: str) -> tuple:
    """Every tab this peer knows about, independent of which one it is viewing."""
    return tuple(re.findall(r"Tab #\d+", screen.split("\n")[0]))


def run_once(index: int, verbose: bool) -> list[tuple[str, bool, str]]:
    results: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        results.append((name, ok, detail if not ok else ""))
        if verbose or not ok:
            print(f"    {'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))

    with Harness(f"j-storm-{index}") as harness:
        host, ticket = harness.create_room("host", cols=140, rows=40)
        g1 = harness.join_room("g1", ticket, cols=140, rows=40)
        g2 = harness.join_room("g2", ticket, cols=140, rows=40)
        peers = (host, g1, g2)
        time.sleep(1.5)

        rss_before = sum(rss_kb(p.pid) for p in peers)

        # Stream output the whole time, so the edit storm races real pane traffic.
        host.type("seq 1 4000")
        host.send(b"\r")
        time.sleep(0.2)

        # --- The storm: all three peers issue splits in the same instant, repeatedly.
        for round_no in range(ROUNDS):
            for peer in peers:
                peer.send(CTRL_P)
            time.sleep(0.3)
            for peer in peers:
                peer.send(b"n")
            time.sleep(2.5)

        for peer in peers:
            peer.settle(quiet_for=1.5, timeout=30)
        time.sleep(2.0)
        for peer in peers:
            peer.settle(quiet_for=1.5, timeout=30)

        # --- Convergence, part 1: panes. Every peer is still on the same tab here,
        #     so their visible pane sets must be identical.
        owners = {p.name: pane_owners(p.snapshot()) for p in peers}
        check(
            "all three peers converge on the same panes after the split storm",
            len(set(owners.values())) == 1,
            "peers disagree:\n" + "\n".join(f"  {n}: {o}" for n, o in owners.items())
            + "\n" + diff_screens("host", host.snapshot(), "g1", g1.snapshot()),
        )

        pane_count = len(owners["host"])
        check(
            "the pane count is sane, not doubled or corrupted",
            1 <= pane_count <= 1 + 3 * ROUNDS,
            f"{pane_count} panes after {ROUNDS} rounds of 3 racing splits",
        )
        check(
            "every pane still has a real owner",
            all(owner in {"host", "g1", "g2"} for owner in owners["host"]),
            f"owners={owners['host']}",
        )
        if verbose:
            print(f"      after split storm: {pane_count} panes, owners={owners['host']}")

        # The session must still work after the storm, not merely look right. Checked
        # here, while every peer is still on the same tab -- after the tab race below,
        # g1 and g2 are viewing their own new tabs and would not see pane 1 at all.
        check("no peer died during the storm", all(p.alive for p in peers),
              f"{[(p.name, p.exit_code) for p in peers]}")

        marker = f"AFTERSTORM{index}"
        host.type(f"echo {marker}")
        host.send(b"\r")
        time.sleep(3.0)
        seen = [p.name for p in peers if marker in p.snapshot()]
        check(
            "the session still streams to every peer after the storm",
            len(seen) == 3,
            f"only {seen} saw it",
        )

        # --- Convergence, part 2: two peers create a tab at the same instant. The tab
        #     *list* must agree everywhere even though each peer views its own tab.
        for peer in (g1, g2):
            peer.send(CTRL_T)
        time.sleep(0.3)
        for peer in (g1, g2):
            peer.send(b"n")
        time.sleep(4.0)
        for peer in peers:
            peer.settle(quiet_for=1.5, timeout=30)

        tabs = {p.name: tab_list(p.snapshot()) for p in peers}
        check(
            "all three peers converge on the same tab list",
            len(set(tabs.values())) == 1 and len(tabs["host"]) >= 2,
            "tab lists disagree:\n" + "\n".join(f"  {n}: {t}" for n, t in tabs.items()),
        )
        if verbose:
            print(f"      after tab race: tabs={tabs['host']}")

        rss_after = sum(rss_kb(p.pid) for p in peers)
        check(
            "no runaway memory from the rejected edits",
            rss_after - rss_before < 400_000,
            f"{rss_before} -> {rss_after} KiB (+{rss_after - rss_before})",
        )

        for peer in peers:
            noise = [ln for ln in peer.raw_text().split("\n") if "panicked at" in ln]
            check(f"no panic on {peer.name}", not noise, "; ".join(noise)[:250])

    return results


def main() -> int:
    repeats = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    baseline = p2pmux_pids()
    runs = []
    print(f"Scenario J: three peers racing structural edits under load ({repeats}x)")
    for index in range(1, repeats + 1):
        print(f"\n  run {index}/{repeats}")
        runs.append(run_once(index, verbose=(repeats == 1)))

    print("\n  per-check failure rate across runs:")
    any_failure = False
    for position, name in enumerate(n for n, _, _ in runs[0]):
        failures = sum(1 for run in runs if position < len(run) and not run[position][1])
        any_failure |= bool(failures)
        print(f"    {failures}/{len(runs)}  {'FAIL' if failures else 'pass'}  {name}")

    leaked = orphans_after(baseline)
    print(f"\n  orphans left behind: {sorted(leaked) if leaked else 'none'}")
    return 1 if (any_failure or leaked) else 0


if __name__ == "__main__":
    sys.exit(main())
