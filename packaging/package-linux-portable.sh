#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
    echo "Usage: $0 OUTPUT_DIRECTORY RELEASE LINUX_MUSL_ARM64 LINUX_MUSL_X64" >&2
    exit 2
fi

output=$1
release=$2
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
trap 'exit 1' HUP INT TERM

# Suppress macOS metadata when packaging Linux executables on a Mac.
export COPYFILE_DISABLE=1
package() {
    case "$(LC_ALL=C file -b "$2")" in
        *ELF*"$3"*statically\ linked*|*ELF*"$3"*static-pie\ linked*) ;;
        *) echo "Expected a static Linux $1 executable: $2" >&2; exit 1 ;;
    esac
    install -m 0755 "$2" "$staging/nodestats"
    tar --format=ustar -czf "$output/nodestats-$release-linux-$1.tar.gz" -C "$staging" nodestats
}

package arm64 "$3" aarch64
package amd64 "$4" x86-64
