//! Blocking bit-perfect ALSA `hw:` output. Exact rate/format negotiation; any
//! attempt by ALSA to resample is turned into a hard `RateMismatch` error.

use std::cell::Cell;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::convert::OutFrames;
use crate::dop::dop_silence;
use crate::error::{Error, Result};
use crate::format::{DeviceFormats, StreamSpec};

/// Silence guard band (ms) clocked out after any stream re-initialization (xrun
/// or suspend recovery) before real audio resumes. The Mojo 2 has no hardware
/// mute during digital transitions, so a full-scale sample emitted in that
/// window is the "burst" hazard (see CLAUDE.md "Output safety"); leading silence
/// makes it impossible. Engine-only — see [`AlsaSink::enable_reinit_guard`].
const REINIT_GUARD_MS: usize = 48;

pub struct AlsaSink {
    pcm: PCM,
    channels: usize,
    spec: StreamSpec,
    /// Whether the device reports hardware pause support (`snd_pcm_hw_params_can_pause`).
    /// When false, the engine holds pause with a silence keep-alive (never drop()).
    can_pause: bool,
    /// Count of recovered underruns (EPIPE) seen while writing — diagnostics for
    /// the RT path (Phase 2). Not shared across threads; the sink lives on the
    /// audio thread only.
    xruns: Cell<u64>,
    /// When set, lead every post-reinit resume (xrun/suspend recovery) with a
    /// silence guard band ([`REINIT_GUARD_MS`]) so the Mojo 2 cannot emit a
    /// full-scale burst. Enabled by the engine; off for the `run_playback` oracle
    /// and the loopback verifier so their bytes stay exact.
    guard_reinit: Cell<bool>,
}

impl AlsaSink {
    /// Open `device` (e.g. `hw:1,0`) for `spec`. `period` frames per period and
    /// `periods` periods of buffering (favouring robustness over latency for v1).
    pub fn open(device: &str, spec: StreamSpec, period: i64, periods: i64) -> Result<Self> {
        let pcm = PCM::new(device, Direction::Playback, false)?;
        configure_hw(&pcm, spec, period, periods)?;

        // Software params: start once a period is buffered; wake every period.
        // Scoped so the borrowing HwParams/SwParams drop before `pcm` is moved.
        let can_pause = {
            let hwc = pcm.hw_params_current()?;
            let actual_period = hwc.get_period_size()?;
            let actual_buffer = hwc.get_buffer_size().unwrap_or(0);
            let can_pause = hwc.can_pause();
            let swp = pcm.sw_params_current()?;
            swp.set_start_threshold(actual_period)?;
            swp.set_avail_min(actual_period)?;
            pcm.sw_params(&swp)?;
            // Diagnostics: `can_pause` decides the pause strategy (hardware pause
            // vs. the silence keep-alive in the audio thread). Logged so a real
            // Mojo 2 confirms which branch it takes — see audio_thread::pause.
            eprintln!(
                "[alsa] open device={device} fmt={} rate={} ch={} dop={} period={actual_period} buffer={actual_buffer} can_pause={can_pause}",
                spec.fmt.label(),
                spec.rate,
                spec.channels,
                spec.is_dop,
            );
            can_pause
        };

        Ok(Self {
            pcm,
            channels: spec.channels as usize,
            spec,
            can_pause,
            xruns: Cell::new(0),
            guard_reinit: Cell::new(false),
        })
    }

    pub fn spec(&self) -> StreamSpec {
        self.spec
    }

    /// Number of recovered underruns since this sink was opened.
    pub fn xruns(&self) -> u64 {
        self.xruns.get()
    }

    /// Enable the post-reinit silence guard ([`REINIT_GUARD_MS`]). Used by the
    /// real-time engine; left off for the v1 `run_playback` oracle and the
    /// loopback verifier so their captured bytes stay exact.
    pub fn enable_reinit_guard(&self) {
        self.guard_reinit.set(true);
    }

    /// Recover from a write error. Underruns (EPIPE) and suspend (ESTRPIPE) both
    /// **re-prepare** the PCM, which on the Mojo 2 opens the full-scale-burst
    /// window; when the guard is enabled we clock out a silence band before the
    /// caller resumes writing real samples.
    fn recover(&self, e: alsa::Error) -> Result<()> {
        let errno = e.errno();
        let reinit = errno == libc::EPIPE || errno == libc::ESTRPIPE;
        if errno == libc::EPIPE {
            let n = self.xruns.get() + 1;
            self.xruns.set(n);
            eprintln!("[alsa] xrun #{n} recovered (EPIPE)");
        } else if errno == libc::ESTRPIPE {
            eprintln!("[alsa] stream suspended (ESTRPIPE) — recovering");
        }
        self.pcm.try_recover(e, true)?;
        if reinit && self.guard_reinit.get() {
            self.prime_reinit_silence();
        }
        Ok(())
    }

