//! Pack full-scale interleaved `i32` samples into the chosen ALSA byte/sample
//! format. Pure integer shifts — no floating point, no dither for lossless.
//!
//! Input invariant: `full` holds interleaved samples normalized to full 32-bit
//! scale (what `SampleBuffer<i32>` produces). For an N-bit output we emit
//! `full >> (32 - N)`, which for a source whose native depth equals N recovers
//! the original sample bit-for-bit.

use crate::format::AlsaFmt;

/// A view of one decoded block, already packed for the target format.
pub enum OutFrames<'a> {
    S16(&'a [i16]),
    /// `S24_3LE`: 3 bytes per sample, little-endian.
    S24(&'a [u8]),
    S32(&'a [i32]),
}

/// Reusable packer holding scratch buffers so the hot loop never allocates.
pub struct Packer {
    fmt: AlsaFmt,
    s16: Vec<i16>,
    s24: Vec<u8>,
}

impl Packer {
    pub fn new(fmt: AlsaFmt) -> Self {
        Self {
            fmt,
            s16: Vec::new(),
            s24: Vec::new(),
        }
    }

    /// Pack `full` (interleaved, full-scale i32) for the target format.
    pub fn pack<'a>(&'a mut self, full: &'a [i32]) -> OutFrames<'a> {
        match self.fmt {
            AlsaFmt::S16 => {
                self.s16.clear();
                // Optimization: extend from an iterator eliminates bounds checks on every push,
                // letting LLVM auto-vectorize the downshift loop cleanly.
                self.s16.extend(full.iter().map(|&s| (s >> 16) as i16));
                OutFrames::S16(&self.s16)
            }
            AlsaFmt::S24_3 => {
                self.s24.clear();
                // Optimization: extend from an iterator eliminates bounds checks on every push
                // and helps LLVM optimize better.
                self.s24.reserve(full.len() * 3);
                self.s24.extend(full.iter().flat_map(|&s| {
                    let v = s >> 8; // full-scale i32 -> native 24-bit value
                    [v as u8, (v >> 8) as u8, (v >> 16) as u8]
                }));
                OutFrames::S24(&self.s24)
            }
            // S32: the input already is the wire format; no copy needed.
            AlsaFmt::S32 => OutFrames::S32(full),
        }
    }
}

/// Append a packed block to a persistent byte buffer (used by the loopback
/// harness and the `dump` path). Layout is identical to what the ALSA sink writes.
pub fn append_bytes(out: &mut Vec<u8>, frames: &OutFrames) {
    match frames {
        OutFrames::S16(b) => {
            for &s in *b {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        OutFrames::S24(b) => out.extend_from_slice(b),
        OutFrames::S32(b) => {
            for &s in *b {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Full-scale i32 values that correspond to known native samples after the
    // documented down-shift.
    #[test]
    fn s16_downshift_recovers_native() {
        // native 16-bit 0x1234 and -1 (0xFFFF) left-justified to full scale.
        let full = [(0x1234i32) << 16, (-1i32) << 16];
        let mut p = Packer::new(AlsaFmt::S16);
        match p.pack(&full) {
            OutFrames::S16(s) => assert_eq!(s, &[0x1234i16, -1i16]),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn s24_packs_low_three_bytes_le() {
        // native 24-bit value 0x7FABCD and -1 (0xFFFFFF) at full scale.
        let full = [(0x7FABCDi32) << 8, (-1i32) << 8];
        let mut p = Packer::new(AlsaFmt::S24_3);
        match p.pack(&full) {
            OutFrames::S24(b) => {
                assert_eq!(b, &[0xCD, 0xAB, 0x7F, 0xFF, 0xFF, 0xFF]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn s32_is_passthrough() {
        let full = [0x0BAD_F00Du32 as i32, -42];
        let mut p = Packer::new(AlsaFmt::S32);
        match p.pack(&full) {
            OutFrames::S32(s) => assert_eq!(s, &full),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn append_bytes_s32_is_little_endian() {
        let full = [1i32, -1];
        let mut p = Packer::new(AlsaFmt::S32);
        let out = p.pack(&full);
        let mut bytes = Vec::new();
        append_bytes(&mut bytes, &out);
        assert_eq!(bytes, &[1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    }
}
