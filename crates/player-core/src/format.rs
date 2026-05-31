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

    /// True if two specs share the same *wire* parameters (rate, channels, ALSA
    /// format) and can therefore stream through one open device gaplessly.
    /// `source_bits` is deliberately ignored: e.g. 20- and 24-bit both emit
    /// `S24_3LE`, so they are gapless-compatible.
    pub fn same_wire(&self, other: &StreamSpec) -> bool {
        self.rate == other.rate && self.channels == other.channels && self.fmt == other.fmt
    }
}

/// Which PCM sample formats a device accepts (probed once per device). Used to
/// pick a bit-perfect output format: the narrowest the device supports that is
/// no narrower than the source. Widening only zero-pads the low bits (e.g. a
/// 16-bit sample carried as `S32_LE`), which is lossless — no dither, no
/// scaling, no resampling. Needed because some DACs (e.g. the Chord Mojo 2) are
/// `S32_LE`-only and reject the source's native `S16_LE`/`S24_3LE`.
#[derive(Clone, Copy, Debug)]
pub struct DeviceFormats {
    pub s16: bool,
    pub s24_3: bool,
    pub s32: bool,
}

impl DeviceFormats {
    /// Assume everything is supported. Used as a fallback when probing fails so
    /// behaviour matches the source-native choice and the real error (if any)
    /// surfaces at device open.
    pub fn all() -> Self {
        DeviceFormats {
            s16: true,
            s24_3: true,
            s32: true,
        }
    }

    pub fn supports(&self, f: AlsaFmt) -> bool {
        match f {
            AlsaFmt::S16 => self.s16,
            AlsaFmt::S24_3 => self.s24_3,
            AlsaFmt::S32 => self.s32,
        }
    }

    /// The narrowest supported format whose width is `>=` the source's native
    /// width (so the original samples fit exactly, zero-padded). `None` if the
    /// device supports nothing wide enough.
    pub fn choose(&self, source_bits: u32) -> Option<AlsaFmt> {
        let want = AlsaFmt::from_source_bits(source_bits).output_bits();
        [AlsaFmt::S16, AlsaFmt::S24_3, AlsaFmt::S32]
            .into_iter()
            .find(|f| f.output_bits() >= want && self.supports(*f))
    }
}
