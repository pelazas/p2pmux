#!/bin/sh
# p2pmux installer.
#
# Read this before you run it. You are about to install a program that, when you share a
# join ticket, lets whoever holds it run commands as your user account. That is the
# product, not a flaw — but it means you should read installers like this one rather than
# pipe them blind. This script is served as text/plain so you can.
#
#   curl -fsSL https://p2pmux.com/install.sh | sh
#
# What it does: works out your system and CPU, downloads that build and its SHA256 from
# GitHub Releases, checks the hash, and copies one binary into place. Nothing else. No
# launch agents, no shell-rc edits, no telemetry.
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
  Darwin) PLATFORM="apple-darwin" ;;
  Linux) PLATFORM="unknown-linux-gnu" ;;
  *) die "p2pmux supports macOS and Linux. Build from source:
  cargo install --git https://github.com/$REPO --locked" ;;
esac

# macOS says arm64 where Linux says aarch64, and both mean the same silicon.
case "$(uname -m)" in
  arm64 | aarch64) ARCH="aarch64-$PLATFORM" ;;
  x86_64 | amd64) ARCH="x86_64-$PLATFORM" ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac

# The published Linux builds link glibc. On a musl system — Alpine, and most of what
# people build containers out of — the binary installs fine and then fails to start with
# a message about a missing loader, which reads like a corrupt download. Say so here
# instead.
if [ "$PLATFORM" = "unknown-linux-gnu" ] && (ldd --version 2>&1 || true) | grep -qi musl; then
  die "this looks like a musl system, and the published Linux builds need glibc.
Build from source:
  cargo install --git https://github.com/$REPO --locked"
fi

for tool in curl tar install; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# Same hash, two names: macOS ships `shasum`, Linux ships `sha256sum`.
if command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1"; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1"; }
else
  die "missing required tool: shasum or sha256sum"
fi

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

say "Downloading ${ASSET}..."
curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP/$ASSET" "$BASE/$ASSET" || die \
  "could not download $BASE/$ASSET
If this is the first release, it may not be published yet. Build from source:
  cargo install --git https://github.com/$REPO --locked"
curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256" || die \
  "could not download the checksum for $ASSET"

# The point of the checksum is that it is published beside the artifact on GitHub, so a
# compromised p2pmux.com can break this install but cannot substitute a binary.
EXPECTED="$(cut -d' ' -f1 < "$TMP/$ASSET.sha256")"
ACTUAL="$(sha256_of "$TMP/$ASSET" | cut -d' ' -f1)"
[ -n "$EXPECTED" ] || die "the published checksum was empty; refusing to install"
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch — refusing to install
  expected $EXPECTED
  actual   $ACTUAL"

tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -f "$TMP/p2pmux" ] || die "the archive did not contain a p2pmux binary"

# Having `sudo` on PATH is not the same as being able to use it. A locked-down work
# laptop or a stock container ships the binary without a sudoers entry for you, and a
# piped installer has no terminal to type a password into. So run it inside an `if`
# rather than as a bare command: `set -e` would abort on the failure and never reach
# the home-directory branch that exists for exactly these machines.
sudo_install() {
  command -v sudo >/dev/null 2>&1 || return 1
  say "$INSTALL_DIR is not writable; using sudo."
  sudo mkdir -p "$INSTALL_DIR" || return 1
  sudo install -m 0755 "$TMP/p2pmux" "$INSTALL_DIR/p2pmux" || return 1
}

if mkdir -p "$INSTALL_DIR" 2>/dev/null && [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$TMP/p2pmux" "$INSTALL_DIR/p2pmux"
elif sudo_install; then
  : # installed with elevated privileges
else
  # /usr/local/bin is root-owned on most Linux systems, and plenty of the machines
  # someone would run this on — a container, a locked-down work laptop — either have no
  # sudo at all or have one you are not allowed to use. A home directory needs no
  # privileges, and the PATH note below covers the distributions that do not already add
  # this one.
  INSTALL_DIR="$HOME/.local/bin"
  say "could not install to a system directory; falling back to $INSTALL_DIR."
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$TMP/p2pmux" "$INSTALL_DIR/p2pmux"
fi

say ""
say "Installed $INSTALL_DIR/p2pmux"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "NOTE: $INSTALL_DIR is not on your PATH." ;;
esac
say ""
say "  p2pmux               you host; Ctrl+S shows the line to send"
say "  p2pmux join <code>   them, on their own machine"
say ""
say "Read https://p2pmux.com/trust before you share a code with anyone."
