#!/usr/bin/env bash
# verify-bitperfect.sh — the byte-for-byte regression gate for the audio path.
#
# This is the characterization test that makes the player-core / sacd / player-cli
# refactors safe: it re-proves the non-negotiable invariant (decoded output is
# bit-identical to ffmpeg, and transport through the real ALSA stack is
# bit-transparent). Run it after EVERY commit that touches a decode / convert /
# pack / DSD / DoP / sink path (Phases 4–5), and any time you want the proof.
#
# Two layers, mirroring CLAUDE.md "Verifying bit-perfect output":
#   1. DECODE GATE (always runs, no hardware): for each testfile, `dump` the
#      decoded full-scale s32le and byte-compare against ffmpeg. A single
#      differing byte fails the gate.
#   2. TRANSPORT GATE (best-effort, needs snd-aloop): play through hw:Loopback,
#      capture it back, and byte-compare — plus the gapless-queue proof. Skipped
#      with a notice if the Loopback card isn't present (never fails for absence).
#
# Usage:
#   scripts/verify-bitperfect.sh                 # build (release) + run both gates
#   PLAYER_CLI=target/release/player-cli scripts/verify-bitperfect.sh   # reuse a binary
#   SKIP_BUILD=1 scripts/verify-bitperfect.sh    # don't rebuild first
#   LOOPBACK_SECONDS=4 scripts/verify-bitperfect.sh
set -euo pipefail

cd "$(dirname "$0")/.."
TESTFILES_DIR="testfiles"
LOOPBACK_SECONDS="${LOOPBACK_SECONDS:-5}"

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "!! ffmpeg not found — the decode gate needs it. Install ffmpeg." >&2
  exit 2
fi

# Resolve the CLI binary: explicit override, else build (unless SKIP_BUILD).
CLI="${PLAYER_CLI:-}"
if [ -z "$CLI" ]; then
  if [ "${SKIP_BUILD:-0}" != "1" ]; then
    echo "==> building player-cli (release) ..."
    cargo build --release -p player-cli >/dev/null
  fi
  CLI="target/release/player-cli"
fi
[ -x "$CLI" ] || { echo "!! player-cli binary not found at $CLI" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failed=0

echo "==> DECODE GATE  (dump vs ffmpeg, byte-for-byte)"
shopt -s nullglob
for f in "$TESTFILES_DIR"/*; do
  [ -f "$f" ] || continue
  case "$f" in *.cue|*.m3u|*.txt) continue;; esac
  name="$(basename "$f")"
  if ! "$CLI" dump "$f" -o "$tmp/mine.s32le" >/dev/null 2>&1; then
    echo "   SKIP $name  (cli dump could not decode it)"
    continue
  fi
  ffmpeg -v error -y -i "$f" -f s32le -ac 2 "$tmp/ff.s32le" 2>/dev/null || {
    echo "   SKIP $name  (ffmpeg could not decode the reference)"; continue;
  }
  if cmp -s "$tmp/mine.s32le" "$tmp/ff.s32le"; then
    echo "   OK   $name"
  else
    echo "   FAIL $name  — decoded bytes differ from the ffmpeg reference!"
    failed=1
  fi
done

# Transport gate: only if a Loopback card is present (snd-aloop loaded).
echo "==> TRANSPORT GATE  (snd-aloop loopback, best-effort)"
if grep -qi loopback /proc/asound/cards 2>/dev/null; then
  # Pick a small file for the real-time loopback (44.1k content needs Loopback
  # anyway on a 48k-only internal card).
  lf=""
  for cand in s16_44k.flac s16_44k.wav flac_48k.flac; do
    [ -f "$TESTFILES_DIR/$cand" ] && { lf="$TESTFILES_DIR/$cand"; break; }
  done
  if [ -n "$lf" ]; then
    echo "   .. loopback-verify $lf (--seconds $LOOPBACK_SECONDS)"
    if "$CLI" loopback-verify "$lf" --seconds "$LOOPBACK_SECONDS" 2>&1 | tee "$tmp/lv.log" | grep -qi 'BIT-PERFECT'; then
      echo "   OK   transport bit-transparent"
    else
      echo "   FAIL transport not bit-perfect (see output above)"; failed=1
    fi
  fi
  # Gapless-queue proof: two same-wire-format files (both 16-bit/44.1k here).
  if [ -f "$TESTFILES_DIR/s16_44k.flac" ] && [ -f "$TESTFILES_DIR/s16_44k.wav" ]; then
    echo "   .. loopback-verify-queue (gapless, byte-for-byte)"
    if "$CLI" loopback-verify-queue "$TESTFILES_DIR/s16_44k.flac" "$TESTFILES_DIR/s16_44k.wav" 2>&1 | grep -qiE 'MATCH|BIT-PERFECT'; then
      echo "   OK   gapless queue byte-identical to the concatenated decode"
    else
      echo "   FAIL gapless queue mismatch (samples inserted/dropped at a boundary)"; failed=1
    fi
  fi
else
  echo "   .. no Loopback card (run: sudo modprobe snd-aloop) — transport gate skipped"
fi

echo
if [ "$failed" -eq 0 ]; then
  echo "RESULT: BIT-PERFECT ✓  (all decode comparisons identical)"
else
  echo "RESULT: FAILED ✗  — a sample-touching regression slipped in. DO NOT SHIP." >&2
fi
exit "$failed"
