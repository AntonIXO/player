# Further Work — Phase 3 & UX

Planning doc for what comes after the verified v1 core and the Phase 2 real-time
gapless engine. Written 2026-05, after the engine was verified bit-perfect on the
real **Chord Mojo 2** (`hw:Mojo2,0`) across 44.1/48/96 kHz, 16- and 24-bit,
gapless, with a 44.1→48 kHz relock, 0 xruns under `SCHED_FIFO`.

## Where we are

- **v1**: bit-perfect decode → pack → blocking `writei` to an ALSA `hw:` device.
- **Phase 2**: decode thread + `SCHED_FIFO` audio thread joined by a wait-free
  `rtrb` byte ring; true **gapless** within a wire format (segment model);
  **drain + reopen** on rate/format change; CLI `play-queue` /
  `loopback-verify-queue`; GTK `Add`.
- **Device-aware format**: probe the device once; widen to the narrowest
  supported container ≥ source width (lossless zero-pad). The Mojo 2 is
  `S32_LE`-only, so 16/24-bit play as full-scale `i32`→`S32_LE`, still bit-perfect.

## Guiding constraint (unchanged, non-negotiable)

Bit-perfect for lossless: **no resampling, no mixing, no dither, no software
volume**, exact device rate. Volume lives in the DAC hardware. Anything that
alters samples (ReplayGain, software volume, SRC for an unsupported rate,
dither on bit-depth reduction) must be an **explicit, off-by-default opt-in**,
and the UI must clearly flag when output is *not* bit-perfect.

---

# Phase 3 — Engine & system

### 3.1 Device discovery & selection
- Enumerate outputs with ALSA `HintIter` (name + description); identify USB DACs
  by card name / USB VID:PID. CLI: `player-cli devices`. Core:
  `fn list_devices() -> Vec<DeviceInfo{ id, name, kind }>`.
- Auto-pick policy: prefer a connected USB DAC (the Mojo) over the internal card;
  remember the last chosen device.

### 3.2 Capability negotiation — extend the format work to rates/channels
- We already probe formats (`DeviceFormats`). Add `test_rate` / `test_channels`
  to build a full `DeviceCaps { formats, rates, channels, max_bits }`.
- On an unsupported **rate**, today we hard-error (`RateMismatch`). Keep that as
  the bit-perfect default, but surface a clear message and (3.6) an optional SRC.
- Mono/multichannel: map channels explicitly (the Mojo is 2ch only).

### 3.3 Exclusive access & PipeWire/WirePlumber coexistence
- We use exclusive `hw:`. On this box the Mojo was free, but make it
  deterministic: implement the `org.freedesktop.ReserveDevice1` D-Bus handshake
  (or ship a WirePlumber drop-in: `api.alsa.disable-reserve` / mark the DAC
  `api.acp.auto-profile=false` / `device.disabled`), and a udev rule tagging the
  DAC so the session manager doesn't grab it. Release the reservation on stop.

### 3.4 Hotplug & robustness
- USB unplug mid-play currently risks an error loop in the audio thread. Detect
  device loss (writei error class / `inotify` on `/dev/snd` or libudev), stop
  cleanly, surface "device removed", and auto-resume when it returns.
- Suspend/resume (`ESTRPIPE`) is already handled via `try_recover`; add a test.

### 3.5 DSD / DoP
- The Mojo 2 does DSD. Support **DoP** (DSD-over-PCM): wrap DSD bitstream in
  S24/S32 PCM frames with the 0x05/0xFA markers, sent bit-perfect as PCM. Needs
  a DSD source path (DSF/DFF demux — not in symphonia; add a small reader) and a
  `DopPacker`. Large feature; gate behind a setting.

### 3.6 Lossy codecs + optional dither
- Add mp3/aac/ogg/opus decode (symphonia features). These aren't bit-perfect by
  nature; that's fine — they're a separate "lossy" path.
- **Dither** only matters when *reducing* bit depth (e.g. 24-bit source to a
  16-bit-only DAC). With device-aware widening we normally go *up*, never down,
  so no dither is needed for our DACs. If a narrowing path is ever required,
  offer TPDF dither as an explicit opt-in (flagged non-bit-perfect).

### 3.7 Pause / seek / precise position
- **Pause**: `snd_pcm_pause` if supported, else drain-and-hold; bit-perfect
  resume. Wire a `Cmd::Pause/Resume`.
