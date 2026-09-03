#!/usr/bin/env bash
#
# pgo-bolt-build.sh — Profile-Guided Optimization (+ optional LLVM BOLT) release
# build for the bit-perfect player. Fully automatic profile collection.
#
# WHAT IT DOES (manual rustc pipeline — robust, no cargo-pgo dependency):
#   1. Build instrumented binaries          (-Cprofile-generate)
#   2. Run a representative workload         -> *.profraw written automatically
#   3. Merge profiles                        (llvm-profdata merge)
#   4. Rebuild PGO-optimized                 (-Cprofile-use)
#   5. (optional) BOLT: instrument -> rerun workload -> merge-fdata -> optimize
#   6. BIT-PERFECT GATE: decode each testfile and byte-compare vs ffmpeg.
#      If ANY file differs, abort — the binary is NOT shipped.
#   7. Strip the final binaries.
#
# WHY IT'S SAFE FOR BIT-PERFECT (the whole point of this player):
#   PGO and BOLT only change code LAYOUT — basic-block / function ordering,
#   inlining and branch-weight heuristics, register allocation pressure. They
#   NEVER change the numeric result of integer arithmetic. The audio hot path is
#   pure integer packing, so output is byte-identical. Step 6 proves it every run.
#
# WHERE THE GAINS ARE (recorded prior finding, .jules/bolt.md):
#   Simple PCM/WAV decode is memcpy/SIMD-bound and shows ~0 PGO gain. PGO/BOLT
#   pay off on BRANCH-HEAVY Rust: FLAC decode, DSD->PCM/DST, library scan +
#   tag/art extraction, and nucleo fuzzy search. The workload below leans on
#   exactly those (FLAC files + a forced rescan + many searches), NOT a flat
#   PCM dump. Note also: PGO/BOLT optimize the Rust YOU compile, not the system
#   gtk4/cairo/pango .so's — so the UI's own rendering is mostly untouched; the
#   wins land in player-core/player-library/sacd (which, being faster, also means
#   fewer xruns -> fewer guarded device re-inits; see CLAUDE.md output-safety).
#
# USAGE:
#   scripts/pgo-bolt-build.sh                 # PGO only, both binaries, CLI training
#   scripts/pgo-bolt-build.sh --bolt          # PGO then BOLT (needs llvm-bolt+merge-fdata)
#   scripts/pgo-bolt-build.sh --gtk           # also train player-gtk under headless mutter
#   scripts/pgo-bolt-build.sh --cli-only      # build/optimize only player-cli
#   scripts/pgo-bolt-build.sh --no-verify     # skip the bit-perfect gate (NOT recommended)
#   scripts/pgo-bolt-build.sh --music DIR     # train scan/search/GTK on a real library too
#
set -euo pipefail

# ---- locate repo root (script lives in scripts/) ----------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

# ---- args -------------------------------------------------------------------
ENABLE_BOLT=0; TRAIN_GTK=0; DO_VERIFY=1; DO_STRIP=1; CLI_ONLY=0; MUSIC_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bolt)      ENABLE_BOLT=1 ;;
    --gtk)       TRAIN_GTK=1 ;;
    --cli-only)  CLI_ONLY=1 ;;
    --no-verify) DO_VERIFY=0 ;;
    --no-strip)  DO_STRIP=0 ;;
    --music)     shift; MUSIC_DIR="${1:?--music needs a DIR}" ;;
    -h|--help)   sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

# ---- toolchain discovery ----------------------------------------------------
HOST="$(rustc -vV | sed -n 's/^host: //p')"
PROFILE="release-pgo"                       # inherits release but strip=false (Cargo.toml)
TARGET_DIR="$ROOT/target/$HOST/$PROFILE"
PGO_DIR="$ROOT/target/pgo-profiles"
BOLT_DIR="$ROOT/target/bolt-profiles"

# target-cpu tuning lives in .cargo/config.toml for aarch64, but an explicit
# RUSTFLAGS env REPLACES config rustflags (they don't merge) — so re-add it here.
BASE_RUSTFLAGS=""
case "$HOST" in aarch64*) BASE_RUSTFLAGS="-Ctarget-cpu=cortex-a75" ;; esac

