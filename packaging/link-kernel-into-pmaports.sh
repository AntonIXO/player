#!/bin/sh
# Link this repo's custom SDM845 kernel aport into pmbootstrap's pmaports checkout
# so `pmbootstrap build linux-postmarketos-qcom-sdm845-audio` builds straight from
# the repo — no copying, and it survives `pmbootstrap pull`.
#
# Why a renamed package: the upstream linux-postmarketos-qcom-sdm845 lives in
# pmaports' device/community/, and pmbootstrap errors on two aports sharing a
# pkgname — so we can't drop a same-named override into custom-*/. This package is
# therefore renamed to ...-audio and provides/replaces the upstream one (it builds
# the same _flavor, so boot/dtb/module artifacts are identical). pmaports' .gitignore
# whitelists top-level custom-*/ dirs, so the symlink in custom-kernel/ is invisible
# to git and never clobbered by a pull.
#
# Re-run any time (idempotent), e.g. after a fresh `pmbootstrap init`.
set -eu

# This package's dir (next to this script).
SRC="$(cd "$(dirname "$0")" && pwd)/linux-postmarketos-qcom-sdm845-audio"

# pmaports = the last entry of `pmbootstrap config aports` (it is always pmaports),
# falling back to the default location.
PMAPORTS="${1:-$(pmbootstrap config aports 2>/dev/null | tr ',' '\n' | tail -n1)}"
PMAPORTS="${PMAPORTS:-$HOME/.local/var/pmbootstrap/cache_git/pmaports}"

[ -f "$SRC/APKBUILD" ] || { echo "error: $SRC/APKBUILD not found" >&2; exit 1; }
[ -d "$PMAPORTS/.git" ] || { echo "error: pmaports not found at $PMAPORTS (run 'pmbootstrap init'?)" >&2; exit 1; }

mkdir -p "$PMAPORTS/custom-kernel"
ln -sfn "$SRC" "$PMAPORTS/custom-kernel/linux-postmarketos-qcom-sdm845-audio"

echo "linked: $PMAPORTS/custom-kernel/linux-postmarketos-qcom-sdm845-audio -> $SRC"
echo "build with:  pmbootstrap build --arch aarch64 linux-postmarketos-qcom-sdm845-audio"
