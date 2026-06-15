#!/usr/bin/env bash
#
# pmb-build-pgo.sh — build the `hifi-player` aport with PGO trained on the REAL
# music library, in one command.
#
# pmbootstrap's `--src` rsyncs the working tree but excludes everything in
# .gitignore (all audio formats), and `mount --bind` isn't recursive, so the
# library can't ride into the aarch64 build chroot the usual way. This wrapper
# bind-mounts the library READ-ONLY into the buildroot chroot at /mnt/pgo-music
# (which the APKBUILD auto-detects and trains PGO on: varied FLAC/MP3/ALAC decode
# + seek, the bounded SACD .iso DST decoder, a real Cyrillic-tagged library scan,
# and fuzzy/FTS search), runs `pmbootstrap build`, then unmounts on exit.
#
# Usage:
#   scripts/pmb-build-pgo.sh [--music DIR] [--src DIR] [--no-music] [-- <pmb args>]
#
# Defaults: --music /home/antonix/Музыка, --src <repo root>. Needs sudo for the
# bind mount. Without a library (or --no-music) the build still works — the
# APKBUILD falls back to its synthetic training corpus.
#
# Tune the workload via env (forwarded is unnecessary — set them in the APKBUILD
# or here is informational only): PLAYER_PGO_DECODE_SECS, PLAYER_PGO_MAX_PER_CODEC,
# PLAYER_PGO_ISO_START, PLAYER_PGO_ISO_SECS, PLAYER_PGO_MUSIC.
set -euo pipefail

PKG="hifi-player"
MUSIC="/home/antonix/Музыка"
SRC="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || echo /home/antonix/player)"
NO_MUSIC=0
EXTRA=()

while [ $# -gt 0 ]; do
	case "$1" in
		--music) MUSIC="$2"; shift 2 ;;
		--src)   SRC="$2";   shift 2 ;;
		--no-music) NO_MUSIC=1; shift ;;
		-h|--help) sed -n '3,28p' "$0"; exit 0 ;;
		--) shift; EXTRA+=("$@"); break ;;
		*) EXTRA+=("$1"); shift ;;
	esac
done

# Resolve the pmbootstrap workdir → the aarch64 buildroot chroot.
WORK="$(pmbootstrap config work 2>/dev/null | tail -n1)"
[ -n "$WORK" ] || WORK="$HOME/.local/var/pmbootstrap"
CHROOT="$WORK/chroot_buildroot_aarch64"
MNT="$CHROOT/mnt/pgo-music"

cleanup() {
	if mountpoint -q "$MNT" 2>/dev/null; then
		sudo umount "$MNT" 2>/dev/null || sudo umount -l "$MNT" 2>/dev/null || true
		echo "→ unmounted $MNT"
	fi
}
trap cleanup EXIT INT TERM

if [ "$NO_MUSIC" = 0 ]; then
	if [ ! -d "$MUSIC" ]; then
		echo "error: music dir not found: $MUSIC  (use --no-music to build with the synth corpus)" >&2
		exit 1
	fi
	# Make sure the buildroot chroot exists before we mount into it.
	echo "→ ensuring the aarch64 buildroot chroot exists…"
	pmbootstrap -y chroot -b aarch64 -- /bin/true >/dev/null 2>&1 || true
	if [ ! -d "$CHROOT" ]; then
		echo "error: buildroot chroot not found at $CHROOT" >&2
		exit 1
	fi
	sudo mkdir -p "$MNT"
	if ! mountpoint -q "$MNT"; then
		echo "→ bind-mounting (read-only) $MUSIC → $MNT"
		sudo mount --bind "$MUSIC" "$MNT"
		sudo mount -o remount,ro,bind "$MNT"
	else
		echo "→ already mounted: $MNT"
	fi
else
	echo "→ --no-music: building with the synthetic PGO corpus"
fi

echo "→ pmbootstrap build --src=$SRC $PKG ${EXTRA[*]:-}"
echo "  (watch for 'PGO: real-corpus workload …' — that confirms the library is in use)"
pmbootstrap --details-to-stdout build --src="$SRC" "$PKG" ${EXTRA[@]+"${EXTRA[@]}"}
