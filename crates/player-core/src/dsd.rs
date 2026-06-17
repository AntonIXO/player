//! DSD source abstraction. A [`DsdSource`] yields **interleaved, MSB-first** DSD
//! bytes (DSDIFF/Scarletbook "clustered frame" layout — see [`crate::dop`]); the
//! engine wraps that stream with a [`crate::dop::DopPacker`] into a bit-perfect
//! DoP PCM stream and pushes the bytes through the same ring/sink path as PCM.
//!
//! Concrete readers: `.dsf`/`.dff` via a direct-offset reader (so a rewind seeks
//! by byte offset instead of rescanning from the start), `.iso` (SACD) via the
//! in-tree `sacd` crate. The [`open_dsd`] factory dispatches on extension.

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
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
        .map(str::to_ascii_lowercase)
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
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("dsf" | "dff" | "iso")
    )
}

/// DFF read granularity (bytes per `next()` for the no-transform interleaved
/// path). Any size works — [`crate::dop::DopPacker`] carries partial frames — so
/// this just trades syscalls for buffer size.
const DFF_CHUNK: usize = 64 * 1024;

/// On-disc layout of a `.dsf`/`.dff` audio stream.
enum Container {
    /// DSDIFF: audio is already **interleaved, MSB-first** raw DSD, so a DoP
    /// frame maps to a plain byte offset and reads need no transform.
    Dff,
    /// DSF: audio is **planar, LSB-first** in fixed `block_size`-per-channel
    /// blocks laid out block-interleaved (`[ch0 blk][ch1 blk][ch0 blk]…`). We
    /// read a whole super-block, optionally bit-reverse (`reverse` when the file
    /// stores LSB-first, i.e. `bits_per_sample == 1`), and interleave across
    /// channels into clustered-frame order.
    Dsf { block_size: usize, reverse: bool },
}

/// Direct-offset `.dsf`/`.dff` reader. Yields **interleaved, MSB-first** DSD in
/// the clustered-frame layout [`crate::dop::DopPacker`] expects — byte-identical
/// to what the `dsd-reader` iterator produced (proved in `tests/dsd_dop.rs`).
/// Unlike a forward-only iterator it seeks by computing the exact file offset of
/// the target DoP frame, so a backward seek (rewind) is O(1) instead of a rescan
/// from the track start.
struct DsfDffSource {
    file: File,
    container: Container,
    /// File byte offset where audio data begins.
    data_offset: u64,
    channels: usize,
    spec: DsdSpec,
    /// Total valid DSD bytes in the stream (excludes any DSF block padding).
    valid_total: u64,
    /// Valid (output) DSD bytes already emitted — the seek anchor.
    pos: u64,
    /// Reused raw-read scratch (DSF super-block).
    rbuf: Vec<u8>,
}