# llvm-profdata ships with the llvm-tools-preview rustup component.
SYSROOT="$(rustc --print sysroot)"
LLVM_PROFDATA="$(ls "$SYSROOT"/lib/rustlib/*/bin/llvm-profdata 2>/dev/null | head -n1 || true)"
if [ -z "$LLVM_PROFDATA" ]; then
  echo ">> installing llvm-tools-preview (for llvm-profdata)…"
  rustup component add llvm-tools-preview
  LLVM_PROFDATA="$(ls "$SYSROOT"/lib/rustlib/*/bin/llvm-profdata 2>/dev/null | head -n1)"
fi

if [ "$DO_VERIFY" -eq 1 ] && ! command -v ffmpeg >/dev/null; then
  echo "!! ffmpeg not found — needed for the bit-perfect gate. Install it or pass --no-verify." >&2
  exit 1
fi

# ---- BOLT toolchain discovery (prefer the NEWEST LLVM available) -----------
# Distros ship both an unversioned `llvm-bolt` and versioned ones
# (/usr/lib/llvm-<N>/bin on Debian/Ubuntu, /usr/lib/llvm<N>/bin on some others).
# BOLT gets meaningfully better with each release (cdsort/cdsplit landed in 17/18,
# and 19+ improved AArch64 support), so pick the highest version we can find
# rather than whatever happens to be first on $PATH.
BOLT=""; MERGE_FDATA=""; BOLT_VER=0
bolt_version_of() {                     # $1 = llvm-bolt path -> major version, or 0
  "$1" --version 2>/dev/null | sed -n 's/.*LLVM version \([0-9]\+\).*/\1/p' | head -n1
}
pick_bolt() {
  local cand ver best_dir=""
  # $ROOT/LLVM-*-Linux-{X64,ARM64} are the pinned upstream release tarballs the
  # aarch64 packaging path (scripts/pmb-build-pgo.sh) already uses; preferring
  # them here keeps the host and device builds on the SAME BOLT, and they're
  # usually newer than the distro LLVM. A tarball for the wrong arch simply
  # fails `--version` (version 0) and is skipped by the comparison below.
  for cand in $(command -v llvm-bolt 2>/dev/null) \
              "$ROOT"/LLVM-*-Linux-X64/bin/llvm-bolt \
              "$ROOT"/LLVM-*-Linux-ARM64/bin/llvm-bolt \
              /usr/lib/llvm-*/bin/llvm-bolt /usr/lib/llvm*/bin/llvm-bolt \
              /usr/local/bin/llvm-bolt /opt/homebrew/opt/llvm*/bin/llvm-bolt; do
    [ -x "$cand" ] || continue
    # merge-fdata must come from the SAME LLVM as llvm-bolt (the .fdata format
    # is not guaranteed stable across majors).
    [ -x "$(dirname "$cand")/merge-fdata" ] || continue
    ver="$(bolt_version_of "$cand")"; ver="${ver:-0}"
    if [ "$ver" -gt "$BOLT_VER" ]; then BOLT_VER="$ver"; best_dir="$(dirname "$cand")"; fi
  done
  [ -n "$best_dir" ] || return 1
  BOLT="$best_dir/llvm-bolt"; MERGE_FDATA="$best_dir/merge-fdata"
}

