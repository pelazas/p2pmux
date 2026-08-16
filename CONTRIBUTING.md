# Contributing

p2pmux is early. The most useful thing you can send right now is a specific bug report — what you
ran, what two machines were involved, and what happened instead.

## Reporting a bug

Open an issue with:

- The operating system and version, and `p2pmux --version`, on **both** machines. Say which one
  hosted the pane that misbehaved — a Mac and a Linux box in one session are not interchangeable
  when something goes wrong.
- Whether the tab bar said `direct` or `relayed`.
- The exact keys or command that led to it, and what you expected.

A session that hangs, drops, or renders wrong is worth reporting even if you cannot reproduce it.
Say so in the issue — intermittent is still a data point.

## Setting up

Rust stable **1.91 or newer**, on macOS or Linux. There is no other toolchain. The floor is iroh
1.0's, not ours, and `--locked` builds fail outright below it.

```sh
git clone https://github.com/pelazas/p2pmux
cd p2pmux
cargo build
cargo run -- create
```

`cargo run -- create` gives you a real session. Open a second terminal, `Ctrl+S` in the first for
the join code, and `cargo run -- join <code>` in the second to exercise both sides on one machine.

## Before you open a PR

CI runs these four on `macos-latest` **and** `ubuntu-latest`, and a PR that fails any of them on
either will not be looked at until it passes:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo check --all-targets --all-features
```

A few tests talk to the network or spawn PTYs and flake on a loaded machine. If one fails, rerun it
alone before assuming your change caused it.

## What gets merged

Small, focused PRs. One behaviour change per PR, with the reasoning in the description — not just
what changed but what was wrong before.

Before writing anything substantial, open an issue and describe the approach. p2pmux has a
[locked MVP design](./docs/MVP_DESIGN.md), and a change that cuts against it needs a conversation
first, not a review comment on 600 lines of finished work.

Docs, error messages, and anything that makes a failure mode legible are always welcome and need no
issue first.

## Scope

Things p2pmux is not going to become: a cloud VM, a remote box everyone's processes run on, an
agent orchestration platform, or a sandbox around the shell it hands you.
[docs/PRODUCT.md](./docs/PRODUCT.md) has the full is/isn't list. Reading it will save you a
rejected PR.

## Cutting a release

Tag-driven, so whatever `v*` points at is what gets built, hashed and published:

```sh
# bump `version` in Cargo.toml, add the release to CHANGELOG.md — including its
# compatibility line — and bump the version on services/site/public/index.html
cargo build                       # so Cargo.lock follows
git commit -am "release: vX.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main && git push origin vX.Y.Z
```

`release.yml` then builds all four targets natively, verifies every archive against its own
published SHA256, publishes the GitHub release, and points the Homebrew tap at it using those
same verified hashes. Nothing about the tap is manual — it used to be, and v0.1.2 shipped for
several minutes with a formula nobody could install from.

**One-time setup:** the tap lives in another repo, which `GITHUB_TOKEN` cannot reach. Create a
fine-grained personal access token with `contents: write` on `pelazas/homebrew-tap` and add it
to this repo as the `HOMEBREW_TAP_TOKEN` secret. Without it the `homebrew` job fails on
purpose rather than skipping quietly — the release itself is already published by then, so a
red job there costs the formula, not the release.

## License

Contributions are accepted under the [MIT license](./LICENSE).
