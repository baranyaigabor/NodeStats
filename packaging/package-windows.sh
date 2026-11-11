#!/bin/sh
set -eu
cd /packages
version=$1
release=$2
apt-get update -qq
apt-get install -yqq --no-install-recommends nsis

for arch in arm64 x64; do
    installer="/packages/output/nodestats-$release-windows-$arch.exe"
    makensis -V2 "-DAGENT=/packages/inputs/windows-$arch.exe" "-DOUTPUT=$installer" "-DVERSION=$version" inputs/windows.nsi
done
