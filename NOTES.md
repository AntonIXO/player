# Bit-Perfect Direct ALSA Output in Rust + GTK4 for postmarketOS/Phosh

## Executive Overview

This document is a developer guide for implementing a bit-perfect, PulseAudio/PipeWire-bypassing audio player in Rust + GTK4/libadwaita targeting Poco F1 (beryllium) running postmarketOS with the Phosh shell. The goal is direct `hw:` device access to the Chord Mojo 2 USB DAC — no sample rate conversion, no format conversion, no software mixing layer — with the audio output thread running at real-time scheduling priority. The UI (GTK4/libadwaita) and the audio engine live in the same process but on separate threads with carefully controlled IPC to prevent GTK's GLib event loop from injecting latency into the audio path.

***

## Part 1: The Audio Stack Conflict Problem

### Why PulseAudio and PipeWire Block You

postmarketOS on Phosh ships with either PulseAudio or PipeWire (with `pipewire-pulse` emulation). Both servers claim exclusive access to ALSA `hw:` devices on detection via the card manager (WirePlumber for PipeWire, module-udev-detect for PulseAudio). When either server is running and it has acquired your USB DAC, any attempt by your application to open `"hw:1,0"` directly will fail with `EBUSY`.[^1][^2][^3][^4]

There are three strategies, ranked by audio quality:

| Strategy | Approach | SQ Impact | Complexity |
|---|---|---|---|
| **A. Exclusive hw: access** | Kill the sound server when player runs, or configure it to release the device | ✅ True bit-perfect, zero overhead | High — must coexist with phone calls |
| **B. ALSA `plug:hw:` + no SRC** | Open through ALSA plugin but fix rate/format so no conversion occurs | ✅ Bit-perfect if configured correctly | Medium |
| **C. PipeWire passthrough** | Configure WirePlumber for passthrough node, disable DSP | ⚠️ Depends on WirePlumber version | Medium |

**Recommendation: Strategy A with graceful fallback to B.** Detect at startup whether the Mojo 2 is claimed by a sound server, and if so, emit a D-Bus signal to suspend PipeWire for the DAC and re-acquire after playback stops. For phone calls (Phosh uses ModemManager + callaudiod), the call stack routes through the built-in codec, not the USB DAC — so suspending the sound server for USB is safe.

### Disabling Sound Server Acquisition of the Mojo 2

**For PipeWire + WirePlumber** (recommended path for postmarketOS edge):

Create `/etc/wireplumber/wireplumber.conf.d/50-mojo2-passthrough.conf`:
```lua
monitor.alsa.rules = [
  {
    matches = [{ device.name = "~alsa_card.*Chord*" }]
    actions = {
      update-props = {
        api.alsa.use-acp = false
        session.suspend-timeout-seconds = 0
        api.alsa.disable-suspend = true
        device.reserved = true
      }
    }
  }
]
```

The `device.reserved = true` key prevents WirePlumber from creating a PipeWire node for that card, leaving the `hw:` device free for your application.[^5]

**For PulseAudio** (older postmarketOS stable):

Add to `/etc/pulse/default.pa`:
```
load-module module-alsa-card device_id="<Mojo2_card_number>" \
    ignore_dB=1 deferred_volume=0 avoid_resampling=yes
```

Or more aggressively, add to `/etc/pulse/default.pa`:
```
# Prevent PA from touching USB audio cards with UAC2 class
load-module module-udev-detect tsched=0 use_ucm=0 \
    ignore_dB=1 avoid_resampling=yes
```

The most reliable approach is a udev rule that sets `SOUND_CLASS=none` for the Mojo 2, preventing PulseAudio's module-udev-detect from ever claiming it:
```
# /etc/udev/rules.d/91-mojo2-nopulse.rules
# Replace 2B0E:0002 with actual Chord Mojo 2 VID:PID from lsusb
ACTION=="add", SUBSYSTEM=="sound", \
    ATTRS{idVendor}=="2B0E", ATTRS{idProduct}=="0002", \
    ENV{SOUND_CLASS}="none"
```

***

## Part 2: Rust Crate Architecture

### The Correct Crate for Bit-Perfect ALSA

Do **not** use `cpal` for bit-perfect work. `cpal`'s ALSA backend has historically set default buffer sizes to 100ms, doesn't expose `Access::MmapInterleaved`, and abstracts away the hw parameter negotiation that you need fine-grained control over.[^6][^7]

Use **`alsa`** (crates.io: `alsa = "0.9"`) directly. It is a thin, safe wrapper around `alsa-lib` that maps nearly 1:1 to the C API while adding Rust ownership semantics. Critically, it exposes `alsa::direct::pcm` — a zero-syscall, zero-allocation MMAP interface that writes directly into the kernel DMA ring buffer.[^8][^9][^10][^11]

