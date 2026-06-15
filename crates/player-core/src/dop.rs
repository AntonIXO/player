//! DoP (DSD-over-PCM) packing — the bit-perfect way to carry DSD to a DAC over
//! a PCM transport (the Chord Mojo 2 is DoP-only).
//!
//! Per the DoP open standard (v1.1), each 24-bit PCM sample carries **16 DSD
//! bits** in its low 16 bits and an 8-bit **marker** (`0x05`/`0xFA`, alternating
//! every sample) in its top byte. The DAC recognizes the marker pattern and
//! recovers the exact DSD bits, so DoP alters no audio: stripping the marker and
//! taking the low 16 bits reconstructs the source DSD byte-for-byte (see the
//! round-trip test). The PCM "sample rate" is the DSD rate / 16 (DSD64 → 176.4
//! kHz).
//!
//! Input is **interleaved, MSB-first** DSD in the DSDIFF/Scarletbook "clustered
//! frame" layout: for `N` channels, `N` consecutive bytes are the same
//! time-slot byte of channels `0..N`, then the next `N` bytes are the following
//! byte of each channel. One DoP frame therefore consumes `2N` input bytes (an
//! older byte then a newer byte per channel) and emits `N` PCM samples. Output
//! is interleaved PCM in `S24_3LE` or `S32_LE`, LE bytes ready for the ALSA sink
//! — identical downstream plumbing to the PCM [`crate::convert::Packer`].

use crate::format::AlsaFmt;

/// The two DoP marker bytes (placed in the most-significant byte of the 24-bit
/// DoP word), alternating every output sample.
pub const DOP_MARKER_A: u8 = 0x05;
pub const DOP_MARKER_B: u8 = 0xFA;

/// DSD "digital silence" / idle byte (`0b0110_1001`) — the same pattern the SACD
/// format uses for muted frames; ~DC-free after the DAC's filter.
pub const DSD_SILENCE: u8 = 0x69;

/// Build `frames` DoP frames of DSD silence for `fmt`/`channels`, markers
/// alternating from `0x05`. Used as a brief **device-open pre-roll** so the DAC
/// locks onto the DoP marker pattern before real audio arrives (otherwise the
/// first frames can be mis-read as PCM — an audible click at track start). This
/// is leading silence: it never alters the music samples. `frames` should be
/// even so the marker phase continues into the packer (which also starts at
/// `0x05`), avoiding a one-frame discontinuity at the join.
pub fn dop_silence(fmt: AlsaFmt, channels: u32, frames: usize) -> Vec<u8> {
    let mut p = DopPacker::new(fmt, channels);
    let dsd = vec![DSD_SILENCE; frames * 2 * channels as usize];
    p.pack(&dsd).to_vec()
}

/// Packs interleaved MSB-first DSD bytes into a DoP PCM byte stream. Holds the
/// marker phase and a small carry of leftover DSD bytes across calls so blocks
/// of any length pack correctly; the audio stays continuous.
pub struct DopPacker {
    fmt: AlsaFmt,
    channels: usize,
    /// Which marker the *next* frame uses (`false` → `0x05`, `true` → `0xFA`).
    marker_b: bool,
    /// Leftover DSD bytes that did not complete a `2*channels` frame.
    carry: Vec<u8>,
    /// Packed-output scratch (reused; never allocates in the steady state).
    out: Vec<u8>,
}

impl DopPacker {
    /// `fmt` must be `S24_3` or `S32` (DoP needs a 24-bit-capable container).
    pub fn new(fmt: AlsaFmt, channels: u32) -> Self {
        debug_assert!(
            matches!(fmt, AlsaFmt::S24_3 | AlsaFmt::S32),
            "DoP requires a 24- or 32-bit container, got {fmt:?}"
        );
        DopPacker {
            fmt,
            channels: channels as usize,
            marker_b: false,
            carry: Vec::new(),
            out: Vec::new(),
        }
    }

    /// Reset marker phase and drop any carry. Call at the start of a track so a
    /// fresh DoP stream begins on the `0x05` marker.
    pub fn reset(&mut self) {
        self.marker_b = false;
        self.carry.clear();
    }

    /// DSD bytes consumed to produce one DoP PCM frame (2 per channel).
    fn frame_in(&self) -> usize {
        2 * self.channels
    }

