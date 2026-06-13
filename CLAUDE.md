# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A hi-end, **bit-perfect** music player in Rust. It decodes lossless audio and writes
it directly to an ALSA `hw:` device with **no resampling, no software mixing, no
dither, and no software volume**. Final target: a Poco F1 DAP on postmarketOS/Phosh
feeding a Chord Mojo 2 USB DAC; it also runs as a normal Linux desktop app, which is
the primary dev environment.

A Cargo workspace of five crates (license: **GPL-3.0-or-later** — the `sacd`
crate is ported from the GPLv3 sacd-ripper, so the whole workspace matches):

- **`player-core`** — the engine. No GTK, no DB; fully headless-testable. Decode →
  convert/pack → ALSA `hw:` output, plus the real-time gapless engine.
- **`player-library`** — headless music index (scan, tags+art, SQLite/FTS5, fuzzy
  search). No audio/GTK deps.
- **`player-cli`** — `probe`/`play`/`dump`/`loopback-verify`/`play-queue`/`scan`/
  `search`/`devices`, etc. The CLI is also the **bit-perfect verification harness**.
- **`player-gtk`** — the libadwaita DAP shell (built on `player-core` + `player-library`).
- **`sacd`** — pure-Rust SACD reader: Scarletbook `.iso` parse + full DST decoder
  (arithmetic coder + adaptive FIR), emitting **native DSD**. Headless, no audio
  deps; publishable standalone. Ported from the C++ reference in `sacd/`
  (vendored, git-ignored). Verified bit-correct vs real DSD & DST discs.

## The non-negotiable invariant

