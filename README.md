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
player-cli devices                          # list bit-perfect hw: outputs (USB DACs first, * = auto-pick)
player-cli scan ~/Music                     # index a folder (incremental; tags + art via lofty)
player-cli search "miles kind of blue"      # fuzzy typeahead search of the index
player-cli search davis --filter artists --group   # grouped FTS search, narrowed by column
player-cli library-stats                    # tracks / albums / artists / folders counts
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

## Library & search (`crates/player-library`)

A headless index: scans a folder tree, extracts tags + embedded art (lofty) and
header wire-facts, caches them in SQLite/FTS5, refreshes **incrementally**
(mtime+size change detection; `mv` is recognised as a move, not re-imported), and
answers two kinds of search — grouped FTS5 (Albums/Folders/Tracks, accent-folded)
and forgiving nucleo fuzzy typeahead. No audio/GTK deps, so it stays testable
without hardware. Default locations: `~/.local/share/player/library.db`, art under
`~/.cache/player/art`. Settings + the last session live in its `meta` table.

## GUI

```sh
cargo run -p player-gtk --release        # device: persisted choice → $PLAYER_DEVICE → auto-picked USB DAC
```

A libadwaita DAP: **Library** (Albums / Artists / Folders / Tracks with album
drill-down), **Playing** (hero art, the bit-perfect format chip, real
**pause/resume**, a **draggable seek** bar, per-track position), **Search** (FTS +
fuzzy), and **Lists** (the queue + `.m3u` save/load). **Settings** (menu) picks the
output device (bit-perfect `hw:` only; switching re-spawns the engine), the music
folder + live-watch, and the theme — all persisted, along with the last
queue/track/position, and restored on launch. Still **no volume slider** by design
(volume is the DAC's hardware knob).

## Cross-compiling for the Poco F1 (aarch64)

The headless crates cross-compile; **player-gtk is built on-device** (postmarketOS
ships gtk4 + libadwaita). The pure-Rust code is portable — only the bundled C
(SQLite via rusqlite, ALSA via alsa-sys) needs an aarch64 toolchain + `libasound`.

**Turnkey (recommended)** — `cross` + Docker, nothing on the host but Docker:

```sh
cargo install cross
# start Docker first if needed:  sudo systemctl start docker
cross build --release --target aarch64-unknown-linux-gnu -p player-cli
cross build --release --target aarch64-unknown-linux-gnu -p player-library
file target/aarch64-unknown-linux-gnu/release/player-cli   # ELF 64-bit ARM aarch64
```

`Cross.toml` adds `libasound2-dev:arm64` inside the build image; `.cargo/config.toml`
documents the bare-host alternative (an `aarch64-linux-gnu-gcc` + arm64 `libasound`
sysroot with per-invocation `PKG_CONFIG_*`).

**On the device** (postmarketOS): `cargo build --release -p player-gtk` natively, and
for the engine give the audio group `SCHED_FIFO` headroom (`/etc/security/limits.d`:
`@audio - rtprio 95`). Optionally pin the audio thread to a big core with
`PLAYER_AUDIO_CPU=<n>` (e.g. a Kryo gold core on the SD845). Point the engine at the
Mojo 2 — `player-cli devices` shows it as the USB auto-pick (`hw:CARD=Mojo2,DEV=0`).

(On this dev box the internal card only does 48k–192k; use `hw:Loopback,0,0` or a
48k/96k file to hear 44.1k content, or rely on the loopback verifier.)

## Done in Phase 2

Decode/RT-audio thread split + wait-free SPSC ring (`rtrb`); `SCHED_FIFO` audio
thread; true **gapless** within a wire format; **drain + reopen** on a
rate/format change; `play-queue` / `loopback-verify-queue`; a GTK **Add**
(enqueue) button. Verified bit-perfect (incl. gapless and under full CPU load)
via snd-aloop across S16_LE @44.1k and S24_3LE @48k.

## Done in Phase 3

Music **library** (scan/index/incremental-refresh, FTS5 + nucleo search) + CLI
`scan`/`search`/`library-stats`; the full **GTK DAP** (browse + album detail,
search, queue, `.m3u` playlists, Settings, session persistence); engine **transport**
— real `pause`/`resume`, sample-accurate `seek`, and **per-track** position; dynamic
**device discovery** (`devices`, USB-DAC auto-pick); aarch64 cross-build config
(`cross` + `Cross.toml`) with on-device GTK.

## Not yet (Phase 4+)

Gapless album playback **through the UI** + an engine-owned queue (`Next`/`Prev` +
`TrackChanged`); direct MMAP; USB hotplug (`inotify` on `/dev/snd`); lossy codecs +
dither; DSD/DoP; WirePlumber `device.reserved` / udev rules. **MPRIS** is
deliberately skipped (the player phone's lockscreen is disabled).