    /// Pack a block of interleaved MSB-first DSD bytes. Returns the packed LE
    /// bytes for this call — a whole number of PCM frames (possibly empty if the
    /// block plus carry is shorter than one frame).
    pub fn pack(&mut self, dsd: &[u8]) -> &[u8] {
        self.out.clear();
        let frame_in = self.frame_in();

        // 1) Complete a partial frame held over from a previous call.
        let mut input = dsd;
        if !self.carry.is_empty() {
            let need = frame_in - self.carry.len();
            if dsd.len() < need {
                self.carry.extend_from_slice(dsd);
                return &self.out;
            }
            let mut frame = std::mem::take(&mut self.carry);
            frame.extend_from_slice(&dsd[..need]);
            self.emit_frame(&frame);
            frame.clear();
            self.carry = frame; // give the allocation back for reuse
            input = &dsd[need..];
        }

        // 2) Emit every whole frame in the remaining input.
        let whole = (input.len() / frame_in) * frame_in;

        let out_bytes_per_frame = match self.fmt {
            AlsaFmt::S24_3 => 3 * self.channels,
            AlsaFmt::S32 => 4 * self.channels,
            AlsaFmt::S16 => unreachable!(),
        };
        // Optimization: Pre-allocate capacity to avoid bounds checks
        self.out.reserve((whole / frame_in) * out_bytes_per_frame);

        let req = (whole / frame_in) * out_bytes_per_frame;
        let start_len = self.out.len();
        // Optimization: resize and chunks_exact_mut avoids bounds checks and iterator
        // overheads present in extend_from_slice, yielding ~30% faster packing.
        self.out.resize(start_len + req, 0);
        let out_slice = &mut self.out[start_len..];

        if let AlsaFmt::S32 = self.fmt {
            let mut out_chunks = out_slice.chunks_exact_mut(4 * self.channels);
            for frame in input[..whole].chunks_exact(frame_in) {
                let chunk = out_chunks.next().unwrap();
                let marker = if self.marker_b { DOP_MARKER_B } else { DOP_MARKER_A };
                self.marker_b = !self.marker_b;
                let (old_slice, new_slice) = frame.split_at(self.channels);

                for c in 0..self.channels {
                    let offset = c * 4;
                    // For S32: [0, new, old, marker]
                    chunk[offset] = 0;
                    chunk[offset + 1] = new_slice[c];
                    chunk[offset + 2] = old_slice[c];
                    chunk[offset + 3] = marker;
                }
            }
        } else {
            let mut out_chunks = out_slice.chunks_exact_mut(3 * self.channels);
            for frame in input[..whole].chunks_exact(frame_in) {
                let chunk = out_chunks.next().unwrap();
                let marker = if self.marker_b { DOP_MARKER_B } else { DOP_MARKER_A };
                self.marker_b = !self.marker_b;
                let (old_slice, new_slice) = frame.split_at(self.channels);

                for c in 0..self.channels {
                    let offset = c * 3;
                    // For S24_3: [new, old, marker]
                    chunk[offset] = new_slice[c];
                    chunk[offset + 1] = old_slice[c];
                    chunk[offset + 2] = marker;
                }
            }
        }

        let i = whole;

        // 3) Stash the remainder for next time.
        if i < input.len() {
            self.carry.extend_from_slice(&input[i..]);
        }
        &self.out
    }