if [ "$ENABLE_BOLT" -eq 1 ]; then
  pick_bolt || {
    echo "!! no usable llvm-bolt + merge-fdata pair found (BOLT needs an LLVM built with BOLT)." >&2
    echo "   Arch/CachyOS:  sudo pacman -S llvm     (provides llvm-bolt + merge-fdata)" >&2
    echo "   Debian/Ubuntu: sudo apt install llvm-<N>  (gives /usr/lib/llvm-<N>/bin/llvm-bolt)" >&2
    echo "   Or drop --bolt to run the PGO-only pipeline." >&2
    exit 1
  }
  # rustc's own LLVM only has to be the same MAJOR as BOLT's for the emitted
  # binary to be well understood; a newer BOLT reading an older-LLVM binary is
  # the supported direction, so we only warn if BOLT is the OLDER of the two.
  RUSTC_LLVM="$(rustc -vV | sed -n 's/^LLVM version: \([0-9]\+\).*/\1/p')"
  if [ -n "$RUSTC_LLVM" ] && [ "$BOLT_VER" -lt "$RUSTC_LLVM" ]; then
    echo "   !! BOLT is LLVM $BOLT_VER but rustc emits LLVM $RUSTC_LLVM code — consider upgrading LLVM."
  fi
fi

# crates to build/optimize
if [ "$CLI_ONLY" -eq 1 ]; then PKGS=(player-cli); else PKGS=(player-gtk player-cli); fi
CARGO_P=(); for p in "${PKGS[@]}"; do CARGO_P+=(-p "$p"); done
# Only instrument the binaries we actually train (running an instrumented binary
# is what yields a profile). The optimized rebuild still covers every requested
# crate — player-gtk gets PGO on the shared decode/library hot code via function
# matching against the player-cli profile even when only the CLI is trained.
INSTR_P=(-p player-cli)
[ "$CLI_ONLY" -eq 0 ] && [ "$TRAIN_GTK" -eq 1 ] && INSTR_P+=(-p player-gtk)

echo "== PGO/BOLT build =="
echo "   host        : $HOST"
echo "   profile     : $PROFILE  (strip disabled; final strip done by this script)"
echo "   crates      : ${PKGS[*]}"
echo "   bolt        : $([ $ENABLE_BOLT -eq 1 ] && echo "on (llvm-bolt $BOLT_VER: $BOLT)" || echo off)"
echo "   train gtk   : $([ $TRAIN_GTK -eq 1 ] && echo on || echo off)"
echo "   music dir   : ${MUSIC_DIR:-<testfiles only>}"

