//! DSD source abstraction. A [`DsdSource`] yields **interleaved, MSB-first** DSD
//! bytes (DSDIFF/Scarletbook "clustered frame" layout — see [`crate::dop`]); the
//! engine wraps that stream with a [`crate::dop::DopPacker`] into a bit-perfect
//! DoP PCM stream and pushes the bytes through the same ring/sink path as PCM.
//!
//! Concrete readers: `.dsf`/`.dff` via the `dsd-reader` crate, `.iso` (SACD) via
//! the in-tree `sacd` crate. The [`open_dsd`] factory dispatches on extension.

use std::path::Path;

use crate::error::{Error, Result};
use crate::format::{AlsaFmt, StreamSpec};

/// DSD64 sample rate (64 × 44.1 kHz).
pub const DSD64_RATE: u32 = 2_822_400;

/// Describes a DSD stream independent of how it reaches the DAC.
#[derive(Clone, Copy, Debug)]
pub struct DsdSpec {
    /// DSD sample rate: 2_822_400 (DSD64), 5_644_800 (DSD128), 11_289_600
    /// (DSD256), … Always a multiple of 44_100 × 64.
    pub dsd_rate: u32,
    pub channels: u32,
    /// Total length in DoP PCM frames (DSD samples / 16), if known — for
    /// position/duration reporting. `None` when the source can't report it.
    pub n_frames: Option<u64>,
}

impl DsdSpec {
    /// The PCM [`StreamSpec`] of the DoP stream carrying this DSD: rate is the
    /// DSD rate / 16 (16 DSD bits per DoP sample) in container `fmt` (`S24_3` or
    /// `S32`).
    pub fn dop_spec(&self, fmt: AlsaFmt) -> StreamSpec {
        StreamSpec::dop(self.dsd_rate / 16, self.channels, fmt)
    }

    /// The DSD "subclass" multiple (64/128/256/…), or 0 if not a clean multiple.
    pub fn dsd_multiple(&self) -> u32 {
        self.dsd_rate / 44_100
    }

    /// A short label like `DSD64`.
    pub fn label(&self) -> String {
        format!("DSD{}", self.dsd_multiple())
    }
}

/// A source of native DSD frames. `next` fills `out` with the next block of
/// interleaved MSB-first DSD bytes and returns `false` at end of stream.
pub trait DsdSource {
    fn next(&mut self, out: &mut Vec<u8>) -> Result<bool>;
    fn spec(&self) -> DsdSpec;
    /// Seek so the next `next()` yields the block at `dop_frame` (a DoP PCM frame
    /// = 16 DSD samples, the engine's position unit = `dsd_rate / 16`). Returns
    /// the landed frame, or `None` if this source can't seek. Repositioning lands
    /// on a real frame/block boundary, so the DSD it then yields is bit-exact.
    fn seek(&mut self, _dop_frame: u64) -> Result<Option<u64>> {
        Ok(None)
    }
}

/// Open a DSD file as a [`DsdSource`]. Dispatches on extension: `.dsf`/`.dff`
/// via the `dsd-reader` crate, `.iso` (SACD) via the `sacd` crate. `track`
/// selects a track within a multi-track SACD `.iso` (ignored for `.dsf`/`.dff`).
pub fn open_dsd(path: &Path, track: Option<usize>) -> Result<Box<dyn DsdSource>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "dsf" | "dff" => Ok(Box::new(DsfDffSource::open(path)?)),
        "iso" => Ok(Box::new(SacdSource::open(path, track.unwrap_or(0))?)),
        other => Err(Error::Unsupported(format!(
            "not a DSD file: .{other} ({})",
            path.display()
        ))),
    }
}

/// True if `path` looks like a DSD container we can open.
pub fn is_dsd_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("dsf" | "dff" | "iso")
    )
}

/// `.dsf` / `.dff` source backed by the `dsd-reader` crate. We request its
/// **interleaved, MSB-first** iterator, which yields the clustered-frame layout
/// the [`crate::dop::DopPacker`] expects (and normalizes DSF's native LSB-first
/// bit order for us).
struct DsfDffSource {
    /// Kept so a backward seek can re-`interl_iter` from the top (the iterator is
    /// forward-only).
    reader: dsd_reader::DsdReader,
    iter: dsd_reader::DsdIter,
    spec: DsdSpec,
    /// DSD bytes yielded so far — the seek anchor.
    pos_bytes: u64,
    /// A block read past the seek target and stashed to be yielded next.
    pending: Option<Vec<u8>>,
}

impl DsfDffSource {
    fn open(path: &Path) -> Result<Self> {
        let reader = dsd_reader::DsdReader::from_container(path.to_path_buf())
            .map_err(|e| Error::Dsd(e.to_string()))?;
        let channels = reader.channels_num() as u32;
        // `dsd_rate()` is the multiple over DSD64 (1/2/4/8).
        let dsd_rate = DSD64_RATE.saturating_mul(reader.dsd_rate().max(1) as u32);
        let audio_len = reader.audio_length();
        // One DoP frame consumes 2 DSD bytes per channel.
        let n_frames = (channels > 0 && audio_len > 0).then(|| audio_len / (2 * channels as u64));
        let iter = reader
            .interl_iter(false, None)
            .map_err(|e| Error::Dsd(e.to_string()))?;
        Ok(DsfDffSource {
            reader,
            iter,
            spec: DsdSpec {
                dsd_rate,
                channels,
                n_frames,
            },
            pos_bytes: 0,
            pending: None,
        })
    }

