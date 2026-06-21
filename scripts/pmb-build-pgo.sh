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
# layout. The two arches are SPLIT on purpose:
#   bin/ : x86_64 llvm-bolt + merge-fdata (+ glibc closure), run NATIVELY (fast) — they
#          READ the fdata. Preferred from a pinned LLVM-*-Linux-X64 tarball in the repo
#          (or --bolt-tools DIR); falls back to the system llvm-bolt/merge-fdata.
#   lib/ : the *aarch64* libbolt_rt_instr.a — BOLT links it INTO the target, so it must
#          match the TARGET arch — from a downloaded LLVM-*-Linux-ARM64 (or --bolt-rt DIR).
#          It WRITES the fdata.
# BOLT resolves the runtime as <dir-above-bin>/lib/libbolt_rt_instr.a relative to the
# llvm-bolt it runs — which is how the x86_64 tool picks up the aarch64 runtime. (Point
# it at an X64 tarball's own lib/ and you get "linking object with arch x86_64 into
# context with arch aarch64".) Tools and runtime must share the LLVM MAJOR — the legacy
# fdata schema is only stable within a major — so the script aborts on a mismatch.
# The APKBUILD then BOLTs player-cli + player-gtk; only the instrumented binary's
# training run is emulated under qemu.
#
# Usage:
#   scripts/pmb-build-pgo.sh [--music DIR] [--src DIR] [--no-music] \
#       [--bolt [--bolt-tools DIR] [--bolt-rt DIR]] [-- <pmb args>]
#
# Defaults: --music /home/antonix/Музыка, --src <repo root>, BOLT off. --bolt-tools / --bolt-rt
# default to the LLVM-*-Linux-X64 / LLVM-*-Linux-ARM64 tarballs found in the repo. Needs sudo
# for the bind mount(s). Without a library (or --no-music) the build still works — the
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
BOLT_TOOLS=""
EXTRA=()

while [ $# -gt 0 ]; do
	case "$1" in
		--music) MUSIC="$2"; shift 2 ;;
		--src)   SRC="$2";   shift 2 ;;
		--no-music) NO_MUSIC=1; shift ;;
		--bolt) BOLT=1; shift ;;
		--no-bolt) BOLT=0; shift ;;
		--bolt-rt) BOLT_RT="$2"; shift 2 ;;
		--bolt-tools) BOLT_TOOLS="$2"; shift 2 ;;
		-h|--help) sed -n '3,41p' "$0"; exit 0 ;;
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
	# --- x86_64 reader tools (llvm-bolt + merge-fdata) ----------------------------
	# They must be x86_64 to run natively in the chroot (an ARM64 tarball ships only
	# aarch64+glibc tools — unrunnable in the musl chroot and qemu-slow). Prefer a
	# pinned LLVM-*-Linux-X64 tarball in the repo (or --bolt-tools DIR) so the whole
	# toolchain is one fixed LLVM release; fall back to the rolling system tools.
	[ -n "$BOLT_TOOLS" ] || BOLT_TOOLS="$(ls -d "$SRC"/LLVM-*-Linux-X64/bin 2>/dev/null | head -n1)"
	if [ -n "$BOLT_TOOLS" ] && [ -x "$BOLT_TOOLS/llvm-bolt" ] && [ -x "$BOLT_TOOLS/merge-fdata" ]; then
		BOLT_BIN="$BOLT_TOOLS/llvm-bolt"; MERGE_BIN="$BOLT_TOOLS/merge-fdata"
		echo "→ BOLT tools: $BOLT_TOOLS (pinned x86_64 LLVM tarball)"
	else
		command -v llvm-bolt >/dev/null && command -v merge-fdata >/dev/null || {
			echo "error: --bolt needs x86_64 llvm-bolt + merge-fdata. Put a downloaded" >&2
			echo "       LLVM-*-Linux-X64 tarball in the repo (recommended — pins the version)," >&2
			echo "       or install the system toolchain (Arch/CachyOS: sudo pacman -S llvm)." >&2
			exit 1
		}
		BOLT_BIN="$(command -v llvm-bolt)"; MERGE_BIN="$(command -v merge-fdata)"
		echo "→ BOLT tools: system llvm-bolt/merge-fdata (no LLVM-*-Linux-X64 tarball found)"
	fi
	# --- aarch64 instrumentation runtime (libbolt_rt_instr.a) ---------------------
	# BOLT links this INTO the aarch64 target, so it must be aarch64 even though the
	# tool is x86_64. Auto-detect a downloaded LLVM-*-Linux-ARM64, or take --bolt-rt DIR.
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
	# --- version guard ------------------------------------------------------------
	# The runtime WRITES the fdata; the tools READ it. The legacy fdata schema only
	# stays compatible within an LLVM MAJOR, so a major mismatch is exactly what yields
	# merge-fdata "Malformed / corrupted entry type". Abort early when we can tell.
	_tool_ver="$("$BOLT_BIN" --version 2>/dev/null | sed -n 's/.*LLVM version \([0-9][0-9]*\).*/\1/p' | head -n1)"
	_rt_ver="$(printf '%s' "$RT_A" | sed -n 's;.*/LLVM-\([0-9][0-9]*\)\..*;\1;p')"
	if [ -n "$_tool_ver" ] && [ -n "$_rt_ver" ] && [ "$_tool_ver" != "$_rt_ver" ]; then
		echo "error: BOLT tool LLVM major ($_tool_ver) != runtime LLVM major ($_rt_ver)." >&2
		echo "       The fdata schema is only stable within a major; a mismatch gives" >&2
		echo "       merge-fdata 'Malformed / corrupted entry type'. Use matching tarballs." >&2
		exit 1
	fi
	echo "  versions: tools=LLVM ${_tool_ver:-?}  runtime=LLVM ${_rt_ver:-?}"
	# Make sure the buildroot chroot exists before we mount into it (a no-op if the
	# music branch above already created it).
	pmbootstrap -y chroot -b aarch64 -- /bin/true >/dev/null 2>&1 || true
	[ -d "$CHROOT" ] || { echo "error: buildroot chroot not found at $CHROOT" >&2; exit 1; }
	# bin/ : the x86_64 tools + their lib closure (statically linked vs LLVM, so the
	# closure is just host glibc/libstdc++/libz/gcc_s). They run NATIVELY (fast) via the
	# bundled loader. lib/ : the aarch64 runtime, which BOLT resolves relative to the
	# tool as <parent-of-bin>/lib/libbolt_rt_instr.a — so the x86_64 tool links the
	# aarch64 runtime into the aarch64 target (NOT the X64 tarball's own x86_64 one).
	BSTAGE="$(mktemp -d)"; mkdir -p "$BSTAGE/bin" "$BSTAGE/lib"
	echo "→ staging x86_64 BOLT tools (native) + aarch64 runtime → $BSTAGE"
	cp -L "$BOLT_BIN" "$BSTAGE/bin/llvm-bolt"; cp -L "$MERGE_BIN" "$BSTAGE/bin/merge-fdata"
	ldd "$BOLT_BIN" \
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
