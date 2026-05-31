# Bit-Perfect Player

A hi-end, bit-perfect music player in Rust. It decodes lossless audio and writes
it **directly to an ALSA `hw:` device** with no resampling, no software mixing,
no dither, and no software volume. Target hardware: Poco F1 DAP on
postmarketOS/Phosh feeding a Chord Mojo 2 USB DAC — but it runs as a normal
Linux desktop app first.

## Design

- **`crates/player-core`** — the engine, no GTK dependency, fully headless-testable.
  - `decode.rs` — Symphonia wrapper. Produces interleaved **full-scale `i32`**
    via `SampleBuffer<i32>` (decoder-agnostic; Symphonia's integer conversions
    are exact bit-shifts).
  - `format.rs` — picks the ALSA output format from the source bit depth:
    16-bit → `S16_LE`, ≤24-bit → `S24_3LE`, else `S32_LE`.
  - `convert.rs` — packs full-scale `i32` to the chosen format by down-shifting
    `(32 - output_bits)`, which recovers the original native sample **exactly**.
    Pure integer math, no float, no dither for lossless.
  - `sink/alsa.rs` — blocking `writei` with exact rate/format negotiation. If the
    device won't accept the source rate exactly, it errors (`RateMismatch`)
    instead of letting ALSA silently resample.
  - `engine.rs` — `run_playback` (the shared decode→convert→write loop) and a
    threaded `Player` (Cmd in / Event out) for the GUI.
  - `rt.rs` — `SCHED_FIFO` helper with graceful fallback (RT is not required for
    bit-perfect; it lowers xrun risk once the audio thread is split out).
- **`crates/player-cli`** — `probe`, `play`, `dump`, `loopback-verify`.
- **`crates/player-gtk`** — minimal libadwaita shell (Open / Play / Stop, a
  bit-perfect format indicator, and **no volume slider** by design).

Why not the things NOTES.md suggested? `glib::MainContext::channel` is removed in
current glib-rs (we use `async-channel` + `glib::spawn_future_local`); ALSA direct
MMAP is fragile and unnecessary for bit-perfect (blocking `writei` is correct and
robust) — both are noted as Phase-2 options.

## Build

Needs `gtk4`, `libadwaita`, and `alsa-lib` dev packages.

```sh
cargo build --release
```

## CLI usage

```sh
player-cli probe FILE                       # codec / rate / depth / chosen format
player-cli play  FILE --device hw:1,0       # bit-perfect playback to a card
player-cli dump  FILE -o out.s32le          # decoded full-scale s32le (for diffing)
player-cli loopback-verify FILE --seconds 8 # play+capture through snd-aloop, byte-compare
```

## Verifying bit-perfect output (local, no DAC needed)

Two independent proofs:

**1. Decode correctness** (vs ffmpeg, CPU only):

```sh
player-cli dump test.flac -o /tmp/mine.s32le
ffmpeg -v error -i test.flac -f s32le /tmp/ff.s32le
cmp /tmp/mine.s32le /tmp/ff.s32le   # identical
```

**2. Transport bit-transparency** (through the real ALSA stack):

```sh
sudo modprobe snd-aloop                     # virtual playback<->capture loopback
player-cli loopback-verify test.flac --seconds 8
# -> MATCH: N frames identical ... BIT-PERFECT
```

Verified locally across S16_LE / S24_3LE / S32_LE and 44.1 / 48 / 96 kHz: decode
is byte-identical to ffmpeg, and the captured loopback stream is byte-identical
to what we wrote.

## GUI

```sh
PLAYER_DEVICE=hw:1,0 cargo run -p player-gtk --release
```

(On this dev box the internal card only does 48k–192k; use `hw:Loopback,0,0` or a
48k/96k file to hear 44.1k content, or rely on the loopback verifier.)

## Not yet (Phase 2+)

Split decode/RT-audio threads + SPSC ring + gapless; ALSA reopen on rate change;
dynamic device discovery + USB hotplug; lossy codecs + dither; DSD/DoP; aarch64
cross-compile and on-device testing (WirePlumber `device.reserved` / udev rules,
MPRIS for the Phosh lockscreen).