    /// Clock out [`REINIT_GUARD_MS`] of digital silence into a freshly
    /// (re)prepared stream so no full-scale sample is emitted during the DAC's
    /// unmuted-but-unattenuated reinit window (the Mojo 2 burst hazard). PCM →
    /// zeros; DoP → valid-marker DSD silence (also re-locks DoP). Best-effort: a
    /// write error is swallowed (the caller's next write recovers again if
    /// needed). Leading silence only — never part of the bit-perfect music.
    fn prime_reinit_silence(&self) {
        let frames = (self.spec.rate as usize / 1000) * REINIT_GUARD_MS;
        let fb = self.spec.bytes_per_frame();
        let silence = if self.spec.is_dop {
            dop_silence(self.spec.fmt, self.spec.channels, frames & !1)
        } else {
            vec![0u8; frames * fb]
        };
        let io = self.pcm.io_bytes();
        let mut off = 0;
        while off < silence.len() {
            match io.writei(&silence[off..]) {
                Ok(n) => off += n * fb,
                Err(_) => break,
            }
        }
    }

    /// Write one packed block via the typed interface (the engine hot path).
    pub fn write(&self, frames: OutFrames<'_>) -> Result<()> {
        match frames {
            OutFrames::S16(b) => self.write_i16(b),
            OutFrames::S24(b) => self.write_bytes(b),
            OutFrames::S32(b) => self.write_i32(b),
        }
    }

    /// Write raw interleaved bytes already in the negotiated format (loopback path).
    pub fn write_all_bytes(&self, bytes: &[u8]) -> Result<()> {
        self.write_bytes(bytes)
    }

    pub fn drain(&self) -> Result<()> {
        self.pcm.drain()?;
        Ok(())
    }

    /// Hardware pause/resume (the DAC clock halts, ALSA's buffer is preserved,
    /// resume is seamless). Bit-perfect: samples are never touched.
    ///
    /// **Only valid when [`AlsaSink::can_pause`] is true** — the audio thread
    /// calls this only on that branch. Devices WITHOUT hardware pause (e.g. the
    /// Mojo 2) are deliberately **never** `drop()`/`prepare()`'d to pause: that
    /// re-inits the USB substream and can make the DAC emit a full-scale burst on
    /// resume. They are held open with a silence keep-alive instead (see
    /// `audio_thread::pause_until_resumed`).
    pub fn pause(&self, enable: bool) -> Result<()> {
        debug_assert!(
            self.can_pause,
            "pause() is only for hardware-pause devices; others use the silence keep-alive"
        );
        self.pcm.pause(enable)?;
        Ok(())
    }

    /// Whether this device supports true hardware pause. The audio thread uses
    /// this to choose the pause strategy: hardware pause when true, else the
    /// silence keep-alive that never drops the stream.
    pub fn can_pause(&self) -> bool {
        self.can_pause
    }

    fn write_i16(&self, buf: &[i16]) -> Result<()> {
        let io = self.pcm.io_i16()?;
        let mut off = 0;
        while off < buf.len() {
            match io.writei(&buf[off..]) {
                Ok(frames) => off += frames * self.channels,
                Err(e) => self.recover(e)?,
            }
        }
        Ok(())
    }

    fn write_i32(&self, buf: &[i32]) -> Result<()> {
        let io = self.pcm.io_i32()?;
        let mut off = 0;
        while off < buf.len() {
            match io.writei(&buf[off..]) {
                Ok(frames) => off += frames * self.channels,
                Err(e) => self.recover(e)?,
            }
        }
        Ok(())
    }

    fn write_bytes(&self, buf: &[u8]) -> Result<()> {
        let io = self.pcm.io_bytes();
        let frame_bytes = self.spec.bytes_per_frame();
        let mut off = 0;
        while off < buf.len() {
            match io.writei(&buf[off..]) {
                Ok(frames) => off += frames * frame_bytes,
                Err(e) => self.recover(e)?,
            }
        }
        Ok(())
    }
}

/// Shared hw-param negotiation used by both playback and capture.
pub(crate) fn configure_hw(
    pcm: &PCM,
    spec: StreamSpec,
    period: i64,
    periods: i64,
) -> Result<()> {
    let hwp = HwParams::any(pcm)?;
    hwp.set_access(Access::RWInterleaved)?;
    hwp.set_format(spec.fmt.to_alsa())?;
    hwp.set_channels(spec.channels)?;
    // Exact rate: ask for nearest, then verify it landed exactly.
    hwp.set_rate(spec.rate, ValueOr::Nearest)?;
    hwp.set_period_size_near(period, ValueOr::Nearest)?;
    hwp.set_buffer_size_near(period * periods)?;
    pcm.hw_params(&hwp)?;

    let actual = pcm.hw_params_current()?.get_rate()?;
    if actual != spec.rate {
        return Err(Error::RateMismatch {
            requested: spec.rate,
            actual,
        });
    }
    Ok(())
}

/// Probe which sample formats `device` accepts. Opens the device only for HW
/// param inspection, then closes it (so it must be called while the device is
/// free — e.g. before playback starts). Used for device-aware, bit-perfect
/// format selection ([`DeviceFormats::choose`]).
pub fn probe_formats(device: &str) -> Result<DeviceFormats> {
    let pcm = PCM::new(device, Direction::Playback, false)?;
    let hwp = HwParams::any(&pcm)?;
    Ok(DeviceFormats {
        s16: hwp.test_format(Format::S16LE).is_ok(),
        s24_3: hwp.test_format(Format::S243LE).is_ok(),
        s32: hwp.test_format(Format::S32LE).is_ok(),
    })
}
