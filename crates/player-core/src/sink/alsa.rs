//! Blocking bit-perfect ALSA `hw:` output. Exact rate/format negotiation; any
//! attempt by ALSA to resample is turned into a hard `RateMismatch` error.

use std::cell::Cell;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::convert::OutFrames;
use crate::error::{Error, Result};
use crate::format::{DeviceFormats, StreamSpec};

pub struct AlsaSink {
    pcm: PCM,
    channels: usize,
    spec: StreamSpec,
    /// Whether the device reports hardware pause support (`snd_pcm_hw_params_can_pause`).
    /// When false, [`AlsaSink::pause`] falls back to drop+prepare.
    can_pause: bool,
    /// Count of recovered underruns (EPIPE) seen while writing — diagnostics for
    /// the RT path (Phase 2). Not shared across threads; the sink lives on the
    /// audio thread only.
    xruns: Cell<u64>,
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
            let can_pause = hwc.can_pause();
            let swp = pcm.sw_params_current()?;
            swp.set_start_threshold(actual_period)?;
            swp.set_avail_min(actual_period)?;
            pcm.sw_params(&swp)?;
            can_pause
        };

        Ok(AlsaSink {
            pcm,
            channels: spec.channels as usize,
            spec,
            can_pause,
            xruns: Cell::new(0),
        })
    }

    pub fn spec(&self) -> StreamSpec {
        self.spec
    }

    /// Number of recovered underruns since this sink was opened.
    pub fn xruns(&self) -> u64 {
        self.xruns.get()
    }

    /// Recover from a write error, counting underruns (EPIPE) for diagnostics.
    fn recover(&self, e: alsa::Error) -> Result<()> {
        if e.errno() == libc::EPIPE {
            self.xruns.set(self.xruns.get() + 1);
        }
        self.pcm.try_recover(e, true)?;
        Ok(())
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

    /// Pause or resume output. Bit-perfect: samples are never touched. When the
    /// device supports hardware pause the DAC clock simply halts (ALSA's buffer
    /// is preserved, so resume continues seamlessly); otherwise we fall back to
    /// drop+prepare, which discards only ALSA's already-buffered audio (a brief
    /// gap on resume) and never alters samples.
    pub fn pause(&self, enable: bool) -> Result<()> {
        if self.can_pause {
            self.pcm.pause(enable)?;
        } else if enable {
            self.pcm.drop()?;
        } else {
            self.pcm.prepare()?;
        }
        Ok(())
    }

    /// Whether this device supports true hardware pause (vs. the drop+prepare
    /// fallback). The audio thread uses this to decide if it must re-prime the
    /// ALSA buffer on resume.
    pub fn can_pause(&self) -> bool {
        self.can_pause
    }

    /// Stop the stream and discard ALSA's buffered audio, then re-prepare so the
    /// next `write` restarts playback from a clean state. Used for seek/flush.
    pub fn reset(&self) -> Result<()> {
        self.pcm.drop()?;
        self.pcm.prepare()?;
        Ok(())
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
