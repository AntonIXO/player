#!/bin/sh
# Link this repo's hifi-player aport into pmbootstrap's pmaports checkout so
# `pmbootstrap build hifi-player` builds straight from the repo — no copying,
# and it survives `pmbootstrap pull`.
#
# How it works: pmaports' .gitignore whitelists top-level `custom-*/` dirs for
# exactly this ("Allow having untracked packages in aports using a custom-something
# directory"). We make a real custom-player/ dir there and symlink the package
# into it. Because the package lives *inside* pmaports it inherits the correct
# channel (e.g. systemd-edge) — unlike a separate `config aports` overlay, which
# would become pkgrepo[0] and flip the primary channel detection.
#
# Re-run this any time (idempotent), e.g. after a fresh `pmbootstrap init`.
set -eu

# This package's dir (next to this script).
SRC="$(cd "$(dirname "$0")" && pwd)/hifi-player"

# pmaports = the last entry of `pmbootstrap config aports` (it is always pmaports),
# falling back to the default location.
PMAPORTS="${1:-$(pmbootstrap config aports 2>/dev/null | tr ',' '\n' | tail -n1)}"
PMAPORTS="${PMAPORTS:-$HOME/.local/var/pmbootstrap/cache_git/pmaports}"

[ -f "$SRC/APKBUILD" ] || { echo "error: $SRC/APKBUILD not found" >&2; exit 1; }
[ -d "$PMAPORTS/.git" ] || { echo "error: pmaports not found at $PMAPORTS (run 'pmbootstrap init'?)" >&2; exit 1; }

mkdir -p "$PMAPORTS/custom-player"
ln -sfn "$SRC" "$PMAPORTS/custom-player/hifi-player"

echo "linked: $PMAPORTS/custom-player/hifi-player -> $SRC"
echo "build with:  pmbootstrap build --src=\"$(cd "$(dirname "$0")/../.." && pwd)\" hifi-player"
