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
  - `engine/` — two playback paths sharing the decode/pack building blocks:
    - `run_playback` (`mod.rs`) — the v1 single-thread decode→pack→`writei` loop,
      kept verbatim for `play`/`dump` and as the bit-perfect oracle.
    - the **real-time gapless engine** (Phase 2) — a decode thread and a
      dedicated `SCHED_FIFO` audio thread joined by a wait-free SPSC byte ring
      (`rtrb`). A *segment* is a maximal run of queued tracks that share one wire
      format; tracks within a segment stream through one open device
      (**gapless** — the audio thread never sees the boundary), and a real
      rate/format change **drains and reopens** the device. Exposed as
      `play_queue_blocking` and the interactive `Player` (used by the GTK shell).
  - `rt.rs` — `SCHED_FIFO` helper with graceful fallback, now applied to the
    audio thread. RT isn't required for bit-perfect, but it keeps output
    glitch-free under load (verified: 0 xruns with every core saturated).
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
player-cli play-queue A.flac B.flac         # gapless queue via the real-time engine
player-cli loopback-verify-queue A.flac B.flac # prove a queue plays gapless + bit-perfect
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

**3. Gapless transport** (Phase 2, through the real-time engine):

Split a file into two sample-exact halves, then prove the queue reproduces the
whole — gaplessness reduces to *"the captured queue equals the original whole,
byte-for-byte."*

```sh
ffmpeg -i song.flac -af atrim=end_sample=N   a.flac
ffmpeg -i song.flac -af atrim=start_sample=N b.flac
player-cli loopback-verify-queue a.flac b.flac
# -> MATCH: ... BIT-PERFECT   (zero samples inserted/dropped at the boundary)
```

Verified locally: decode is byte-identical to ffmpeg, and the captured loopback
stream is byte-identical to what we wrote — for single tracks and **gapless
queues**, across S16_LE / S24_3LE / S32_LE and 44.1 / 48 / 96 kHz, including
with 0 xruns while every CPU core was saturated. A rate/format change between
queued tracks drains and reopens the device (`play-queue`).

## GUI

```sh
PLAYER_DEVICE=hw:1,0 cargo run -p player-gtk --release
```

**Open** replaces the queue and plays now; **Add** enqueues a file (gapless if it
shares the current wire format). There is still no volume slider by design.

(On this dev box the internal card only does 48k–192k; use `hw:Loopback,0,0` or a
48k/96k file to hear 44.1k content, or rely on the loopback verifier.)

## Done in Phase 2

Decode/RT-audio thread split + wait-free SPSC ring (`rtrb`); `SCHED_FIFO` audio
thread; true **gapless** within a wire format; **drain + reopen** on a
rate/format change; `play-queue` / `loopback-verify-queue`; a GTK **Add**
(enqueue) button. Verified bit-perfect (incl. gapless and under full CPU load)
via snd-aloop across S16_LE @44.1k and S24_3LE @48k.

## Not yet (Phase 3+)

Direct MMAP; dynamic device discovery (`HintIter`) + USB hotplug (`inotify` on
`/dev/snd`); lossy codecs + dither; DSD/DoP; aarch64 cross-compile and on-device
testing (WirePlumber `device.reserved` / udev rules, MPRIS for the Phosh
lockscreen).
