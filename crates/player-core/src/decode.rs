//! Symphonia decode wrapper. Produces interleaved full-scale `i32` blocks.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder as SymDecoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::error::{Error, Result};
use crate::format::StreamSpec;

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymDecoder>,
    track_id: u32,
    sbuf: Option<SampleBuffer<i32>>,
    pub spec: StreamSpec,
    pub codec_name: &'static str,
    /// Total frames if known (from the container).
    pub n_frames: Option<u64>,
    /// Stop after this many frames have been emitted since the last `set_limit`
    /// (used for `.cue` tracks that decode a sub-range). `None` = decode to EOF.
    limit_frames: Option<u64>,
    /// Frames emitted toward `limit_frames`.
    frames_emitted: u64,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let fmt_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        let meta_opts = MetadataOptions::default();

        let probed =
            symphonia::default::get_probe().format(&hint, mss, &fmt_opts, &meta_opts)?;
        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(Error::NoTrack)?;
        let track_id = track.id;
        let cp = &track.codec_params;

        let rate = cp.sample_rate.ok_or(Error::NoTrack)?;
        let channels = cp.channels.map(|c| c.count() as u32).ok_or(Error::NoTrack)?;
        // FLAC always reports bits_per_sample; default to 32 (-> S32_LE) if absent.
        let source_bits = cp.bits_per_sample.unwrap_or(32);
        let n_frames = cp.n_frames;

        let codec_name = symphonia::default::get_codecs()
            .get_codec(cp.codec)
            .map(|d| d.short_name)
            .unwrap_or("unknown");

        let decoder =
            symphonia::default::get_codecs().make(cp, &DecoderOptions::default())?;

        Ok(Decoder {
            format,
            decoder,
            track_id,
            sbuf: None,
            spec: StreamSpec::new(rate, channels, source_bits),
            codec_name,
            n_frames,
            limit_frames: None,
            frames_emitted: 0,
        })
    }

    /// Decode only the next `frames` frames (from the current position), then
    /// report EOF — for `.cue` tracks. Call after [`Decoder::seek`] to the track
    /// start. The final block is trimmed to the exact frame for sample accuracy.
    pub fn set_limit(&mut self, frames: u64) {
        self.limit_frames = Some(frames);
        self.frames_emitted = 0;
    }

    /// Seek to `pos` (sample-accurate). Returns the actual landed frame in this
    /// track's timebase (which the engine uses to rebase the per-track position).
    /// Resets decoder state and the sample buffer so the next [`Decoder::next`]
    /// yields fresh audio at the seek point.
    pub fn seek(&mut self, pos: Duration) -> Result<u64> {
        let time = Time::new(pos.as_secs(), f64::from(pos.subsec_nanos()) / 1_000_000_000.0);
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        )?;
        // Decoders may carry inter-frame state (e.g. residuals); reset after a seek.
        self.decoder.reset();
        self.sbuf = None;
        Ok(seeked.actual_ts)
    }

    /// Decode the next block into `out` as interleaved full-scale `i32`.
    /// Returns `false` at end of stream.
    pub fn next(&mut self, out: &mut Vec<i32>) -> Result<bool> {
        // Reached the cue-track end: stop before decoding further.
        if self.limit_frames.is_some_and(|lim| self.frames_emitted >= lim) {
            return Ok(false);
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(false)
                }
                Err(SymError::ResetRequired) => return Ok(false),
                Err(e) => return Err(e.into()),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                // A single corrupt packet is skipped, not fatal.
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            };

            if self.sbuf.is_none() {
                let spec = *decoded.spec();
                let cap = decoded.capacity() as u64;
                self.sbuf = Some(SampleBuffer::new(cap, spec));
            }
            let sbuf = self.sbuf.as_mut().unwrap();
            sbuf.copy_interleaved_ref(decoded);

            out.clear();
            out.extend_from_slice(sbuf.samples());

            // Enforce the cue-track frame limit, trimming the final block exactly.
            if let Some(limit) = self.limit_frames {
                let ch = self.spec.channels.max(1) as usize;
                let frames = (out.len() / ch) as u64;
                let remaining = limit - self.frames_emitted;
                if frames > remaining {
                    out.truncate(remaining as usize * ch);
                    self.frames_emitted = limit;
                } else {
                    self.frames_emitted += frames;
                }
            }
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg_ok() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn gen_wav(path: &Path, secs: f32, rate: u32) {
        let ok = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
            .args(["-ar", &rate.to_string(), "-ac", "2", "-sample_fmt", "s16"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg failed to generate fixture");
    }

    fn count_frames(dec: &mut Decoder) -> u64 {
        let ch = dec.spec.channels.max(1) as u64;
        let mut out = Vec::new();
        let mut frames = 0u64;
        while dec.next(&mut out).unwrap() {
            frames += out.len() as u64 / ch;
        }
        frames
    }

    #[test]
    fn limit_trims_to_exact_frames() {
        if !ffmpeg_ok() {
            eprintln!("ffmpeg not found — skipping decoder limit test");
            return;
        }
        let dir = std::env::temp_dir().join(format!("pc-dec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("t.wav");
        gen_wav(&wav, 2.0, 44_100);

        // Whole file ≈ 2s × 44100 frames.
        let mut dec = Decoder::open(&wav).unwrap();
        assert!(count_frames(&mut dec) >= 80_000, "decodes the whole file");

        // A frame limit stops at exactly that many frames.
        let mut dec = Decoder::open(&wav).unwrap();
        dec.set_limit(44_100);
        assert_eq!(count_frames(&mut dec), 44_100, "limit is sample-exact");

        // seek + limit = a .cue sub-range (from 0.5s, take 0.5s).
        let mut dec = Decoder::open(&wav).unwrap();
        dec.seek(Duration::from_millis(500)).unwrap();
        dec.set_limit(22_050);
        assert_eq!(count_frames(&mut dec), 22_050, "seek + limit is sample-exact");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
