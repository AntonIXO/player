//! DSD → DoP integration test. Generates a tiny synthetic stereo DSD64 `.dsf`
//! with a known pattern, reads it through the real `dsd-reader`-backed source,
//! and proves (headless, no audio hardware):
//!   1. the source yields interleaved **MSB-first** DSD (DSF stores LSB-first,
//!      so each byte must be bit-reversed) in clustered-frame layout, and
//!   2. DoP packing is lossless (decode the DoP stream back to the exact DSD).

use std::io::Write;
use std::path::PathBuf;

use player_core::{open_dsd, AlsaFmt, DopPacker};

const BLOCK: usize = 4096;
const RATE: u32 = 2_822_400;

/// Write a minimal valid stereo DSD64 `.dsf` (one block per channel) with the
/// given planar, LSB-first channel data. Returns the path.
fn write_dsf(ch0: &[u8], ch1: &[u8]) -> PathBuf {
    assert_eq!(ch0.len(), BLOCK);
    assert_eq!(ch1.len(), BLOCK);
    let data_len = ch0.len() + ch1.len();
    let data_chunk = 4 + 8 + data_len as u64;
    let total = 28 + 52 + data_chunk;

    let mut b = Vec::new();
    b.extend_from_slice(b"DSD ");
    b.extend_from_slice(&28u64.to_le_bytes());
    b.extend_from_slice(&total.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // no metadata
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&52u64.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes()); // format version
    b.extend_from_slice(&0u32.to_le_bytes()); // format id (0 = DSD raw)
    b.extend_from_slice(&2u32.to_le_bytes()); // channel type (2 = stereo)
    b.extend_from_slice(&2u32.to_le_bytes()); // channel num
    b.extend_from_slice(&RATE.to_le_bytes()); // sampling frequency
    b.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
    b.extend_from_slice(&((BLOCK * 8) as u64).to_le_bytes()); // sample count / ch
    b.extend_from_slice(&(BLOCK as u32).to_le_bytes()); // block size / ch
    b.extend_from_slice(&0u32.to_le_bytes()); // reserved
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_chunk.to_le_bytes());
    b.extend_from_slice(ch0);
    b.extend_from_slice(ch1);

    let path = std::env::temp_dir().join(format!("pc-dsd-{}.dsf", std::process::id()));
    std::fs::File::create(&path).unwrap().write_all(&b).unwrap();
    path
}

/// Decode a DoP byte stream (S32_LE) back to interleaved DSD — the inverse of
/// `DopPacker` — for the lossless check.
fn unpack_dop_s32(channels: usize, dop: &[u8]) -> Vec<u8> {
    let frame_bytes = 4 * channels;
    let mut dsd = Vec::new();
    for frame in dop.chunks_exact(frame_bytes) {
        let mut olds = Vec::new();
        let mut news = Vec::new();
        for samp in frame.chunks_exact(4) {
            // LE word: [0, new, old, marker]
            olds.push(samp[2]);
            news.push(samp[1]);
        }
        dsd.extend_from_slice(&olds);
        dsd.extend_from_slice(&news);
    }
    dsd
}

#[test]
fn dsf_yields_msb_first_interleaved_and_dop_is_lossless() {
    let ch0: Vec<u8> = (0..BLOCK).map(|i| ((i * 73 + 5) & 0xff) as u8).collect();
    let ch1: Vec<u8> = (0..BLOCK).map(|i| (255 - ((i * 31 + 7) & 0xff)) as u8).collect();
    let path = write_dsf(&ch0, &ch1);

    // Read all DSD bytes the source yields.
    let mut src = open_dsd(&path, None).expect("open .dsf");
    let spec = src.spec();
    assert_eq!(spec.dsd_rate, RATE);
    assert_eq!(spec.channels, 2);

    let mut got = Vec::new();
    let mut buf = Vec::new();
    while src.next(&mut buf).expect("read dsd") {
        got.extend_from_slice(&buf);
    }

    // Expected: each stored (LSB-first) byte bit-reversed to MSB-first, planar
    // interleaved to clustered-frame order [c0b0, c1b0, c0b1, c1b1, …].
    let mut expected = Vec::with_capacity(2 * BLOCK);
    for i in 0..BLOCK {
        expected.push(ch0[i].reverse_bits());
        expected.push(ch1[i].reverse_bits());
    }
    assert_eq!(got, expected, "source must yield MSB-first clustered DSD");

    // DoP packing must be lossless: pack then decode == source DSD.
    let mut dop = DopPacker::new(AlsaFmt::S32, 2);
    let packed = dop.pack(&got).to_vec();
    assert_eq!(packed.len(), got.len() * 2, "S32 DoP = 4 bytes per 2 DSD bytes");
    assert_eq!(unpack_dop_s32(2, &packed), got, "DoP round-trip must be exact");

    let _ = std::fs::remove_file(&path);
}