impl DsfDffSource {
    fn open(path: &Path) -> Result<Self> {
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("dsf") => Self::open_dsf(path),
            Some("dff") => Self::open_dff(path),
            _ => Err(Error::Dsd(format!("not a .dsf/.dff file: {}", path.display()))),
        }
    }

    fn open_dsf(path: &Path) -> Result<Self> {
        let meta = dsf_meta::DsfFile::open(path).map_err(|e| Error::Dsd(e.to_string()))?;
        let fmt = meta.fmt_chunk();
        let channels = fmt.channel_num() as usize;
        let block_size = fmt.block_size_per_channel() as usize;
        let reverse = fmt.bits_per_sample() == 1; // 1 = LSB-first → reverse to MSB-first
        // `sample_count` is samples (bits) per channel → /8 bytes per channel.
        let valid_total = (fmt.sample_count() / 8).saturating_mul(channels as u64);
        let container = Container::Dsf { block_size, reverse };
        Self::finish(File::open(path)?, container, dsf_meta::DSF_SAMPLE_DATA_OFFSET, channels, fmt.sampling_frequency(), valid_total)
    }

    fn open_dff(path: &Path) -> Result<Self> {
        let meta = dff_meta::DffFile::open(path).map_err(|e| Error::Dsd(e.to_string()))?;
        let channels = meta.get_num_channels().map_err(|e| Error::Dsd(e.to_string()))?;
        let dsd_rate = meta.get_sample_rate().map_err(|e| Error::Dsd(e.to_string()))?;
        Self::finish(File::open(path)?, Container::Dff, meta.get_dsd_data_offset(), channels, dsd_rate, meta.get_audio_length())
    }

    fn finish(file: File, container: Container, data_offset: u64, channels: usize, dsd_rate: u32, valid_total: u64) -> Result<Self> {
        if channels == 0 {
            return Err(Error::Dsd("DSD file reports zero channels".into()));
        }
        // Never read past the file (guards a corrupt header); DSF block padding
        // already keeps the on-disc size ≥ valid_total, so this is a no-op there.
        // Keep it a whole number of channels so the DSF per-channel block math
        // always reaches the end (a no-op for valid files — DSD byte counts are
        // already a multiple of the channel count).
        let file_len = file.metadata()?.len();
        let valid_total = valid_total.min(file_len.saturating_sub(data_offset));
        let valid_total = valid_total - valid_total % channels as u64;
        let n_frames = (valid_total > 0).then(|| valid_total / (2 * channels as u64));
        Ok(Self {
            file,
            container,
            data_offset,
            channels,
            spec: DsdSpec { dsd_rate, channels: channels as u32, n_frames },
            valid_total,
            pos: 0,
            rbuf: Vec::new(),
        })
    }
}

/// Read up to `buf.len()` bytes, returning how many were read (tolerates a short
/// final block / EOF rather than erroring like `read_exact`).
fn read_up_to(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut off = 0;
    while off < buf.len() {
        match file.read(&mut buf[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(off)
}

impl DsdSource for DsfDffSource {
    fn next(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();
        if self.pos >= self.valid_total {
            return Ok(false);
        }
        match self.container {
            Container::Dff => {
                let n = (self.valid_total - self.pos).min(DFF_CHUNK as u64) as usize;
                self.file.seek(SeekFrom::Start(self.data_offset + self.pos))?;
                out.resize(n, 0);
                self.file.read_exact(out)?;
                self.pos += n as u64;
                Ok(true)
            }
            Container::Dsf { block_size, reverse } => {
                let ch = self.channels;
                let super_block = block_size * ch; // on-disc bytes == full-block output bytes
                let block_idx = self.pos / super_block as u64;
                let block_out_start = block_idx * super_block as u64;
                // Valid per-channel bytes in this block (the last block may be short).
                let valid_per_chan = self.valid_total / ch as u64;
                let per_chan = block_size.min((valid_per_chan - block_idx * block_size as u64) as usize);
                // Read the on-disc super-block (channels are block-interleaved, so
                // every channel's block must be in hand to interleave them).
                self.file.seek(SeekFrom::Start(self.data_offset + block_out_start))?;
                self.rbuf.resize(super_block, 0);
                let got = read_up_to(&mut self.file, &mut self.rbuf)?;
                // Transform the valid region to clustered-frame MSB-first.
                out.reserve(per_chan * ch);
                for i in 0..per_chan {
                    for c in 0..ch {
                        let idx = c * block_size + i;
                        if idx < got {
                            let b = self.rbuf[idx];
                            out.push(if reverse { b.reverse_bits() } else { b });
                        }
                    }
                }
                // Drop the within-block prefix a mid-block seek landed inside.
                let skip = (self.pos - block_out_start) as usize;
                if skip > 0 {
                    out.drain(..skip.min(out.len()));
                }
                self.pos += out.len() as u64;
                Ok(true)
            }
        }
    }

    fn spec(&self) -> DsdSpec {
        self.spec
    }

    fn seek(&mut self, dop_frame: u64) -> Result<Option<u64>> {
        // One DoP frame is 2 DSD bytes per channel; land on a frame boundary,
        // clamped to end of stream.
        let bpf = 2 * self.channels as u64;
        let target = dop_frame.saturating_mul(bpf).min(self.valid_total);
        self.pos = target - (target % bpf); // frame-align (valid_total may not be whole frames)
        Ok(Some(self.pos / bpf))
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
        Ok(Self { reader, spec })
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
