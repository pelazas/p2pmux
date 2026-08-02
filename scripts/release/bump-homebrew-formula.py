#!/usr/bin/env python3
"""Point the Homebrew formula at a release, using hashes the workflow already verified.

Each `sha256` is rewritten by locating its own archive's `url` line and replacing the
`sha256` that follows it, so the four targets can never be transposed — the failure a
positional edit invites, and the one nobody notices until an install refuses to run.
"""
import pathlib, re, sys

formula_path, version, dist = pathlib.Path(sys.argv[1]), sys.argv[2], pathlib.Path(sys.argv[3])
formula = formula_path.read_text()

hashes = {}
for line in sorted(dist.glob("*.sha256")):
    digest, name = line.read_text().split()
    hashes[name] = digest
if len(hashes) != 4:
    sys.exit(f"expected 4 archives, found {sorted(hashes)}")

before = formula
formula, count = re.subn(r'^(\s*version )"[^"]*"', rf'\1"{version}"', formula, flags=re.M)
if count != 1:
    sys.exit(f"expected exactly one version line, rewrote {count}")

for archive, digest in hashes.items():
    pattern = re.compile(
        rf'(url "[^"]*/{re.escape(archive)}"\s*\n\s*sha256 )"[^"]*"'
    )
    formula, count = pattern.subn(rf'\1"{digest}"', formula)
    if count != 1:
        sys.exit(f"expected exactly one sha256 for {archive}, rewrote {count}")

if formula == before:
    print("formula already current")
    sys.exit(0)
formula_path.write_text(formula)
print(f"formula now at {version}")
