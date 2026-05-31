//! Symphonia decode wrapper. Produces interleaved full-scale `i32` blocks.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder as SymDecoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

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
        })
    }

    /// Decode the next block into `out` as interleaved full-scale `i32`.
    /// Returns `false` at end of stream.
    pub fn next(&mut self, out: &mut Vec<i32>) -> Result<bool> {
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
            return Ok(true);
        }
    }
}
