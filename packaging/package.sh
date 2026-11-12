#!/bin/sh
set -eu

if [ "$#" -ne 8 ]; then
    echo "Usage: $0 VERSION REVISION MAC_ARM64 MAC_X64 WIN_ARM64 WIN_X64 LINUX_MUSL_ARM64 LINUX_MUSL_X64" >&2
    exit 2
fi

cd "$(dirname "$0")/../.."
version=$1
revision=$2
case "$version" in
    ''|*[!0-9.]*) echo "Invalid version" >&2; exit 2 ;;
esac
case "$revision" in
    ''|*[!a-f0-9]*) echo "Invalid revision" >&2; exit 2 ;;
esac
release="$version-$revision"
mkdir -p .cache
work=$(mktemp -d "$PWD/.cache/node-installers.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
mkdir -p "$work/macos/usr/local/bin" "$work/inputs" "$work/output"
lipo -create "$3" "$4" -output "$work/macos/usr/local/bin/nodestats"
chmod 0755 "$work/macos/usr/local/bin/nodestats"
codesign --force --sign - "$work/macos/usr/local/bin/nodestats"
pkgbuild --root "$work/macos" --identifier com.nodestats.agent --version "$version" --install-location / --scripts client/packaging/macos-scripts \
    "$work/output/nodestats-$release-macos.pkg"

cp "$5" "$work/inputs/windows-arm64.exe"
cp "$6" "$work/inputs/windows-x64.exe"
cp client/packaging/windows.nsi client/packaging/package-windows.sh "$work/inputs/"
docker run --rm --mount "type=bind,source=$work,target=/packages" \
    debian:bookworm-slim sh /packages/inputs/package-windows.sh "$version" "$release"

sh client/packaging/package-linux-portable.sh "$work/output" "$release" "$7" "$8"

(cd "$work/output"; shasum -a 256 ./*.pkg ./*.exe ./*.tar.gz > "nodestats-$release-SHA256SUMS.txt")
cp "$work/output/"* frontend/public/downloads/