```toml
[dependencies]
alsa = "0.9"
symphonia = { version = "0.5", features = ["aac", "flac", "mp3", "ogg", "wav", "alac", "isomp4"] }
gtk4 = "0.9"
libadwaita = { version = "0.7", features = ["v1_6"] }
tokio = { version = "1", features = ["rt-multi-thread", "sync", "fs"] }
crossbeam-channel = "0.5"  # lock-free SPSC for UI→audio commands
```

**Why Symphonia?** It is a pure-Rust audio decoder supporting FLAC, MP3, AAC, ALAC, Vorbis, WAV, OGG, and more. It decodes to `AudioBufferRef` — a format-agnostic sample buffer — which you can then convert to the exact sample type the Mojo 2 accepts (`S24_3LE` or `S32_LE`) without any intermediate floating-point conversion path. Symphonia's FLAC decoder is rated "Perfect" quality.[^12][^13]

### Thread Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Main Thread (GLib/GTK4 event loop)                          │
│   - AdwApplicationWindow, AdwNavigationView                 │
│   - Library browser, Now Playing, Queue                     │
│   - Sends PlayerCmd via crossbeam SPSC channel              │
└────────────────────────┬────────────────────────────────────┘
                         │ crossbeam_channel::bounded(32)
                         │ PlayerCmd { Play, Pause, Stop,
                         │             Next, Seek(Frames) }
                         ▼
┌─────────────────────────────────────────────────────────────┐
│ Decode Thread (std::thread, SCHED_OTHER, normal prio)       │
│   - Symphonia format probe + decode loop                    │
│   - Converts samples to target format (S24_3LE / S32_LE)   │
│   - Fills pre-allocated ring buffer (2 × period_frames)     │
│   - Sends PlayerEvent { Position, TrackEnd } back to GTK    │
└────────────────────────┬────────────────────────────────────┘
                         │ ringbuf::HeapRb (lock-free SPSC)
                         ▼
┌─────────────────────────────────────────────────────────────┐
│ Audio Thread (std::thread, SCHED_FIFO prio 80)              │
│   - ALSA direct MMAP write loop (alsa::direct::pcm)         │
│   - Opens "hw:X,0" exclusively                              │
│   - Never allocates, never blocks on GTK                    │
│   - Period-aligned writes only                              │
└─────────────────────────────────────────────────────────────┘
```

**The key invariant**: the audio thread must **never** touch GTK, never lock a mutex held by the GTK thread, and never call any function that may sleep or allocate. All GTK updates (position display, track progress) are pushed via `glib::MainContext::default().spawn_local()` from the decode thread — not the audio thread.

***

## Part 3: ALSA hw: Device Opening — The Exact API Sequence

This is the most critical section. Every step matters for bit-perfect operation.

### 3.1 Device Discovery

```rust
use alsa::device_name::HintIter;
use alsa::Direction;

fn find_mojo2_device() -> Option<String> {
    // Iterate ALSA PCM playback hints to find Chord Mojo 2
    for hint in HintIter::new_str(None, "pcm").unwrap() {
        if let Some(name) = hint.name {
            if let Some(desc) = hint.desc {
                if desc.contains("Chord") || desc.contains("Mojo") {
                    // Returns "hw:1,0" style string
                    return Some(name);
                }
            }
        }
    }
    None
}
```

Always discover dynamically — never hardcode `hw:1,0`. The card index can change between boots depending on USB enumeration order.

### 3.2 Exclusive hw: Open + Hardware Parameter Negotiation

```rust
use alsa::pcm::{PCM, HwParams, Format, Access, State};
use alsa::{Direction, ValueOr};

pub struct AlsaDevice {
    pcm: PCM,
    period_frames: usize,
    buffer_frames: usize,
    rate: u32,
    format: Format,
}

