//! Stream description and the source-depth -> ALSA-format mapping.
//!
//! Bit-perfect philosophy: the decoder (via `SampleBuffer<i32>`) always hands us
//! samples normalized to full 32-bit scale (`native << (32 - bits)`, an exact
//! shift in Symphonia). We pick the ALSA output format from the *source* bit
//! depth and down-shift by `(32 - output_bits)` when packing, which recovers the
//! original native sample exactly. No resampling, no dither, no gain.

use alsa::pcm::Format;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlsaFmt {
    /// 16-bit signed little-endian (2 bytes/sample).
    S16,
    /// 24-bit signed little-endian, packed in 3 bytes (`S24_3LE`).
    S24_3,
    /// 32-bit signed little-endian (4 bytes/sample).
    S32,
}

impl AlsaFmt {
    /// Choose the native output format for a given source bit depth.
    pub fn from_source_bits(bits: u32) -> Self {
        match bits {
            b if b <= 16 => AlsaFmt::S16,
            b if b <= 24 => AlsaFmt::S24_3,
            _ => AlsaFmt::S32,
        }
    }

    pub fn to_alsa(self) -> Format {
        match self {
            AlsaFmt::S16 => Format::S16LE,
            AlsaFmt::S24_3 => Format::S243LE,
            AlsaFmt::S32 => Format::S32LE,
        }
    }

    /// Bytes per single sample (one channel) on the wire.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            AlsaFmt::S16 => 2,
            AlsaFmt::S24_3 => 3,
            AlsaFmt::S32 => 4,
        }
    }

    /// Number of valid output bits; used to derive the down-shift from full scale.
    pub fn output_bits(self) -> u32 {
        match self {
            AlsaFmt::S16 => 16,
            AlsaFmt::S24_3 => 24,
            AlsaFmt::S32 => 32,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AlsaFmt::S16 => "S16_LE",
            AlsaFmt::S24_3 => "S24_3LE",
            AlsaFmt::S32 => "S32_LE",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StreamSpec {
    pub rate: u32,
    pub channels: u32,
    /// Source bit depth as reported by the codec (informational + drives `fmt`).
    pub source_bits: u32,
    pub fmt: AlsaFmt,
}

impl StreamSpec {
    pub fn new(rate: u32, channels: u32, source_bits: u32) -> Self {
        StreamSpec {
            rate,
            channels,
            source_bits,
            fmt: AlsaFmt::from_source_bits(source_bits),
        }
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.fmt.bytes_per_sample() * self.channels as usize
    }
}
