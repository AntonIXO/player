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
# With --bolt it ALSO stages a BOLT toolchain READ-ONLY at /mnt/x86-bolt in a bin/lib
# layout: the host's x86_64 llvm-bolt + merge-fdata (+ closure) under bin/, run
# NATIVELY (fast), PLUS an *aarch64* libbolt_rt_instr.a under lib/. BOLT links that
# runtime INTO the target, so it must match the target arch; it's lifted from a
# downloaded aarch64 LLVM (auto-detected as LLVM-*-Linux-ARM64 in the repo, or pass
# --bolt-rt DIR). The APKBUILD then BOLTs player-cli + player-gtk; only the instrumented
# binary's training run is emulated. Needs a host llvm-bolt (Arch: `pacman -S llvm`)
# AND the aarch64 runtime, else --bolt errors out.
#
# Usage:
#   scripts/pmb-build-pgo.sh [--music DIR] [--src DIR] [--no-music] [--bolt [--bolt-rt DIR]] [-- <pmb args>]
#
# Defaults: --music /home/antonix/Музыка, --src <repo root>, BOLT off. Needs sudo for the
# bind mount(s). Without a library (or --no-music) the build still works — the
# APKBUILD falls back to its synthetic training corpus; without --bolt, PGO-only.
#
# Tune the workload via env (forwarded is unnecessary — set them in the APKBUILD
# or here is informational only): PLAYER_PGO_DECODE_SECS, PLAYER_PGO_MAX_PER_CODEC,
# PLAYER_PGO_ISO_START, PLAYER_PGO_ISO_SECS, PLAYER_PGO_MUSIC.
set -euo pipefail

PKG="hifi-player"
MUSIC="/home/antonix/Музыка"
SRC="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || echo /home/antonix/player)"
NO_MUSIC=0
BOLT=0
BOLT_RT=""
EXTRA=()

while [ $# -gt 0 ]; do
	case "$1" in
		--music) MUSIC="$2"; shift 2 ;;
		--src)   SRC="$2";   shift 2 ;;
		--no-music) NO_MUSIC=1; shift ;;
		--bolt) BOLT=1; shift ;;
		--no-bolt) BOLT=0; shift ;;
		--bolt-rt) BOLT_RT="$2"; shift 2 ;;
		-h|--help) sed -n '3,32p' "$0"; exit 0 ;;
		--) shift; EXTRA+=("$@"); break ;;
		*) EXTRA+=("$1"); shift ;;
	esac
done

# Resolve the pmbootstrap workdir → the aarch64 buildroot chroot.
WORK="$(pmbootstrap config work 2>/dev/null | tail -n1)"
[ -n "$WORK" ] || WORK="$HOME/.local/var/pmbootstrap"
CHROOT="$WORK/chroot_buildroot_aarch64"
MNT="$CHROOT/mnt/pgo-music"
BMNT="$CHROOT/mnt/x86-bolt"
BSTAGE=""   # host staging dir for the BOLT toolchain (set below when --bolt)

_umount() {
	mountpoint -q "$1" 2>/dev/null || return 0
	sudo umount "$1" 2>/dev/null || sudo umount -l "$1" 2>/dev/null || true
	echo "→ unmounted $1"
}
cleanup() {
	_umount "$MNT"
	_umount "$BMNT"
	[ -n "$BSTAGE" ] && rm -rf "$BSTAGE" 2>/dev/null || true
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

if [ "$BOLT" = 1 ]; then
	command -v llvm-bolt >/dev/null && command -v merge-fdata >/dev/null || {
		echo "error: --bolt needs host llvm-bolt + merge-fdata (Arch/CachyOS: sudo pacman -S llvm)" >&2
		exit 1
	}
	# Instrumentation links an aarch64 libbolt_rt_instr.a INTO the target, so the
	# RUNTIME must be aarch64 even though the tool is x86_64. Auto-detect a downloaded
	# aarch64 LLVM (LLVM-*-Linux-ARM64) under the repo, or take --bolt-rt DIR.
	[ -n "$BOLT_RT" ] || BOLT_RT="$(ls -d "$SRC"/LLVM-*-Linux-ARM64/lib 2>/dev/null | head -n1)"
	RT_A=""
	for c in "$BOLT_RT/libbolt_rt_instr.a" "$BOLT_RT/lib/libbolt_rt_instr.a"; do
		[ -f "$c" ] && { RT_A="$c"; break; }
	done
	[ -n "$RT_A" ] || {
		echo "error: --bolt needs an aarch64 libbolt_rt_instr.a (the instrumentation runtime)." >&2
		echo "       Put a downloaded aarch64 LLVM (LLVM-*-Linux-ARM64) in the repo, or pass" >&2
		echo "       --bolt-rt DIR pointing at its lib/ dir." >&2
		exit 1
	}
	# Make sure the buildroot chroot exists before we mount into it (a no-op if the
	# music branch above already created it).
	pmbootstrap -y chroot -b aarch64 -- /bin/true >/dev/null 2>&1 || true
	[ -d "$CHROOT" ] || { echo "error: buildroot chroot not found at $CHROOT" >&2; exit 1; }
	# bin/ : x86_64 llvm-bolt + merge-fdata + their lib closure (statically linked vs
	# LLVM, so the closure is just glibc/libstdc++/libz/zstd, ~55 MB). They run
	# NATIVELY (fast) via the bundled loader. lib/ : the aarch64 runtime, which BOLT
	# resolves relative to the tool as <parent-of-bin>/lib/libbolt_rt_instr.a.
	BSTAGE="$(mktemp -d)"; mkdir -p "$BSTAGE/bin" "$BSTAGE/lib"
	echo "→ staging x86_64 BOLT tools (native) + aarch64 runtime → $BSTAGE"
	for t in llvm-bolt merge-fdata; do cp -L "$(command -v "$t")" "$BSTAGE/bin/"; done
	ldd "$(command -v llvm-bolt)" \
		| awk '{for(i=1;i<=NF;i++) if($i ~ /^\//) print $i}' | sort -u \
		| while read -r lib; do cp -L "$lib" "$BSTAGE/bin/" 2>/dev/null || true; done
	cp -L /lib64/ld-linux-x86-64.so.2 "$BSTAGE/bin/ld-linux-x86-64.so.2"
	cp -L "$RT_A" "$BSTAGE/lib/libbolt_rt_instr.a"
	cp -L "$(dirname "$RT_A")/libbolt_rt_hugify.a" "$BSTAGE/lib/" 2>/dev/null || true
	chmod -R a+rX "$BSTAGE"   # the in-chroot build user must read/exec it
	echo "  runtime: $RT_A"
	sudo mkdir -p "$BMNT"
	_umount "$BMNT"   # drop any stale mount so we always bind THIS fresh staging
	echo "→ bind-mounting (read-only) BOLT toolchain → $BMNT"
	sudo mount --bind "$BSTAGE" "$BMNT"
	sudo mount -o remount,ro,bind "$BMNT"
	echo "  (watch for 'BOLT: player-cli optimized' — that confirms BOLT ran)"
else
	echo "→ --no-bolt: PGO only (pass --bolt to also BOLT player-cli + player-gtk)"
fi

echo "→ pmbootstrap build --src=$SRC $PKG ${EXTRA[*]:-}"
echo "  (watch for 'PGO: real-corpus workload …' — that confirms the library is in use)"
pmbootstrap --details-to-stdout build --src="$SRC" "$PKG" ${EXTRA[@]+"${EXTRA[@]}"}
