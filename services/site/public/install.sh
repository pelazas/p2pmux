#!/bin/sh
# p2pmux installer.
#
# Read this before you run it. You are about to install a program that, when you share a
# join ticket, lets whoever holds it run commands as your macOS user. That is the product,
# not a flaw — but it means you should read installers like this one rather than pipe them
# blind. This script is served as text/plain so you can.
#
#   curl -fsSL https://p2pmux.com/install.sh | sh
#
# What it does: works out your CPU, downloads that build and its SHA256 from GitHub
# Releases, checks the hash, and copies one binary into place. Nothing else. No launch
# agents, no shell-rc edits, no telemetry.
#
# Binaries come from GitHub Releases, never from p2pmux.com. Whoever controls the domain can
# break your install; they cannot hand you a different binary than the one published and
# hashed on GitHub.
#
# Prefer to build it yourself:
#   cargo install --git https://github.com/pelazas/p2pmux --locked

set -eu

REPO="pelazas/p2pmux"
INSTALL_DIR="${P2PMUX_INSTALL_DIR:-/usr/local/bin}"
TAG="${P2PMUX_VERSION:-latest}"

say() { printf '%s\n' "$*" >&2; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) ;;
  Linux) die "Linux builds are not published yet. Build from source:
  cargo install --git https://github.com/$REPO --locked" ;;
  *) die "p2pmux supports macOS today. Build from source:
  cargo install --git https://github.com/$REPO --locked" ;;
esac

case "$(uname -m)" in
  arm64) ARCH="aarch64-apple-darwin" ;;
  x86_64) ARCH="x86_64-apple-darwin" ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac

for tool in curl shasum tar install; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

if [ "$TAG" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$TAG"
fi
ASSET="p2pmux-$ARCH.tar.gz"

TMP="$(mktemp -d)"
# `set -e` plus a trap: an interrupted download must not leave a half-extracted binary or a
# temp directory behind.
trap 'rm -rf "$TMP"' EXIT INT TERM

say "Downloading $ASSET…"
curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP/$ASSET" "$BASE/$ASSET" || die \
  "could not download $BASE/$ASSET
If this is the first release, it may not be published yet. Build from source:
  cargo install --git https://github.com/$REPO --locked"
curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256" || die \
  "could not download the checksum for $ASSET"

# The point of the checksum is that it is published beside the artifact on GitHub, so a
# compromised p2pmux.com can break this install but cannot substitute a binary.
EXPECTED="$(cut -d' ' -f1 < "$TMP/$ASSET.sha256")"
ACTUAL="$(shasum -a 256 "$TMP/$ASSET" | cut -d' ' -f1)"
[ -n "$EXPECTED" ] || die "the published checksum was empty; refusing to install"
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch — refusing to install
  expected $EXPECTED
  actual   $ACTUAL"

tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -f "$TMP/p2pmux" ] || die "the archive did not contain a p2pmux binary"

if [ -w "$INSTALL_DIR" ] || [ ! -e "$INSTALL_DIR" ]; then
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$TMP/p2pmux" "$INSTALL_DIR/p2pmux"
else
  say "$INSTALL_DIR is not writable; using sudo."
  sudo mkdir -p "$INSTALL_DIR"
  sudo install -m 0755 "$TMP/p2pmux" "$INSTALL_DIR/p2pmux"
fi

say ""
say "Installed $INSTALL_DIR/p2pmux"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "NOTE: $INSTALL_DIR is not on your PATH." ;;
esac
say ""
say "  p2pmux create        start a session, then Ctrl+S for the join code"
say "  p2pmux join <code>   join one"
say ""
say "Read https://p2pmux.com/trust before you share a code with anyone."