impl AlsaDevice {
    pub fn open(device: &str, rate: u32, format: Format, channels: u32) -> Result<Self> {
        // nonblock=false: blocking open — will fail EBUSY if sound server holds it
        let pcm = PCM::new(device, Direction::Playback, false)?;

        let hwp = HwParams::any(&pcm)?;

        // ACCESS: MmapInterleaved for direct kernel DMA writes (zero-copy path)
        // Fall back to RWInterleaved only if MMAP is not supported by the hw driver
        if hwp.set_access(&pcm, Access::MmapInterleaved).is_err() {
            hwp.set_access(&pcm, Access::RWInterleaved)?;
        }

        // FORMAT: never use ValueOr::Nearest here — fail hard if format is not supported
        // Mojo 2 supports: S16_LE, S24_3LE (packed 24-bit), S32_LE
        hwp.set_format(&pcm, format)?;

        // RATE: exact match, no resampling. ENODEV if not supported.
        hwp.set_rate(&pcm, rate, ValueOr::Nearest)?;
        // Verify rate was set exactly
        let actual_rate = hwp.get_rate()?;
        if actual_rate != rate {
            return Err(/* rate not supported exactly */);
        }

        hwp.set_channels(&pcm, channels)?;

        // PERIOD SIZE: 1024 frames minimum for USB (UAC2 = 1ms packets)
        // For 44100 Hz: 1024 frames ≈ 23.2ms (2 USB packets per period at 512-frame microframes)
        // For 96000 Hz: 2048 frames ≈ 21.3ms
        // Do NOT go below 512 frames — USB audio at 44.1kHz sends ~44 frames per 1ms packet
        let period_frames: u64 = 1024;
        hwp.set_period_size(&pcm, period_frames as alsa::pcm::Frames, ValueOr::Nearest)?;

        // BUFFER SIZE: 4 × period = 4096 frames. More = more latency but fewer xruns.
        // With the kernel tuning from the previous analysis, 4× is safe.
        hwp.set_buffer_size(&pcm, (period_frames * 4) as alsa::pcm::Frames)?;

        pcm.hw_params(&hwp)?;

        // SOFTWARE PARAMETERS
        let swp = pcm.sw_params_current()?;
        // Start automatically when the buffer is half full (2 periods)
        swp.set_start_threshold(pcm, (period_frames * 2) as alsa::pcm::Frames)?;
        // Wakeup application every period
        swp.set_avail_min(pcm, period_frames as alsa::pcm::Frames)?;
        pcm.sw_params(&swp)?;

        let actual_period = pcm.hw_params_current()?.get_period_size()?;
        let actual_buffer = pcm.hw_params_current()?.get_buffer_size()?;

        Ok(AlsaDevice {
            pcm,
            period_frames: actual_period as usize,
            buffer_frames: actual_buffer as usize,
            rate: actual_rate,
            format,
        })
    }
}
```

### 3.3 Direct MMAP Write Loop (Zero-Copy Path)

`alsa::direct::pcm` bypasses alsa-lib entirely after device setup — no function call overhead, no allocation, just a memory write into the kernel DMA buffer:[^10][^8]

```rust
use alsa::direct::pcm::{MmapPlayback, SyncPtrStatus};

fn audio_loop(pcm: &alsa::pcm::PCM, ring: &mut ringbuf::HeapConsumer<i32>) {
    // Steal the fd from alsa-lib and enter direct MMAP mode
    let mut mmap: MmapPlayback<i32> = pcm.direct_mmap_playback::<i32>().unwrap();

    loop {
        // Check available frames in kernel DMA ring buffer
        // SyncPtrStatus::sync_ptr is a single syscall — not snd_pcm_avail() which is heavier
        if mmap.avail() == 0 {
            // Wait for hardware pointer to advance (period interrupt)
            pcm.wait(Some(200)).unwrap();
            unsafe { mmap.update_avail() };
            continue;
        }

        // Write directly into DMA-mapped memory — zero copy from ring buffer
        let written = mmap.write(|buf| {
            // buf.raw_samples() returns a *mut T into DMA memory
            let n = ring.len().min(buf.len());
            for (dst, src) in buf[..n].iter_mut().zip(ring.pop_iter().take(n)) {
                *dst = src;
            }
            n
        });

        if written == 0 {
            // Underrun: decode thread fell behind
            // DO NOT PANIC — recover gracefully
            match pcm.state() {
                State::Xrun => { pcm.prepare().unwrap(); }
                _ => {}
            }
        }
    }
}
```

**Critical**: `MmapPlayback::write` never calls into alsa-lib at steady state — it is a pure memory copy. This is the zero-overhead path.[^10]

***

## Part 4: Sample Format Negotiation with the Chord Mojo 2

The Mojo 2 is UAC2 compliant and presents its supported formats via USB audio descriptors. Query them via ALSA before assuming:

```bash
# On target device, after loading snd-usb-audio:
aplay -D hw:CARD=Mojo2 --dump-hw-params /dev/zero
# Look for: Formats: S16_LE S24_3LE S32_LE
# And:      Rates: 44100 48000 88200 96000 176400 192000 352800 384000
```

The Mojo 2 internally operates at 705.6 kHz (FPGA oversampled) — it accepts PCM at any standard rate and uses its own FPGA-based SRC internally. **For bit-perfect transmission**, send the file's native sample rate and format. Never let the host resample. The correct ALSA format mapping is:[^14]

| File Format | Symphonia Output | ALSA Format | hw: Device String |
|---|---|---|---|
| FLAC 16-bit | `i16` samples | `Format::S16LE` | `hw:CARD=Mojo2,DEV=0` |
| FLAC 24-bit | `i32` packed in 24 | `Format::S24_3LE` | `hw:CARD=Mojo2,DEV=0` |
| FLAC 32-bit | `i32` | `Format::S32LE` | `hw:CARD=Mojo2,DEV=0` |
| MP3 | decoded `i16` | `Format::S16LE` | `hw:CARD=Mojo2,DEV=0` |
| DSD64 (DoP) | N/A — see below | — | — |

**DSD/DoP**: The Mojo 2 supports DoP (DSD over PCM) in native UAC2 mode. DSD64 is transmitted as 24-bit PCM frames at 176.4 kHz with DSD markers (`0x05` and `0xFA` in the high byte). This requires explicit format conversion — Symphonia does not currently support DSD natively. Treat this as a Phase 2 feature.

### Format Change on Track Boundary

When the sample rate or bit depth changes between tracks (e.g., 44.1 kHz FLAC → 96 kHz FLAC), you **must** close and reopen the ALSA PCM device. There is no "seamless rate switch" in the `snd-usb-audio` driver for UAC2 devices. The sequence:

```rust
// On track change with format/rate mismatch:
// 1. Drain the current stream (flush remaining PCM data)
pcm.drain()?;
// 2. Close the ALSA device (drop the AlsaDevice struct)
drop(alsa_device);
// 3. Brief sleep for USB device negotiation (10ms is enough)
std::thread::sleep(Duration::from_millis(10));
// 4. Reopen with new parameters
let new_device = AlsaDevice::open("hw:CARD=Mojo2,DEV=0", new_rate, new_format, 2)?;
```

***

## Part 5: Real-Time Thread Priority

### The Two Permission Systems

Linux uses two systems for granting RT scheduling permissions:[^15]
1. **RTKit** (D-Bus service) — default for desktop systems, used by PipeWire. Grants RT priority up to 80 to any app that asks. Suitable for consumer audio.
2. **RLIMIT** via `/etc/security/limits.conf` — traditional method. Requires user to be in `@audio` group. Grants RT priority up to 95.[^16]

For pro-audio use, RLIMIT is more reliable because it doesn't go through D-Bus.[^15]

### Setup on postmarketOS (Alpine-based)

```bash
# 1. Add user to audio group
sudo addgroup $USER audio

