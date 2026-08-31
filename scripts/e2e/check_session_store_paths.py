#!/usr/bin/env python3
"""Issue #122: the harness finds session records on both supported store paths.

Run: python3 scripts/e2e/check_session_store_paths.py
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from driver import Harness, session_store_dirs_for  # noqa: E402


def write_record(store: Path, name: str, session_id: str) -> None:
    store.mkdir(parents=True, exist_ok=True)
    (store / f"{name}.json").write_text(
        json.dumps({"id": session_id, "ticket": f"ticket-for-{session_id}"})
    )


def descriptors_for(home: Path) -> list[dict]:
    harness = object.__new__(Harness)
    harness.home = home
    return harness.session_descriptors()


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="p2pmux-session-store-") as temporary:
        home = Path(temporary)
        macos, linux = session_store_dirs_for(home)

        write_record(linux, "linux", "linux-id")
        assert [record["id"] for record in descriptors_for(home)] == ["linux-id"]

        (linux / "linux.json").unlink()
        write_record(macos, "macos", "macos-id")
        assert [record["id"] for record in descriptors_for(home)] == ["macos-id"]

        write_record(linux, "linux", "linux-id")
        assert {record["id"] for record in descriptors_for(home)} == {"macos-id", "linux-id"}

        (linux / "invalid.json").write_text("not json")
        assert {record["id"] for record in descriptors_for(home)} == {"macos-id", "linux-id"}

        previous = os.environ.get("XDG_STATE_HOME")
        os.environ["XDG_STATE_HOME"] = "/tmp/p2pmux-developer-state"
        try:
            assert all(directory.is_relative_to(home) for directory in session_store_dirs_for(home))
        finally:
            if previous is None:
                del os.environ["XDG_STATE_HOME"]
            else:
                os.environ["XDG_STATE_HOME"] = previous

    print("session store paths: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
