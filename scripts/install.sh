#!/bin/sh
set -eu

REPO="${NODESTATS_REPO:-baranyaigabor/NodeStats}"
INSTALL_DIR="${NODESTATS_INSTALL_DIR:-/usr/local/bin}"

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "Missing required command: $1" >&2
        exit 1
    fi
}

need_cmd curl
need_cmd tar

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux) OS="linux" ;;
    darwin) OS="macos" ;;
    *)
        echo "Unsupported OS: $OS (download binaries directly from https://github.com/$REPO/releases/latest)" >&2
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64|amd64)
        if [ "$OS" = "linux" ]; then ARCH="amd64"; else ARCH="x64"; fi
        ;;
    arm64|aarch64) ARCH="arm64" ;;
    *)
        echo "Unsupported architecture: $ARCH" >&2
        exit 1
        ;;
esac

if [ -n "${NODESTATS_VERSION:-}" ]; then
    VERSION="${NODESTATS_VERSION#v}"
    BASE_URL="https://github.com/$REPO/releases/download/v$VERSION"
else
    VERSION=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed -n 's#.*/tag/v\([^/]*\)$#\1#p')
    if [ -z "$VERSION" ]; then
        echo "Could not resolve latest release version from https://github.com/$REPO/releases/latest" >&2
        exit 1
    fi
    BASE_URL="https://github.com/$REPO/releases/download/v$VERSION"
fi

ARCHIVE="nodestats-$VERSION-$OS-$ARCH.tar.gz"
SUMS="nodestats-$VERSION-SHA256SUMS.txt"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT INT HUP TERM

echo "Downloading NodeStats v$VERSION ($OS/$ARCH)..."
curl -fsSL "$BASE_URL/$ARCHIVE" -o "$TMPDIR/$ARCHIVE"
curl -fsSL "$BASE_URL/$SUMS" -o "$TMPDIR/$SUMS"

EXPECTED=$(awk -v f="$ARCHIVE" '$2 == f || $2 == ("./" f) { print $1; exit }' "$TMPDIR/$SUMS")
if [ -z "$EXPECTED" ]; then
    echo "Checksum entry not found for $ARCHIVE in $SUMS" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "$TMPDIR/$ARCHIVE" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL=$(shasum -a 256 "$TMPDIR/$ARCHIVE" | awk '{print $1}')
else
    echo "Missing sha256sum or shasum to verify download integrity" >&2
    exit 1
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "SHA-256 verification failed for $ARCHIVE" >&2
    exit 1
fi

tar -xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"

if [ ! -f "$TMPDIR/nodestats" ]; then
    echo "Extracted archive did not contain nodestats binary" >&2
    exit 1
fi

if [ -w "$INSTALL_DIR" ] || { [ ! -d "$INSTALL_DIR" ] && [ -w "$(dirname "$INSTALL_DIR")" ]; }; then
    mkdir -p "$INSTALL_DIR"
    install -m 0755 "$TMPDIR/nodestats" "$INSTALL_DIR/nodestats"
else
    need_cmd sudo
    sudo install -d -m 0755 "$INSTALL_DIR"
    sudo install -m 0755 "$TMPDIR/nodestats" "$INSTALL_DIR/nodestats"
fi

echo "Installed nodestats v$VERSION to $INSTALL_DIR/nodestats"
echo "Run the following to pair this host and start background telemetry:"
echo "  nodestats setup --server https://nodestats.hu"