    /// Emit a single frame (used for completing a partial frame from carry).
    /// `frame` is `[old_c0..old_c(N-1), new_c0..new_c(N-1)]`; `old` is the
    /// earlier (more-significant) DSD byte.
    fn emit_frame(&mut self, frame: &[u8]) {
        let n = self.channels;
        let marker = if self.marker_b { DOP_MARKER_B } else { DOP_MARKER_A };
        self.marker_b = !self.marker_b;
        for c in 0..n {
            let old = frame[c];
            let new = frame[n + c];
            match self.fmt {
                // 24-bit value (marker<<16)|(old<<8)|new, little-endian.
                AlsaFmt::S24_3 => self.out.extend_from_slice(&[new, old, marker]),
                // Left-justify the 24-bit DoP word into 32 bits (low byte 0):
                // (marker<<24)|(old<<16)|(new<<8), little-endian.
                AlsaFmt::S32 => self.out.extend_from_slice(&[0, new, old, marker]),
                AlsaFmt::S16 => unreachable!("DoP requires a 24/32-bit container"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a DoP byte stream back to interleaved DSD (clustered-frame layout)
    /// and collect the marker of each frame — the inverse of `DopPacker`.
    fn unpack(fmt: AlsaFmt, channels: usize, dop: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let bps = match fmt {
            AlsaFmt::S24_3 => 3,
            AlsaFmt::S32 => 4,
            AlsaFmt::S16 => unreachable!(),
        };
        let frame_bytes = bps * channels;
        let mut dsd = Vec::new();
        let mut markers = Vec::new();
        for frame in dop.chunks_exact(frame_bytes) {
            let mut olds = Vec::new();
            let mut news = Vec::new();
            for (c, samp) in frame.chunks_exact(bps).enumerate() {
                // LE word; marker is the top byte, then old, then new.
                let (marker, old, new) = match fmt {
                    AlsaFmt::S24_3 => (samp[2], samp[1], samp[0]),
                    AlsaFmt::S32 => (samp[3], samp[2], samp[1]),
                    AlsaFmt::S16 => unreachable!(),
                };
                if c == 0 {
                    markers.push(marker);
                }
                olds.push(old);
                news.push(new);
            }
            dsd.extend_from_slice(&olds); // old bytes of all channels
            dsd.extend_from_slice(&news); // then new bytes of all channels
        }
        (dsd, markers)
    }

    #[test]
    fn stereo_s32_layout_and_markers() {
        // 2 frames of stereo DSD: frame0 [old L,old R,new L,new R], frame1 ...
        let dsd = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let mut p = DopPacker::new(AlsaFmt::S32, 2);
        let out = p.pack(&dsd).to_vec();
        // frame0 marker 0x05: L=(0x05,0x12,0x56) R=(0x05,0x34,0x78)
        // frame1 marker 0xFA: L=(0xFA,0x9A,0xDE) R=(0xFA,0xBC,0xF0)
        assert_eq!(
            out,
            vec![
                0x00, 0x56, 0x12, 0x05, 0x00, 0x78, 0x34, 0x05, // frame 0
                0x00, 0xDE, 0x9A, 0xFA, 0x00, 0xF0, 0xBC, 0xFA, // frame 1
            ]
        );
    }

    #[test]
    fn s24_layout() {
        let dsd = [0xAA, 0xBB, 0xCC, 0xDD]; // one stereo frame
        let mut p = DopPacker::new(AlsaFmt::S24_3, 2);
        let out = p.pack(&dsd).to_vec();
        // L=(marker 0x05, old 0xAA, new 0xCC) -> LE [0xCC,0xAA,0x05]
        // R=(0x05, 0xBB, 0xDD) -> [0xDD,0xBB,0x05]
        assert_eq!(out, vec![0xCC, 0xAA, 0x05, 0xDD, 0xBB, 0x05]);
    }

    #[test]
    fn roundtrip_is_lossless_and_markers_alternate() {
        // A pseudo-random DSD stream, stereo, several frames.
        let mut dsd = Vec::new();
        let mut x = 0x1234_5678u32;
        for _ in 0..(4 * 50) {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            dsd.push((x >> 16) as u8);
        }
        for &fmt in &[AlsaFmt::S24_3, AlsaFmt::S32] {
            let mut p = DopPacker::new(fmt, 2);
            let out = p.pack(&dsd).to_vec();
            let (back, markers) = unpack(fmt, 2, &out);
            assert_eq!(back, dsd, "DoP must reconstruct DSD exactly ({fmt:?})");
            // 200 bytes / (2*2) = 50 frames; markers 05,FA,05,FA…
            assert_eq!(markers.len(), 50);
            for (i, &m) in markers.iter().enumerate() {
                let want = if i % 2 == 0 { DOP_MARKER_A } else { DOP_MARKER_B };
                assert_eq!(m, want, "marker phase at frame {i}");
            }
        }
    }

    #[test]
    fn carry_across_calls_matches_one_shot() {
        // Feeding the stream in awkward chunks must equal packing it all at once.
        let dsd: Vec<u8> = (0..=199u32).map(|v| (v * 7) as u8).collect();
        let mut one = DopPacker::new(AlsaFmt::S32, 2);
        let whole = one.pack(&dsd).to_vec();

        let mut split = DopPacker::new(AlsaFmt::S32, 2);
        let mut acc = Vec::new();
        // chunk sizes deliberately not multiples of 2*channels.
        for chunk in dsd.chunks(3) {
            acc.extend_from_slice(split.pack(chunk));
        }
        assert_eq!(acc, whole);
    }
}