    /// Pull the next interleaved block (pending first, else the iterator), or
    /// `None` at end of stream. Does not advance `pos_bytes`.
    fn next_block(&mut self) -> Option<Vec<u8>> {
        if let Some(p) = self.pending.take() {
            return Some(p);
        }
        match self.iter.next() {
            // Interleaved output is a single buffer; `read_size` is its valid
            // length (the whole clustered frame for all channels).
            Some((read_size, bufs)) => bufs.first().map(|buf| {
                let n = read_size.min(buf.len());
                buf[..n].to_vec()
            }),
            None => None,
        }
    }
}

impl DsdSource for DsfDffSource {
    fn next(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();
        match self.next_block() {
            Some(block) => {
                self.pos_bytes += block.len() as u64;
                out.extend_from_slice(&block);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn spec(&self) -> DsdSpec {
        self.spec
    }

    fn seek(&mut self, dop_frame: u64) -> Result<Option<u64>> {
        // 2 DSD bytes per channel make one DoP frame.
        let bpf = 2 * self.spec.channels.max(1) as u64;
        let target = dop_frame.saturating_mul(bpf);
        // The iterator only moves forward; rewind by recreating it from the top.
        if target < self.pos_bytes {
            self.iter = self
                .reader
                .interl_iter(false, None)
                .map_err(|e| Error::Dsd(e.to_string()))?;
            self.pos_bytes = 0;
            self.pending = None;
        }
        // Discard whole blocks until one would straddle the target; stash it so
        // playback resumes from that block's (frame-aligned) start.
        loop {
            let Some(block) = self.next_block() else {
                return Ok(Some(self.pos_bytes / bpf)); // sought past end
            };
            if self.pos_bytes + block.len() as u64 > target {
                self.pending = Some(block);
                return Ok(Some(self.pos_bytes / bpf));
            }
            self.pos_bytes += block.len() as u64;
        }
    }
}

/// SACD `.iso` source backed by the `sacd` crate (Scarletbook parse + DST
/// decode). Defaults to the stereo area; multichannel selection is a later step.
struct SacdSource {
    reader: sacd::SacdTrackReader,
    spec: DsdSpec,
}

impl SacdSource {
    fn open(path: &Path, track: usize) -> Result<Self> {
        let img = sacd::SacdImage::open(path).map_err(|e| Error::Dsd(e.to_string()))?;
        // Prefer the stereo area; fall back to whatever the disc has.
        let area = img
            .areas()
            .iter()
            .map(|a| a.area)
            .find(|a| *a == sacd::Area::Stereo)
            .or_else(|| img.areas().first().map(|a| a.area))
            .ok_or_else(|| Error::Dsd("SACD image has no playable area".into()))?;
        let info = img
            .areas()
            .iter()
            .find(|a| a.area == area)
            .ok_or_else(|| Error::Dsd("SACD area vanished".into()))?;
        // DoP frames = track seconds × (dsd_rate / 16), from the TOC duration.
        let n_frames = info.tracks.get(track).map(|t| {
            (t.duration.as_secs_f64() * (info.dsd_rate as f64 / 16.0)).round() as u64
        });
        let spec = DsdSpec {
            dsd_rate: info.dsd_rate,
            channels: info.channels,
            n_frames,
        };
        let reader = img
            .reader(area, track)
            .map_err(|e| Error::Dsd(e.to_string()))?;
        Ok(SacdSource { reader, spec })
    }
}

impl DsdSource for SacdSource {
    fn next(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();
        self.reader.next(out).map_err(|e| Error::Dsd(e.to_string()))
    }

    fn spec(&self) -> DsdSpec {
        self.spec
    }

    fn seek(&mut self, dop_frame: u64) -> Result<Option<u64>> {
        // Each SACD frame is a fixed 1/75 s, i.e. `dsd_rate / 1200` DoP frames
        // (2352 at DSD64). Seek by counting whole frames.
        let dpf = (self.spec.dsd_rate as u64 / 1200).max(1);
        let target_frame = dop_frame / dpf;
        let landed = self
            .reader
            .seek_frame(target_frame)
            .map_err(|e| Error::Dsd(e.to_string()))?;
        Ok(Some(landed * dpf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_spec_maps_dsd64_to_176k4() {
        let s = DsdSpec {
            dsd_rate: 2_822_400,
            channels: 2,
            n_frames: None,
        };
        let dop = s.dop_spec(AlsaFmt::S32);
        assert_eq!(dop.rate, 176_400);
        assert_eq!(dop.channels, 2);
        assert!(dop.is_dop);
        assert_eq!(dop.fmt, AlsaFmt::S32);
        assert_eq!(s.label(), "DSD64");
    }

    #[test]
    fn dsd128_maps_to_352k8() {
        let s = DsdSpec {
            dsd_rate: 5_644_800,
            channels: 2,
            n_frames: None,
        };
        assert_eq!(s.dop_spec(AlsaFmt::S24_3).rate, 352_800);
        assert_eq!(s.label(), "DSD128");
    }
}
