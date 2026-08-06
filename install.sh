#!/bin/sh
# cona installer — works two ways:
#
#   From a source checkout:  ./install.sh [--bin-dir DIR]
#       builds with cargo and wires upgrade git hooks.
#
#   Without Rust (curl/wget one-liner):
#       curl -fsSL https://raw.githubusercontent.com/ph1p/cona/main/install.sh | sh
#       wget -qO- https://raw.githubusercontent.com/ph1p/cona/main/install.sh | sh
#       downloads the prebuilt release binary — no toolchain needed.
#
# Env overrides (binary mode):
#   BIN_DIR=DIR        install dir   (default: ~/.local/bin)
#   CONA_VERSION=X  pin a version (default: latest release)
set -eu

REPO="ph1p/cona"

# --- source-checkout mode: build + `cona install` -----------------------
# Only when run as a file sitting next to Cargo.toml (not piped through sh).
src_dir=""
case "${0:-}" in
    */*) src_dir="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || src_dir="" ;;
esac
[ -n "$src_dir" ] && [ -f "$src_dir/Cargo.toml" ] || src_dir=""

if [ -n "$src_dir" ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "error: cargo not found — install Rust first (https://rustup.rs)," >&2
        echo "       or install the prebuilt binary:" >&2
        echo "       curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | sh" >&2
        exit 1
    fi
    cd "$src_dir"
    cargo build --release
    # `cona install` wires git hooks and prints next-steps itself
    exec ./target/release/cona install "$@"
fi

# --- binary mode: download the prebuilt release ----------------------------
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
err() { echo "error: $*" >&2; exit 1; }

if command -v curl >/dev/null 2>&1; then
    dl() { curl -fsSL -A cona-install "$1" -o "$2"; }
    fetch() { curl -fsSL -A cona-install "$1"; }
elif command -v wget >/dev/null 2>&1; then
    dl() { wget -qO "$2" "$1"; }
    fetch() { wget -qO- "$1"; }
else
    err "need curl or wget (or run ./install.sh from a source checkout with cargo)"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)  osp="unknown-linux-gnu" ;;
    Darwin) osp="apple-darwin" ;;
    *)      err "no prebuilt binary for $os — grab the Windows zip from releases, or build from source" ;;
esac
case "$arch" in
    x86_64|amd64)  archp="x86_64" ;;
    arm64|aarch64) archp="aarch64" ;;
    *) err "unsupported arch: $arch" ;;
esac
target="${archp}-${osp}"

ver="${CONA_VERSION:-}"
if [ -z "$ver" ]; then
    ver="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name":[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -n1)"
    [ -n "$ver" ] || err "could not resolve latest version — set CONA_VERSION"
fi
ver="${ver#v}"

url="https://github.com/$REPO/releases/download/v${ver}/cona-v${ver}-${target}.tar.gz"
echo "cona v${ver} (${target}) -> ${BIN_DIR}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
dl "$url" "$tmp/cona.tar.gz" || err "download failed: $url"

# Verify against the release's checksum sidecar — the Rust self-upgrade path
# refuses unverified binaries, and this path must not be weaker. Mismatch is
# fatal; a missing sidecar (older release) or missing hash tool only warns.
if dl "$url.sha256" "$tmp/cona.tar.gz.sha256" 2>/dev/null; then
    want="$(awk '{print $1}' "$tmp/cona.tar.gz.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
        got="$(sha256sum "$tmp/cona.tar.gz" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        got="$(shasum -a 256 "$tmp/cona.tar.gz" | awk '{print $1}')"
    elif command -v openssl >/dev/null 2>&1; then
        got="$(openssl dgst -sha256 -r "$tmp/cona.tar.gz" | awk '{print $1}')"
    else
        got=""
        echo "warn: no sha256 tool found — skipping checksum verification"
    fi
    if [ -n "$got" ]; then
        [ "$got" = "$want" ] || err "checksum mismatch for $url"
        echo "checksum ok"
    fi
else
    echo "warn: no checksum sidecar for this release — skipping verification"
fi

tar -xf "$tmp/cona.tar.gz" -C "$tmp" || err "extract failed"
[ -f "$tmp/cona" ] || err "binary not found in archive"

mkdir -p "$BIN_DIR"
install -m 0755 "$tmp/cona" "$BIN_DIR/cona"
"$BIN_DIR/cona" --version >/dev/null 2>&1 || err "installed binary won't run"
echo "installed: $BIN_DIR/cona"

# Optional semantic-resolve helper: bundled beside cona in the tarball for
# targets where it builds. Install it as a sibling so cona finds it; absence
# is fine (cona degrades to its name-based + arity heuristics).
if [ -f "$tmp/cona-resolve-helper" ]; then
    install -m 0755 "$tmp/cona-resolve-helper" "$BIN_DIR/cona-resolve-helper"
    echo "installed: $BIN_DIR/cona-resolve-helper (semantic resolve)"
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on PATH — add: export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

# git-hook wiring is source-only; `cona setup` records install_path + wires agents.
cat <<EOF

next steps
  cona setup            interactive setup — index this project + wire agent integration
  cona setup project    project only (git hooks, .claude/, CLAUDE.md, AGENTS.md, …)
  cona setup global     global only (~/.claude, ~/.codex, … home configs)
  cona doctor           verify the installation (binary, PATH, hooks, skill, index)

run setup inside each project you want indexed — then agents pick it up automatically
EOF