- **Seek**: symphonia `seek()` + ring flush + `pcm` reset; recompute position.
- **Precise position**: report true playback position via `snd_pcm_delay`
  (frames-written minus what's still buffered) instead of frames-submitted; give
  the engine a per-track frame base so the UI shows per-track elapsed across a
  gapless queue.

### 3.8 aarch64 cross-compile + on-device (Poco F1 / postmarketOS)
- Cross-build with `cross` or a Docker/Alpine sysroot; `PKG_CONFIG_*` for
  alsa-lib + gtk4 + libadwaita aarch64. Deploy to postmarketOS/Phosh.
- On-device: drive the Mojo over USB-C OTG; run the loopback self-test on-device;
  confirm `SCHED_FIFO` (rtprio limits + `audio` group on pmOS); pin the audio
  thread to a big core (`sched_setaffinity`) on the F1's big.LITTLE.
- Power: inhibit suspend during playback; verify screen-off playback.

### 3.9 Optional, only if measured
- Direct **MMAP** path (lower latency) — not needed; blocking `writei` is robust
  and bit-perfect. CPU affinity / priority tuning per device.

---

# UX — the DAP application

The Phosh target is now a real player, not just a proof. Touch-first GTK4 /
libadwaita, mobile sizing, dark by default.

### U.1 Navigation / information architecture
- `AdwNavigationView` (or split view on wide screens): **Library → Album/Artist →
  Now Playing**, plus a **Queue** sheet and **Settings**. Bottom mini-player bar
  that expands to the full Now-Playing screen (Adwaita pattern).

### U.2 Now Playing — the bit-perfect hero screen
- Album art, title / artist / album, elapsed / total, a seek slider, transport
  (prev / play-pause / next), and the **signal-path badge**:
  `16-bit FLAC → S32_LE → Mojo 2 @ 44.1 kHz · bit-perfect ✓`.
- Show a small **relock** flash when the device reopens for a new rate, and a
  **gapless** marker when the next track shares the wire spec.

### U.3 Library & browsing
- Scan a music folder; index by folder and by tags. Metadata via `lofty` (or
  symphonia metadata) for tags + embedded art. Fast incremental scan, cached DB
  (sqlite). New crate `player-library` (keep `player-core` headless).
- Views: Albums (art grid), Artists, Folders, Tracks; search.

### U.4 Queue & playlists (gapless-aware)
- Add track / "play album" (enqueues the whole album → gapless via the engine);
  reorder, remove, clear; save/load `.m3u`. Show which boundaries are gapless vs
  a relock.

### U.5 Transport
- Pause/resume, prev/next, seek (3.7). Repeat / shuffle (shuffle only between
  tracks — never alters samples).

### U.6 Output device & settings
- Device picker listing outputs with capabilities (max rate, formats,
  reservation status); reconnect handling. Toggles: exclusive mode, optional SRC
  fallback (default **off**, flagged non-bit-perfect), DSD/DoP enable, theme.

### U.7 Volume policy
- **No software volume** (bit-perfect). For the Mojo, volume is on-device. If a
  DAC exposes a *hardware* ALSA mixer control (its own analog volume), optionally
  surface that single control — it's the DAC's domain, still bit-perfect — but
  never insert a software gain stage.

### U.8 Phosh integration
- **MPRIS2** (D-Bus `org.mpris.MediaPlayer2`) for lockscreen controls + hardware
  media keys, with metadata + art. Thin adapter crate `player-mpris` over the
  `Player` events/commands.
- Suspend-inhibit + screen-off playback; resume queue/position on launch.

### U.9 Visual / touch design
- Large touch targets, swipe nav, libadwaita adaptive layouts, light/dark, album
  art as the visual anchor. Keep the bit-perfect badge prominent — it's the
  product's identity.

### U.10 Persistence
- Last queue + position, chosen device, library path, settings (small TOML/JSON
  or the sqlite DB).

---

# Architecture notes
- Keep `player-core` headless and bit-perfect-pure. New crates: `player-library`
  (scan + tags + art + DB), `player-mpris` (D-Bus adapter); the GTK crate grows
  into the DAP shell. The engine's `Cmd`/`Event` interface is the single seam —
  add `Pause/Resume/Seek/Next/Prev` to `Cmd`, and richer `Event`s
  (`TrackChanged`, `Relock{spec}`, `Position{track_frames}`, `Xrun`).

# Suggested order
1. **3.3 + 3.4** (deterministic exclusive access + hotplug) — needed for a
   reliable DAP on the Mojo.
2. **3.7** (pause/seek/precise position) + **U.5** — table-stakes transport.
3. **U.1–U.4** (navigation, Now-Playing, library, gapless queue) — the app.
4. **3.8** (aarch64 + on-device on the Poco F1) — get it onto the real device.
5. **U.8** (MPRIS) — lockscreen/media-key control on Phosh.
6. **3.2/3.6** (rate caps + lossy), then **3.5** (DSD/DoP) as a flagship extra.

# Bit-perfect tensions (call them out in the UI)
ReplayGain, software volume, and SRC for unsupported rates all alter samples.
If ever added, they live in an explicit **"DSP / convenience" mode**, default
off, and the Now-Playing badge must switch from `bit-perfect ✓` to a clear
`processed` state so the user always knows what the DAC is receiving.