**Bit-perfect for lossless: never alter samples.** No SRC, no mixing, no dither, no
software volume. Volume is the DAC's hardware knob — there is deliberately **no volume
slider** and **no MPRIS** (the player phone's lockscreen is disabled). Anything that
*could* alter samples (ReplayGain, software volume, SRC for an unsupported device rate,
dither on a narrowing path) must be an explicit, **off-by-default** opt-in, and the UI
must flag output as no longer bit-perfect. On an unsupported device rate the engine
**hard-errors** (`RateMismatch`) rather than silently resampling. When in doubt, do not
add a sample-touching code path — re-read `FURTHER.md` "Bit-perfect tensions".

The bit-perfect pipeline (`player-core`): `decode.rs` produces interleaved **full-scale
`i32`** (Symphonia `SampleBuffer<i32>`, exact bit-shifts) → `format.rs` picks the ALSA
format from source bit depth, *widened* to the narrowest container the device supports
(device-aware; the Mojo 2 is `S32_LE`-only) → `convert.rs` packs by down-shifting
`(32 - output_bits)`, pure integer math, recovering the native sample exactly →
`sink/alsa.rs` blocking `writei` with exact rate/format negotiation.

**DSD / DoP** (the parallel source path; still bit-perfect — DoP only re-frames DSD,
never alters bits): `dsd.rs` is the `DsdSource` seam + `open_dsd` factory (`.dsf`/`.dff`
via the `dsd-reader` crate; SACD `.iso` via the `sacd` crate). `dop.rs` `DopPacker`
wraps interleaved **MSB-first** DSD bytes into `S32_LE`/`S24_3LE` PCM at `dsd_rate/16`
with alternating `0x05`/`0xFA` markers — a normal `StreamSpec{is_dop:true}`. The engine
seam (`engine/decode_thread.rs`) is a `TrackProducer` enum (`Pcm`=Decoder+Packer,
`Dsd`=DsdSource+DopPacker) that yields already-packed bytes, so the ring/segment/audio
thread are **unchanged**. DoP only (the Mojo 2 won't take native ALSA DSD); `alsa` 0.11
does expose `DSD_U32_*` if ever wanted. Add DSD playback features here, *not* by
touching the threads. Verify with the same loopback harness (DoP is just PCM bytes).

## Output safety: the full-scale-burst hazard (NON-NEGOTIABLE)

A real incident: the Mojo 2 emitted a **full-scale (0 dBFS) burst on resume from
pause**, nearly injuring the user. This is **Bug B** in
`Chord Mojo 2  Два отдельных бага — полный разбор.md` (read it): the Mojo 2 applies
volume **digitally in its FPGA and has no hardware mute relay during digital
transitions**, so for a few ms after any PCM **re-initialization** the analog output
is live while attenuation isn't applied — full-scale input → full-scale output, and
**its volume buttons do not help** (potentially 130+ dB). Because we are bit-perfect
there is **no software volume to clamp it**. Triggers we control: `snd_pcm_drop()`+
`prepare()` (pause/seek/flush), xrun recovery, suspend/wake, rate change, device
reopen. (Bug A — white noise from a USB stream interruption feeding the FPGA garbage —
is separate, plays at the *current* volume, and shares the same "minimize
interruptions" mitigations.)

**The only bit-perfect defence is silence** (the music is never altered). Two rules,
both already implemented — do not regress them, and apply them to any new audio path:

1. **Never tear the stream down to pause.** A device without hardware pause
   (`can_pause()==false`, the Mojo 2) is held open with a **silence keep-alive**
   (`engine/audio_thread.rs::pause_until_resumed` writes idle silence until resumed),
   **never** `drop()`/`prepare()`. Hardware pause only when `can_pause()==true`.
2. **Lead every unavoidable re-init with a silence guard band**, so only zeros (or
   valid-marker DoP silence — markerless zeros would break DoP lock) are clocked out
   during the unmuted window:
   - device (re)open → `DOP_PREROLL_MS`/`PCM_PREROLL_MS` pushed into the new segment
     (`engine/decode_thread.rs::producer_for`);
   - xrun/suspend recovery → `REINIT_GUARD_MS` in `sink/alsa.rs::recover` (gated by
     `enable_reinit_guard()`, which the audio thread sets). Keep these constants in
     step.

**Hard rules for future changes:**
- Adding **any** new `prepare()`/`drop()`/device-open path means adding a silence
  guard in front of it. No exceptions. (The dead `AlsaSink::reset()` and the
  drop-on-pause fallback were **removed** precisely so they can't be reused unguarded.)
- The guard is **engine-only**. `run_playback` (the bit-perfect oracle) and the
  `loopback-verify` playback stay byte-pure for the proof — so **CLI `play`/`dump` are
  unguarded and are *not* the safe daily path for the Mojo 2; the interactive `Player`
  / GTK app is.** Don't "fix" this by adding silence to the oracle.
- Prefer **gapless within a segment** (no reopen) over per-track reopen; minimize rate
  switches (each is a guarded but real transition).
- Silence guards are **leading silence, not a sample ramp** — never apply gain/fade to
  the music itself (that breaks bit-perfect). Verify any change to these paths still
  passes the loopback harness unchanged.
- System backstop (defence-in-depth, not the fix): `snd_usb_audio.power_save=0` +
  `usbcore.autosuspend=-1` (packaging). `implicit_fb`/`autoclock` are **test-only** —
  the Mojo 2 is async with its own feedback endpoint; don't force them by default.

**Does a realtime (PREEMPT_RT) kernel help?** It **cannot** touch the burst hazard or
the DAC's clock: the Mojo 2 is an **asynchronous** USB DAC (it is clock master; the
host adapts via the feedback endpoint), so host scheduling only affects *USB packet
delivery timing*, i.e. how often we underrun. RT can modestly **reduce xruns** (fewer
reinit triggers), so it is **complementary** to — never a substitute for — the silence
guard. We already run `SCHED_FIFO` on an isolated core with RT-prioritised USB IRQs and
bounded C-states/DMA latency (~0 xruns), so a full RT kernel is **marginal** here and
can even *raise* risk if misconfigured (too-small buffers, priority inversion with the
xHCI threads). Net: optional, low priority; keep the silence-guard discipline regardless.

## Build / test / run

```sh
cargo build --release                       # whole workspace
cargo build --release -p player-cli         # one crate
cargo test                                  # all tests (headless; no audio HW needed)
cargo test -p player-library                # one crate
cargo test -p player-library scan_search    # integration test file
cargo test -p player-core convert           # tests in convert.rs by name
cargo clippy --all-targets                  # lint
cargo run -p player-gtk --release           # the DAP GUI
```

Needs dev packages: `gtk4`, `libadwaita`, `alsa-lib` (and a C toolchain for rusqlite's
bundled SQLite). Tests live as unit tests in `player-core` (`convert.rs`, `decode.rs`),
`player-library` (`cue.rs` + `tests/scan_search.rs`) — all run without audio hardware.

## Verifying bit-perfect output (do this for any change touching the audio path)

The proof is byte-for-byte equality, and the CLI is the harness. Two independent checks:

```sh
# 1. Decode correctness vs ffmpeg (CPU only, no device):
cargo run -p player-cli --release -- dump testfiles/s16_44k.flac -o /tmp/mine.s32le
ffmpeg -v error -i testfiles/s16_44k.flac -f s32le /tmp/ff.s32le
cmp /tmp/mine.s32le /tmp/ff.s32le            # must be identical

# 2. Transport bit-transparency through the real ALSA stack (virtual loopback):
sudo modprobe snd-aloop
cargo run -p player-cli --release -- loopback-verify testfiles/s16_44k.flac --seconds 8
# -> "MATCH: ... BIT-PERFECT"

# 3. Gapless transport (split a file, prove the queue == the whole, byte-for-byte):
cargo run -p player-cli --release -- loopback-verify-queue a.flac b.flac
```

`testfiles/` holds small fixtures across S16/S32 and 44.1/48/96 kHz. On this dev box the
internal card only does 48k–192k, so 44.1k content must go through `hw:Loopback,0,0` (or
use a 48k/96k file) to be heard — but the loopback verifier proves correctness regardless.

## Architecture: the engine seam

`player-core::engine` has **two playback paths** sharing the decode/convert/pack blocks:

- `run_playback` (`engine/mod.rs`) — the v1 single-thread `decode → pack → writei` loop.
  Kept verbatim and used by CLI `play`/`dump` and as the **bit-perfect oracle** the
  loopback tests compare against. Do not "improve" it casually; it defines correctness.
- The **real-time gapless engine** — a decode thread + a dedicated `SCHED_FIFO` audio
  thread (`engine/audio_thread.rs`, `engine/decode_thread.rs`) joined by a wait-free SPSC
  byte ring (`engine/ring.rs`, `rtrb`). The ring carries **already-packed bytes**; the
  audio thread writes them unchanged — that's how per-segment output stays byte-identical
  to v1. A *segment* is a maximal run of queued tracks sharing one wire `StreamSpec`:
  tracks within a segment stream through one open device (**gapless** — the audio thread
  never sees the boundary); a real rate/format change **drains and reopens** the device.

The single interface to the engine is `Cmd` (in) / `Event` (out) on the interactive
`Player` (`engine/mod.rs`): `Play`/`PlayRange`/`Enqueue`/`Pause`/`Resume`/`Seek`/`Stop`/
`Quit` → `Started`/`Position`/`Ended`/`Error`. **Extend playback features by adding to
`Cmd`/`Event`, not by reaching into the threads.** `play_queue_blocking` is the
non-interactive variant used by the CLI verifier. `rt.rs` provides the `SCHED_FIFO`
helper with graceful fallback (RT isn't required for bit-perfect, only for glitch-free
output under load).

Device selection: `devices.rs` enumerates bit-perfect `hw:` outputs (USB DACs first) and
auto-picks. The GTK app resolves device as: persisted choice → `$PLAYER_DEVICE` →
auto-picked USB DAC. `PLAYER_AUDIO_CPU=<n>` pins the audio thread to a core.

## Architecture: library

`player-library` is a `Library` (one SQLite connection per thread, for scan + browse +
the `meta` key/value store) plus a separate, `Send` `SearchIndex` (in-RAM nucleo fuzzy
haystacks, typically owned by a worker thread). Scan is **incremental** (mtime+size
change detection; `mv` is recognized as a move, not re-imported) via `scan.rs` (rayon +
`ignore` walk), tags+embedded art via `lofty` (`extract.rs`), `.cue` sheets in `cue.rs`.
Search has two flavors: grouped FTS5 (accent-folded, Albums/Folders/Tracks) and forgiving
nucleo typeahead. Settings + last session persist in the DB `meta` table. Defaults:
`~/.local/share/player/library.db`, art under `~/.cache/player/art`.

## Architecture: GTK shell

A libadwaita DAP with a bottom view switcher over **Library · Playing · Search · Lists**
(no equalizer — it's a no-DSP player). `main.rs` is the assembly point: it builds each
component (the `ui/` modules: `library`, `now_playing`, `search`, `queue`, `settings`,
`mini`), stashes long-lived handles in `state::Ui`, and wires cross-cutting header/
transport actions. All state is single-threaded (GTK main loop): `SharedState =
Rc<RefCell<State>>`, `SharedUi = Rc<Ui>`, passed to every component. Engine `Event`s
cross the thread boundary via `async-channel` + `glib::spawn_future_local` (note:
`glib::MainContext::channel` is gone in current glib-rs). The GTK app currently drives
the queue track-by-track in the UI (advancing on `Ended`); a fully engine-owned queue
with gapless-through-UI is the next deferred step (`FURTHER.md` 3.7 / Phase 4).

## GTK shell: mobile / adaptive layout (Phosh / Poco F1)

**The UI is mobile-first.** Phosh renders the Poco F1's 1080×2246 panel at 3× →
**~360×749 logical px**, and maximises app windows, so every screen must *fit* ~360 px
wide. The fix is always to **budget to that viewport** — never `scale-to-fit`, `GDK_SCALE`,
or per-app phoc hacks (they blur the whole app and mask the real bug). Default window is
phone-portrait `360×720`; it stays resizable on desktop (the primary dev box).

**The trap: `AdwViewStack` sizes to its *widest* page.** It is effectively homogeneous, so
a *single* page whose minimum width exceeds ~360 forces **every** page that wide and the
whole app overflows — you'll see `Adwaita-WARNING: AdwToastOverlay exceeds
AdwApplicationWindow width: requested NNN px, 360 px available` and the content visibly
shifts/clips. So **every page must be able to shrink to ≤360**, and nothing in a page may
pin a large minimum width:
- A fixed **N-up grid of fixed-size cells** pins `N × cell + padding`. The Albums grid is
  3-up with covers sized to the window width (`ui::library::album_cover_px`, clamped/
  quantised) and rebuilt on resize — keep `3 × cover + padding ≤ 360` at the phone floor.
  **Gotcha:** `window.width()` is the *default* (360) during `build_ui` and only becomes
  the real value after the compositor configures the surface — and Phosh often maximises to
  a logical width *wider* than 360 (depends on the device scale). So anything sized from the
  width must recompute *after the surface settles*: `main.rs` runs a deferred
  `timeout_add_local_once` recompute (300 ms / 1200 ms) on top of the `default-width` /
  `maximized` notifies, else covers stay small until the first resize/re-sort. Always budget
  the *minimum* width to ≤360 (so it never overflows a true-360 device) but derive actual
  sizes from the real allocated width.
- A **non-shrinking control row** (e.g. a `.linked` segmented bar of text tabs) pins its
  natural width. Make such chrome **horizontally scrollable**: the Library browse tabs live
  in a `ScrolledWindow` with `PolicyType::External` (hexpand, scrollbar hidden) so the bar
  centres when there's room and swipes when narrow — it never pins a min wider than the
  screen.

**Per-screen patterns already in place — follow them for new screens:**
- **Now Playing is a single centred column** (matching the design's `NowPlayingView`):
  hero art (`ART_HERO` = 208) → titles → toggles → seek → transport → format chip, wrapped
  in `clamp(content)` with `valign: Center` as the stack child (the clamp caps width on a
  wide desktop; centring balances the cluster on a tall phone). **No `ScrolledWindow` and no
  `vexpand` dock-to-bottom spacer** — the device screen is *tall*, so a spacer just left a
  big empty gap with a lonely chip at the bottom (looked broken). The cluster measures
  ~553 px tall, well under the budget; don't add tall extras and re-check the height fits.
- **Pages with a text entry use `AdwToolbarView`** (Search does): entry + filters in
  `add_top_bar` (pinned), the list in `set_content` with `vexpand`. When phoc resizes for
  the on-screen keyboard (squeekboard/Stevia, via `text-input-v3`) only the content shrinks
  — the entry stays put.
- **Do NOT add a height-keyed `AdwBreakpoint` to hide the switcher for the OSK.** It seems
  tidy ("steps the switcher aside for the keyboard") but on Phosh it *flickers the keyboard
  open/closed*: the OSK resizes the window, the `max-height` breakpoint crosses its
  threshold and toggles a bottom bar's `reveal`, that changes content height, which
  re-triggers phoc's `text-input-v3` resize — an oscillation that retracts the OSK. The
  switcher just stays above the OSK (harmless "double chrome"). Keyboard-safety comes from
  the `AdwToolbarView` structure, not from moving chrome while the OSK animates.
- **No label that shows a variable-length spec may be un-ellipsized.** The bit-perfect
  format chip (`widgets::format_chip`) and list-row meta (`row_inner`) ellipsize their spec
  — otherwise the full `signal_spec`/`format_spec` ("24-bit · 96 kHz · FLAC") pins a min
  width wider than the phone and (via the homogeneous `ViewStack`) shifts/clips every page.
- The 3-up Albums grid floor is tuned (`album_cover_px`, cover floor 80) so the page min
  width lands ~326 px — comfortably under 360 even on a slightly narrower/scaled device.
- Touch targets ≥ ~44 px; secondary controls may be smaller (toggles ~36).

**Debugging over-wide layout:** after `window.present()`, a temporary
`glib::timeout_add_local_once` that calls `widget.measure(Orientation::Horizontal, -1)` on
each `ViewStack` page (and suspect sub-widgets) and `eprintln!`s min/nat pinpoints the
offending widget instantly — far faster than eyeballing screenshots. Remove it before
committing.

## Cross-compiling for the Poco F1 (aarch64)

Headless crates cross-compile; **player-gtk is built on-device** (postmarketOS ships
gtk4+libadwaita). Turnkey path uses `cross` + Docker:

```sh
cross build --release --target aarch64-unknown-linux-gnu -p player-cli
cross build --release --target aarch64-unknown-linux-gnu -p player-library
```

`Cross.toml` installs `libasound2-dev:arm64` in the build image; `.cargo/config.toml`
tunes `target-cpu=cortex-a75` (SDM845) for aarch64 only and documents the bare-host
sysroot alternative — note `PKG_CONFIG_*` are intentionally **not** set globally there
(that would break the native x86_64 build); set them per-invocation. `packaging/` holds
the postmarketOS kernel APKBUILD/patches and the `hifi-player` aport (udev/sysctl/RT
limits for the device).

## Planning docs

`README.md` (status + verification recipes) and `FURTHER.md` (the Phase 3/4 roadmap and
the bit-perfect rationale) are the source of truth for *what's done and what's next* —
consult `FURTHER.md` before starting a feature; it specifies the intended design (e.g.
DoP for DSD, hotplug, device reservation) and the suggested order.