# ---- training workloads -----------------------------------------------------
# Deterministic, headless. Emphasises branch-heavy paths (FLAC decode, forced
# rescan + tag/art extraction, fuzzy search) per the .jules/bolt.md finding.
run_cli_workload() {           # $1 = path to a player-cli binary
  local cli="$1" db; db="$(mktemp -u)"
  echo "   .. CLI workload ($cli)"
  # FLAC + WAV decode (the convert/pack hot path); loop to accumulate counts.
  for _ in 1 2 3 4 5; do
    for f in s16_44k.flac flac_48k.flac flac_96k.flac s16_44k.wav s32_44k.wav; do
      [ -f "testfiles/$f" ] && "$cli" dump "testfiles/$f" -o /dev/null >/dev/null 2>&1 || true
    done
  done
  for f in testfiles/*; do "$cli" probe "$f" >/dev/null 2>&1 || true; done
  # library scan (tag + art extraction) + fuzzy search — very branch heavy.
  "$cli" scan testfiles --db "$db" --force >/dev/null 2>&1 || true
  [ -n "$MUSIC_DIR" ] && "$cli" scan "$MUSIC_DIR" --db "$db" >/dev/null 2>&1 || true
  for q in flac wav test album the a love live remix 2024 piano jazz; do
    for filt in all tracks albums artists; do
      "$cli" search "$q" --db "$db" --filter "$filt" >/dev/null 2>&1 || true
    done
  done
  "$cli" library-stats --db "$db" >/dev/null 2>&1 || true
  rm -f "$db"
}

run_gtk_workload() {           # $1 = path to a player-gtk binary (best-effort)
  local gtk="$1"
  command -v mutter >/dev/null || { echo "   .. (no mutter — skipping GTK training)"; return 0; }
  echo "   .. GTK workload under headless mutter ($gtk)"
  ( set +e
    mutter --headless --no-x11 --virtual-monitor 1080x720 >/dev/null 2>&1 &
    local mpid=$!; sleep 2
    export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
    [ -n "$MUSIC_DIR" ] && export PLAYER_MUSIC_DIR="$MUSIC_DIR"
    GDK_BACKEND=wayland "$gtk" >/dev/null 2>&1 &
    local gpid=$!; sleep 8
    kill "$gpid" 2>/dev/null; wait "$gpid" 2>/dev/null
    kill "$mpid" 2>/dev/null; wait "$mpid" 2>/dev/null
  ) || true
}

run_full_workload() {          # $1 = dir containing freshly built binaries
  local dir="$1"
  run_cli_workload "$dir/player-cli"
  if [ "$CLI_ONLY" -eq 0 ] && [ "$TRAIN_GTK" -eq 1 ]; then
    run_gtk_workload "$dir/player-gtk"
  fi
}

# ---- bit-perfect gate -------------------------------------------------------
verify_bitperfect() {          # $1 = player-cli binary
  [ "$DO_VERIFY" -eq 1 ] || { echo "   .. (--no-verify) skipping bit-perfect gate"; return 0; }
  local cli="$1" tmp; tmp="$(mktemp -d)"; local failed=0
  echo "   .. bit-perfect gate ($cli)"
  for f in s16_44k.flac flac_48k.flac flac_96k.flac s16_44k.wav s32_44k.wav; do
    [ -f "testfiles/$f" ] || continue
    "$cli" dump "testfiles/$f" -o "$tmp/mine.s32le"
    ffmpeg -v error -y -i "testfiles/$f" -f s32le -ac 2 "$tmp/ff.s32le"
    if cmp -s "$tmp/mine.s32le" "$tmp/ff.s32le"; then
      echo "      OK   $f"
    else
      echo "      FAIL $f  — decode differs from ffmpeg reference!"; failed=1
    fi
  done
  rm -rf "$tmp"
  if [ "$failed" -ne 0 ]; then
    echo "!! BIT-PERFECT GATE FAILED — aborting, this binary must NOT ship." >&2
    exit 1
  fi
}

# ============================================================================
# 1) instrument
# ============================================================================
echo; echo ">> [1/4] building PGO-instrumented binaries"
rm -rf "$PGO_DIR"; mkdir -p "$PGO_DIR"
RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-generate=$PGO_DIR" \
  cargo build --profile "$PROFILE" --target "$HOST" "${INSTR_P[@]}"

# ============================================================================
# 2) train
# ============================================================================
echo; echo ">> [2/4] collecting profiles"
run_full_workload "$TARGET_DIR"
PROFRAW_N=$(find "$PGO_DIR" -name '*.profraw' | wc -l | tr -d ' ')
[ "$PROFRAW_N" -gt 0 ] || { echo "!! no .profraw produced — workload failed?" >&2; exit 1; }
echo "   .. $PROFRAW_N profraw file(s)"
"$LLVM_PROFDATA" merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"

# ============================================================================
# 3) optimize (PGO)
# ============================================================================
echo; echo ">> [3/4] building PGO-optimized binaries"
PGO_USE="-Cprofile-use=$PGO_DIR/merged.profdata"
# BOLT needs relocations preserved in its input binary (-Wl,-q / --emit-relocs).
[ "$ENABLE_BOLT" -eq 1 ] && PGO_USE="$PGO_USE -Clink-args=-Wl,-q"
RUSTFLAGS="$BASE_RUSTFLAGS $PGO_USE" \
  cargo build --profile "$PROFILE" --target "$HOST" "${CARGO_P[@]}"
verify_bitperfect "$TARGET_DIR/player-cli"

# ============================================================================
# 4) BOLT (optional)
# ============================================================================
if [ "$ENABLE_BOLT" -eq 1 ]; then
  echo; echo ">> [4/4] BOLT optimize"
  rm -rf "$BOLT_DIR"; mkdir -p "$BOLT_DIR"
  # panic=abort means minimal unwind tables, which keeps BOLT happy.
  # Pass set tuned for a modern LLVM (>= 18) and degraded gracefully on older ones:
  #   ext-tsp   — the extended TSP block layout (BOLT's best block orderer).
  #   cdsort    — cache-directed function sort; supersedes hfsort/hfsort+ and is
  #               what upstream recommends since LLVM 17. Falls back to hfsort+.
  #   cdsplit   — 3-way hot/warm/cold function splitting (LLVM 18+), strictly
  #               better than the old -split-all-cold 2-way split. Falls back to
  #               -split-all-cold.
  #   icf=safe  — identical code folding, but only where BOLT can prove function
  #               pointers aren't compared; plain `icf=all` can merge two distinct
  #               functions into one address, which Rust vtable/fn-pointer identity
  #               can observe. Safety over the last fraction of a percent.
  # (No --plt: our binaries aren't linked with -znow. No --hugify: it needs a
  # runtime lib and buys nothing for a short-lived CLI / a phone.)
  BOLT_OPTS=(-reorder-blocks=ext-tsp -dyno-stats)
  BOLT_HELP="$("$BOLT" --help-list 2>&1 || true)"
  bolt_supports() { printf '%s' "$BOLT_HELP" | grep -q -- "$1"; }
  if bolt_supports "=cdsort"; then BOLT_OPTS+=(-reorder-functions=cdsort)
  elif bolt_supports "=hfsort+"; then BOLT_OPTS+=(-reorder-functions=hfsort+)
  else BOLT_OPTS+=(-reorder-functions=hfsort); fi
  if bolt_supports "=cdsplit"; then BOLT_OPTS+=(-split-functions --split-strategy=cdsplit)
  else BOLT_OPTS+=(-split-functions -split-all-cold); fi
  if bolt_supports "=safe"; then BOLT_OPTS+=(-icf=safe); else BOLT_OPTS+=(-icf=1); fi
  echo "   .. llvm-bolt $BOLT_VER: ${BOLT_OPTS[*]}"
  for bin in "${PKGS[@]}"; do
    BIN="$TARGET_DIR/$bin"
    [ -f "$BIN" ] || continue
    echo "   -- BOLT $bin: instrument"
    "$BOLT" "$BIN" -instrument \
      --instrumentation-file="$BOLT_DIR/$bin.fdata" \
      --instrumentation-file-append-pid \
      -o "$BIN.bolt.inst"
    echo "   -- BOLT $bin: train"
    case "$bin" in
      player-cli) run_cli_workload "$BIN.bolt.inst" ;;
      player-gtk) [ "$TRAIN_GTK" -eq 1 ] && run_gtk_workload "$BIN.bolt.inst" || \
                  echo "   .. (no --gtk: BOLT player-gtk trained on startup only)" ;;
    esac
    shopt -s nullglob; FDATA=("$BOLT_DIR/$bin.fdata"*); shopt -u nullglob
    if [ "${#FDATA[@]}" -eq 0 ]; then
      echo "   !! no fdata for $bin — skipping its BOLT optimize"; rm -f "$BIN.bolt.inst"; continue
    fi
    "$MERGE_FDATA" "${FDATA[@]}" > "$BOLT_DIR/$bin.merged.fdata"
    echo "   -- BOLT $bin: optimize"
    "$BOLT" "$BIN" -o "$BIN.bolted" -data="$BOLT_DIR/$bin.merged.fdata" "${BOLT_OPTS[@]}"
    mv "$BIN.bolted" "$BIN"; rm -f "$BIN.bolt.inst"
  done
  verify_bitperfect "$TARGET_DIR/player-cli"
fi

# ============================================================================
# strip + report
# ============================================================================
if [ "$DO_STRIP" -eq 1 ]; then
  for bin in "${PKGS[@]}"; do [ -f "$TARGET_DIR/$bin" ] && strip "$TARGET_DIR/$bin" || true; done
fi

echo; echo "== done =="
for bin in "${PKGS[@]}"; do
  [ -f "$TARGET_DIR/$bin" ] && printf "   %s\n" "$TARGET_DIR/$bin"
done
echo "   (PGO$([ $ENABLE_BOLT -eq 1 ] && echo '+BOLT') optimized, bit-perfect-verified)"