# 2. Configure RLIMIT
# /etc/security/limits.d/99-audio.conf
@audio - rtprio 95
@audio - memlock unlimited
```

Logout and back in. Verify:
```bash
ulimit -r  # Should print 95
```

**Note on `CONFIG_RT_GROUP_SCHED`**: if the postmarketOS kernel is built with `CONFIG_RT_GROUP_SCHED=y`, you additionally need to configure the cgroup. Check with:[^17]
```bash
zcat /proc/config.gz | grep RT_GROUP_SCHED
```
If enabled, add the process to the `system.slice` cgroup or create an explicit RT cgroup.

### Setting RT Priority from Rust

Use `libc` crate — no need for `nix` or external crate:

```rust
use libc::{sched_param, sched_setscheduler, SCHED_FIFO};

pub fn set_realtime_fifo(priority: i32) -> Result<(), std::io::Error> {
    let param = sched_param { sched_priority: priority };
    let ret = unsafe {
        sched_setscheduler(0, SCHED_FIFO, &param as *const sched_param)
    };
    if ret != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// In the audio thread spawn:
std::thread::Builder::new()
    .name("audio-rt".into())
    .spawn(move || {
        // Set RT priority as the very FIRST thing
        set_realtime_fifo(80).expect("failed to set RT priority — check @audio group");
        // Pin to the isolated audio core (CPU 7 on Snapdragon 845)
        set_cpu_affinity(7);
        // Never call GTK, never lock UI mutexes after this point
        audio_loop(pcm, ring_consumer);
    })?;
```

**Priority hierarchy** (adapt from ArchQ analysis):
- Kernel DWC3 USB IRQ thread: `SCHED_FIFO 90` (set via `chrt` post-boot)
- ALSA kworker on audio core: `SCHED_FIFO 88` (set via `chrt`)
- Your audio thread: `SCHED_FIFO 80`
- ksoftirqd on audio core: `SCHED_FIFO 54` (set via `chrt`)
- Decode thread: `SCHED_OTHER` (normal, can be CPU-intensive)
- GTK/UI thread: `SCHED_OTHER` (never give RT to GTK)

***

## Part 6: ALSA UCM Profile for the Mojo 2

postmarketOS uses UCM to describe how to route audio on phones. For USB DACs, UCM is not strictly required — `snd-usb-audio` presents the device as a standard ALSA card. However, creating a UCM profile for the Mojo 2 enables proper integration with sound server policy and prevents WirePlumber from mangling the mixer state.[^18][^19]

```
# /usr/share/alsa/ucm2/USB-Audio/Chord-Mojo-2/Chord-Mojo-2.conf
Syntax 2

SectionUseCase."HiFi" {
    File "HiFi"
    Comment "Chord Mojo 2 Hi-Fi Playback"
}
```

```
# /usr/share/alsa/ucm2/USB-Audio/Chord-Mojo-2/HiFi.conf
SectionVerb {
    Value {
        PlaybackChannels "2"
        CaptureChannels "0"
        TQ "HiFi"
    }

    EnableSequence [
        # No mixer controls needed — Mojo 2 is fixed-output
        # Its volume is controlled by the hardware knob only
    ]

    DisableSequence []
}

SectionDevice."Playback" {
    Comment "Chord Mojo 2 Playback"
    Value {
        PlaybackPCM "hw:${CardId},0"
        PlaybackChannels "2"
    }
}
```

***

## Part 7: The `asound.conf` for Bit-Perfect Operation

When your application opens `hw:CARD=Mojo2,DEV=0` directly, ALSA routes go through the kernel driver with zero plugin processing. The `asound.conf` is only relevant for system-default routing. Set it to ensure no accidental `dmix` or `plug` processing:[^20]

```
# /etc/asound.conf

# Prevent any global default from routing through mixing plugins
pcm.!default {
    type hw
    card Mojo2
    device 0
}

ctl.!default {
    type hw
    card Mojo2
}

# Disable automatic format/rate conversion for hw: devices
# This makes ALSA return EINVAL instead of silently resampling
defaults.pcm.rate_converter "samplerate_dummy"
```

The `samplerate_dummy` rate converter returns an error instead of doing SRC — this catches any code path that accidentally triggers resampling.[^21]

***

## Part 8: GTK4/libadwaita + Audio Thread Integration

### The Golden Rule: Never Block the Audio Thread on GTK

GTK4's GLib main loop runs on the main thread and uses an event loop with `poll()`/`epoll`. It can be woken up by external events (D-Bus, Wayland compositor, input events) at any time. If your audio thread holds any lock that the GTK thread also needs, you will get priority inversion — the GTK thread blocks the audio thread, causing xruns.

**Correct pattern using `glib::Sender`**:

```rust
use glib::{MainContext, PRIORITY_DEFAULT};

// In UI code:
let (sender, receiver) = MainContext::channel::<PlayerEvent>(PRIORITY_DEFAULT);

// Attach receiver to GTK main loop — runs only on the GTK thread
receiver.attach(None, move |event| {
    match event {
        PlayerEvent::Position(frames) => update_seekbar(frames),
        PlayerEvent::TrackChanged(meta) => update_now_playing(&meta),
        PlayerEvent::Xrun => show_xrun_indicator(),
    }
    glib::Continue(true)
});

// In the decode thread — sends events to GTK without blocking:
sender.send(PlayerEvent::Position(current_frame)).ok();
```

**Never use `gtk4::glib::MainContext::default().invoke_sync()` from the audio thread** — this synchronously blocks until the GTK thread processes the invocation, which can take an arbitrary amount of time.

### Volume Control Philosophy

Since the Chord Mojo 2 has a hardware rotary volume control, your application should have **no software volume control**. Do not implement a software mixer. Do not apply any DSP, EQ, or gain. The application sends samples → ALSA `hw:` → USB → Mojo 2 hardware DAC. Any software volume multiplication changes bit values and destroys bit-perfect operation.

If the user asks for volume control in the UI, display a reminder that volume is controlled by the Mojo 2's hardware knob.

***

## Part 9: Symphonia Integration — Zero-Copy Decode Path

```rust
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct Decoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        // Use nocache I/O hint to prevent page cache pollution (ArchQ pagecache trick)
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let hint = Hint::new();
        // Disable all metadata decoding overhead for audio-only paths
        let meta_opts = MetadataOptions::default();
        let fmt_opts = FormatOptions { enable_gapless: true, ..Default::default() };

        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &fmt_opts, &meta_opts)?;

        let format = probed.format;
        let track = format.tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(/* no audio track */)?;

        let dec_opts = DecoderOptions { verify: false }; // No checksum for performance
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &dec_opts)?;

        Ok(Decoder { format, decoder, track_id: track.id })
    }

    pub fn decode_next(&mut self) -> Result<Option<AudioBufferRef<'_>>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e.into()),
            };

            if packet.track_id() != self.track_id { continue; }

            match self.decoder.decode(&packet) {
                Ok(buf) => return Ok(Some(buf)),
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
```

### Sample Conversion to S32_LE for Mojo 2

Symphonia decodes FLAC 24-bit into `AudioBuffer<i32>` with values in the range `[-2^23, 2^23-1]`. The Mojo 2 in `S32_LE` mode expects values in `[-2^31, 2^31-1]`. Left-shift by 8 bits for 24-bit content:

```rust
fn convert_to_s32le(buf: &AudioBufferRef<'_>, out: &mut Vec<i32>) {
    use symphonia::core::audio::AudioBufferRef::*;
    match buf {
        S32(b) => {
            // 32-bit FLAC: use as-is
            out.extend(b.chan(0).iter().zip(b.chan(1).iter())
                .flat_map(|(&l, &r)| [l, r]));
        }
        S24(b) => {
            // 24-bit packed: extend to 32-bit by left-shift
            out.extend(b.chan(0).iter().zip(b.chan(1).iter())
                .flat_map(|(&l, &r)| [(l.inner() as i32) << 8, (r.inner() as i32) << 8]));
        }
        S16(b) => {
            // 16-bit: left-shift to 32-bit
            out.extend(b.chan(0).iter().zip(b.chan(1).iter())
                .flat_map(|(&l, &r)| [(l as i32) << 16, (r as i32) << 16]));
        }
        _ => { /* F32, F64: convert via f64 → i32 with dither for best SQ */ }
    }
}
```

**The dithering question**: For `F32`/`F64` decoded sources (lossy codecs: MP3, AAC, Vorbis), add triangular probability density dither before truncating to integer. Dither adds noise below the noise floor of the recording — it is theoretically correct for lossy → integer conversion. For lossless sources (FLAC, ALAC, WAV PCM), never apply dither.

***

## Part 10: Gapless Playback

Gapless requires that the audio thread never stops producing samples between tracks. The decode thread must begin decoding the next track while the current track is still playing. Architecture:

```
Decode Thread:
  current_track → fill ring_a → [when ring_a is ~80% full] → start decoding next_track
  next_track    → fill ring_b →

Audio Thread:
  drain ring_a → immediately continue from ring_b (no gap, no drain/reopen)
```

**Rate/format change between gapless tracks**: If the two tracks have the same rate and format, truly gapless is achievable. If the rate changes (e.g., a 44.1 kHz track followed by a 96 kHz track), there will be a brief stop to reopen ALSA with new parameters — this is unavoidable with `snd-usb-audio`. Symphonia's `enable_gapless: true` option handles codec-level gapless (removes encoder delay and padding from MP3/AAC) but not hardware-level rate changes.

***

## Part 11: postmarketOS/Phosh-Specific Considerations

### Audio Stack Reality on beryllium

postmarketOS's audio history on beryllium is unstable — the Qualcomm WCD9340 codec and its UCM profiles have changed multiple times. When the Mojo 2 is connected via USB OTG and enumerated as `card 1`, your application should:[^22]

1. **Watch `/dev/snd/` via inotify** for device add/remove events (USB connect/disconnect)
2. **Query `aplay -l`** (via `alsa::device_name::HintIter`) on startup and after each hotplug event
3. **Prefer the Mojo 2 by VID:PID** over card index — use `libudev` or parse `/sys/class/sound/card*/` to find the card associated with `idVendor=2B0E`

### Phosh Integration Points

Phosh is a GNOME Phone Shell built on GTK4/libadwaita and uses:
- **MPRIS2 via D-Bus** — implement `org.mpris.MediaPlayer2.Player` so the Phosh lockscreen media controls work
- **`org.freedesktop.MediaSession`** — for audio focus management (stops media when phone call arrives)
- **Portal `org.freedesktop.portal.Background`** — to keep playing with the screen off

MPRIS2 implementation in Rust: use the `mpris-server` crate (crates.io). Implement `PlaybackStatus`, `Metadata`, `Position`, `CanGoNext`, `CanGoPrevious` — these are the properties Phosh's lockscreen widget queries.

```toml
[dependencies]
mpris-server = "0.8"
zbus = "4"  # D-Bus for MPRIS2 and portal API
```

### Screen-Off Playback

By default Phosh may suspend background processes. Register with the background portal and request `org.freedesktop.portal.Background.RequestBackground` with `auto_start=false, commandline=["/path/to/player"]`. More reliably, set the systemd user unit to `KillMode=none` so the audio thread survives suspend/resume.

***

## Part 12: Development Environment Setup

### Build System (following idea.md recommendations)

1. **Phase 1 — Desktop development**: build on x86_64 with GTK4 + alsa-lib. Test against any USB DAC or the system soundcard with `hw:0,0`.
2. **Phase 2 — Phosh testing**: run under nested Phosh (`phosh -T`), verify adaptive layout, MPRIS integration.
3. **Phase 3 — Cross-compilation for aarch64**: use Docker with alpine aarch64 sysroot + `PKG_CONFIG_SYSROOT_DIR`.

```dockerfile
# Dockerfile for aarch64 cross-build
FROM alpine:edge AS sysroot
RUN apk add --no-cache \
    alsa-lib-dev alsa-utils \
    gtk4.0-dev libadwaita-dev \
    --arch aarch64

FROM rust:latest AS builder
RUN apt-get install -y gcc-aarch64-linux-gnu
ENV PKG_CONFIG_SYSROOT_DIR=/sysroot
ENV PKG_CONFIG_LIBDIR=/sysroot/usr/lib/pkgconfig
RUN cargo build --target aarch64-unknown-linux-musl --release
```

### Verifying Bit-Perfection

Use `alsabat` — ALSA's built-in bit accuracy test tool:
```bash
# Loopback test through hw: device (requires loopback cable or DAC monitor)
alsabat -D hw:CARD=Mojo2,DEV=0 --rate 44100 --format S32_LE --standalone

# Check for xruns during playback:
cat /proc/asound/card1/pcm0p/sub0/status
# "state: RUNNING" is good; "state: XRUN" means the audio thread fell behind
```

Monitor xrun count:
```bash
watch -n 0.5 'cat /proc/asound/card*/pcm*/sub*/status | grep xrun'
```

***

## Implementation Checklist

- [ ] `alsa` crate v0.9 with `direct::pcm::MmapPlayback` for zero-copy DMA writes
- [ ] `symphonia` v0.5 with `flac`, `mp3`, `aac`, `alac`, `ogg`, `wav` features
- [ ] Device discovery via `alsa::device_name::HintIter` (dynamic, not hardcoded)
- [ ] Hardware parameter negotiation with exact rate match (no `ValueOr::Nearest` on rate)
- [ ] Format-specific sample conversion: S16, S24_3LE, S32_LE — never f32 intermediate for lossless
- [ ] Audio thread: `SCHED_FIFO` priority 80, pinned to CPU 7 (`taskset`)
- [ ] `/etc/security/limits.d/99-audio.conf` with `@audio rtprio 95`
- [ ] `udev` rule setting `SOUND_CLASS=none` for Mojo 2 (prevents sound server acquisition)
- [ ] WirePlumber drop rule `device.reserved = true` for Mojo 2
- [ ] `asound.conf` with `samplerate_dummy` to catch accidental SRC
- [ ] `crossbeam-channel` for UI→audio commands (zero-allocation bounded SPSC)
- [ ] `glib::MainContext::channel` for audio→UI events (GTK-thread safe)
- [ ] MPRIS2 via `mpris-server` crate for Phosh lockscreen integration
- [ ] inotify watch on `/dev/snd/` for USB hotplug
- [ ] ALSA drain + close + reopen on sample-rate change between tracks
- [ ] Software volume control: **absent by design** (hardware knob on Mojo 2)
- [ ] Gapless: double-buffer decode with `enable_gapless: true` in Symphonia

---

## References

1. [PulseAudio issues, programs conflicting and only one will play audio](https://forum.manjaro.org/t/pulseaudio-issues-programs-conflicting-and-only-one-will-play-audio/117689) - If PulseAudio is blocked from connecting to a device in ALSA, audio streams will still appear to pla...

2. [pulseaudio removed by pipewire-pulse-0.3.24-r1 - postmarketOS](https://postmarketos.org/edge/2021/04/02/pipewire-pulse/) - A recent update on edge caused the pipewire-pulse package to completely replace pulseaudio. This was...

3. [pipewire-pulse used instead of pulseaudio in new pmbootstrap installs](https://postmarketos.org/edge/2021/04/15/pipewire-pulse/) - When building postmarketOS images with the Phosh UI, pipewire-pulse gets installed instead of pulsea...

4. [PipeWire - ArchWikiwiki.archlinux.org › title › PipeWire](https://wiki.archlinux.org/title/PipeWire)

5. [WirePlumber](https://wiki.archlinux.org/title/WirePlumber)

6. [Reviewing and deciding upon a default buffer size strategy ...](https://github.com/RustAudio/cpal/issues/446) - #401 finally allows users to specify a fixed buffer size, allowing some control over the trade-off b...

7. [ALSA: fix buffer size / period size configuration by abique · Pull Request #917 · RustAudio/cpal](https://github.com/RustAudio/cpal/pull/917/files) - With ALSA, the buffer size is nperiod * period_size. We want 2 periods ideally. The default is now s...

8. [Module alsa::direct::pcm[−][src]](https://docs.rs/alsa/0.2.1/alsa/direct/pcm/index.html) - API documentation for the Rust `pcm` mod in crate `alsa`.

9. [alsa-rs/README.md at master · diwic/alsa-rs](https://github.com/diwic/alsa-rs/blob/master/README.md) - Thin but safe ALSA wrappers for Rust. Contribute to diwic/alsa-rs development by creating an account...

10. [alsa::direct::pcm - Rust - Docs.rs](https://docs.rs/alsa/latest/alsa/direct/pcm/index.html) - This module bypasses alsa-lib and directly read and write into memory mapped kernel memory. In case ...

11. [alsa - crates.io: Rust Package Registry](https://crates.io/crates/alsa/0.11.0) - ALSA bindings for Rust. Thin but safe wrappers for ALSA, the most common API for accessing audio dev...

12. [symphonia 0.4.0](https://docs.rs/crate/symphonia/0.4.0)

13. [Symphonia (pdeljanov/symphonia) | Context7](https://context7.com/pdeljanov/symphonia) - Symphonia is a pure Rust audio decoding and media demuxing library supporting AAC, ADPCM, AIFF, ALAC...

14. [Chord Mojo 2 DAC Headphone Amp Review & Setup Guide](https://www.moon-audio.com/blogs/expert-advice/chord-electronics-mojo-2-dac-amp-review) - The Apple CCK cable tells the Apple device that the Mojo 2 will output digital audio. You will need ...

15. [Realtime audio scheduling: RTKIT vs. RLIMIT_RTPRIO ... - GitHub](https://github.com/rerdavies/pipedal/discussions/99) - RLIMIT typically allows only users who belong to a group (usually the audio group, but sometimes the...

16. [How do I configure my linux system to allow JACK to use realtime ...](https://jackaudio.org/faq/linux_rt_config.html) - You need to carry out 3 steps to be able to run JACK with RT scheduling. In what follows, several re...

17. [change Linux thread priority to real time SCHED_FIFO](https://stackoverflow.com/questions/72593494/change-linux-thread-priority-to-real-time-sched-fifo) - I am trying to change Linux thread priority to real time SCHED_FIFO by pthread_setschedparam. I am g...

18. [The Use case definition](https://wiki.postmarketos.org/wiki/Alsa_UCM)

19. [Alsa UCM](https://wiki.postmarketos.org/wiki/Troubleshooting:alsa)

20. [the C library reference: PCM (digital audio) plugins](https://www.alsa-project.org/alsa-doc/alsa-lib/pcm_plugins.html)

21. [Plugins](https://wiki.archlinux.org/title/Advanced_Linux_Sound_Architecture)

22. [idea.md](https://ppl-ai-file-upload.s3.amazonaws.com/web/direct-files/attachments/16322705/4f44c8f1-b527-479a-90ee-30555caf31dd/idea.md?AWSAccessKeyId=ASIA2F3EMEYEV2PIONBF&Signature=rIlB7rhHTFjG553X%2B0arpyIDEaQ%3D&x-amz-security-token=IQoJb3JpZ2luX2VjEC0aCXVzLWVhc3QtMSJHMEUCIBfcsjEl2B1FgxUds3yMP78tsjJuXUt0ZPPfuBTVUtThAiEAiCcA5atPvnt0S5%2BcWh2F1q2ZNm3I7fc54aSsyXNNx2cq%2FAQI9v%2F%2F%2F%2F%2F%2F%2F%2F%2F%2FARABGgw2OTk3NTMzMDk3MDUiDHBCTE4exr3TTkBs9CrQBK2egsFhxWmT62RNDq3XBQ8S%2BlgBUnr%2FkMliOk%2Fy1CJYYY5iijYO4yf9NThRzywp8N5LQtEGkiDaKq4N66MjTBjRuTO6mIIkrLHFwX98lDWdZ76%2FBTMJ78%2FCmRXSmLvX%2BlaSnuk1d4Ur76ez71AOn0fAxXCdUeZxDxTV5M0gBm5ZfLm%2BXdSF4MG7FU5cOh6S1YavfUQy3%2BNoiIKilQFiBZoRiqzjKcgMMn0SLHrWQ7yD7OIaupkw6IqjXxeGcXO66C76WU0ZglBF7oa%2BsB5qvK7d2pGLeU6cA%2Fsuc%2FwKYCk88AzgUXV5wzFMoOXv08cqkWB%2BKAm1FX9YDYnkWmb3EQfIPnJC2T4mdDNk5AUqOxIjjaFCPTZDHsjM45r8J1YA1gzXcbMm7ho9HPueaDR0Na8BnYglSnepkBLbtaYLb8%2FAmSDl5YE1194cysoZHPx3II%2FnviNm99wFczqr0ucHEAqlJetMnwlJwxZkIc%2F0N%2F9AfnZBiZUdInuQGXgsbE6dr7KAAY0vpEtoLoCniIXfVdoWk%2BSn3KaJAxZVe7moghbnL9EWDL2UufSvob45QjKCMfpp%2FO6teNq6Nz83zG%2B6mdtwihSe8bAxsOLfQ%2B6QCZsM3GoBw2urrhfd2GM7p2Y4z0knsi732gicpfWzNkEHKPrj48o0GyGlmFqlhMWdMrfJSidaAbttJ2%2FeeObQWSb%2FJoI9AcVuwaEKUusKzIWib4AiQttFF1RPgGRezt2AOxj8bEkvDllSaxmMZ3KppJUHEG%2B2f1dgvnMTLil8oDG0tzIw%2BtDw0AY6mAF%2BOT7s1HTjDj40XHznBfQ97%2Bu4zvmG3vnPWSngH62i9bN9yNCuAZkSthNB4oPRurPSu6ZsOETXXDYHSFlj%2F5TgbHyu7YUCFSAaIxRpysqoP1%2FbQJPjgH6SJb44oIHrvtCGXgZl46%2F7HRWL7017rc2vdXdRe3k4eO4%2Bg%2BJUCcprZ9dsBo7O4%2FsGW3jPAsVRaj38zzlpDUkobQ%3D%3D&Expires=1780233805) - Build it as an adaptive Linux player first, then as a phone player. For Poco F1 on postmarketOS/Phos...

