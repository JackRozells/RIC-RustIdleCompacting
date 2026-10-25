#!/bin/sh
# idlepack installer for macOS and Linux, in the style of rtk's.
#
#   From a checkout:   sh install.sh
#   From GitHub:       curl -fsSL https://raw.githubusercontent.com/JackRozells/RIC-RustIdleCompacting/main/install.sh | sh
#
# Installs a prebuilt release binary when the repository publishes one for this
# platform (see .github/workflows/release.yml), otherwise builds with cargo, then
# runs `idlepack init`. Variables:
#   IDLEPACK_REPO          owner/name on GitHub (default JackRozells/RIC-RustIdleCompacting)
#   IDLEPACK_BIN_DIR       where a prebuilt binary goes (default ~/.local/bin)
#   IDLEPACK_FROM_SOURCE=1 always build with cargo
#   IDLEPACK_NO_INIT=1     install the binary only
#   IDLEPACK_DOWNLOAD_BASE override the release download URL (testing, mirrors)
set -eu

say() { printf '%s\n' "$*"; }
die() { printf 'idlepack install: %s\n' "$*" >&2; exit 1; }

REPO="${IDLEPACK_REPO:-JackRozells/RIC-RustIdleCompacting}"
BIN_DIR="${IDLEPACK_BIN_DIR:-$HOME/.local/bin}"
HERE=""
case "$0" in
  *install.sh) HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) ;;
esac
if [ -n "$HERE" ] && [ -f "$HERE/Cargo.toml" ] && grep -q '^name = "idlepack"' "$HERE/Cargo.toml"; then
  CHECKOUT="$HERE"
else
  CHECKOUT=""
fi

target_triple() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) return 1 ;;
  esac
  case "$os" in
    Darwin) printf '%s-apple-darwin' "$arch" ;;
    Linux) printf '%s-unknown-linux-gnu' "$arch" ;;
    *) return 1 ;;
  esac
}

fetch() { # url dest
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    return 1
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

installed=""

try_prebuilt() {
  [ "${IDLEPACK_FROM_SOURCE:-0}" = "1" ] && return 1
  triple=$(target_triple) || return 1
  base="${IDLEPACK_DOWNLOAD_BASE:-}"
  if [ -z "$base" ]; then
    [ -n "$REPO" ] || return 1
    base="https://github.com/$REPO/releases/latest/download"
  fi
  tmp=$(mktemp -d)
  asset="idlepack-$triple.tar.gz"
  say "Downloading $asset ..."
  if ! fetch "$base/$asset" "$tmp/$asset"; then
    rm -rf "$tmp"
    say "No prebuilt binary for $triple; building from source instead."
    return 1
  fi
  if fetch "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
    want=$(cut -d' ' -f1 < "$tmp/$asset.sha256")
    got=$(sha256_of "$tmp/$asset")
    [ "$want" = "$got" ] || { rm -rf "$tmp"; die "checksum mismatch for $asset"; }
  fi
  tar -xzf "$tmp/$asset" -C "$tmp"
  mkdir -p "$BIN_DIR"
  install -m 755 "$tmp/idlepack" "$BIN_DIR/idlepack"
  rm -rf "$tmp"
  installed="$BIN_DIR/idlepack"
}

from_source() {
  command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust from https://rustup.rs, then run this again."
  if [ -n "$CHECKOUT" ]; then
    say "Building idlepack from $CHECKOUT ..."
    cargo install --path "$CHECKOUT" --locked --force
  elif [ -n "$REPO" ]; then
    say "Building idlepack from github.com/$REPO ..."
    cargo install --git "https://github.com/$REPO" --locked --force idlepack
  else
    die "run this from a checkout, or set IDLEPACK_REPO=owner/name"
  fi
  installed="${CARGO_HOME:-$HOME/.cargo}/bin/idlepack"
}

if [ -n "$CHECKOUT" ] && [ -z "${IDLEPACK_DOWNLOAD_BASE:-}" ]; then
  from_source
else
  try_prebuilt || from_source
fi

[ -x "$installed" ] || die "the build did not produce $installed"
say "Installed $("$installed" --version) at $installed"
case ":$PATH:" in
  *":$(dirname "$installed"):"*) ;;
  *) say "Note: $(dirname "$installed") is not on your PATH; add it to use \`idlepack\` by name." ;;
esac

if [ "${IDLEPACK_NO_INIT:-0}" != "1" ]; then
  say ""
  "$installed" init
fi
