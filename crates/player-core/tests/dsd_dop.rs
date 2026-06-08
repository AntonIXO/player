//! DSD → DoP integration test. Generates a tiny synthetic stereo DSD64 `.dsf`
//! with a known pattern, reads it through the real `dsd-reader`-backed source,
//! and proves (headless, no audio hardware):
//!   1. the source yields interleaved **MSB-first** DSD (DSF stores LSB-first,
//!      so each byte must be bit-reversed) in clustered-frame layout, and
//!   2. DoP packing is lossless (decode the DoP stream back to the exact DSD).

use std::io::Write;
use std::path::PathBuf;

use player_core::{open_dsd, AlsaFmt, DopPacker, DsdSource};

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

/// Write a stereo DSD64 `.dsf` of `blocks` blocks per channel from full-length
/// (planar, LSB-first) channel streams (`blocks * BLOCK` bytes each), laid out
/// block-interleaved as DSF stores them: ch0_blk0, ch1_blk0, ch0_blk1, …
fn write_dsf_blocks(ch0: &[u8], ch1: &[u8], blocks: usize) -> PathBuf {
    assert_eq!(ch0.len(), blocks * BLOCK);
    assert_eq!(ch1.len(), blocks * BLOCK);
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
    b.extend_from_slice(&((blocks * BLOCK * 8) as u64).to_le_bytes()); // samples / ch
    b.extend_from_slice(&(BLOCK as u32).to_le_bytes()); // block size / ch
    b.extend_from_slice(&0u32.to_le_bytes()); // reserved
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_chunk.to_le_bytes());
    for blk in 0..blocks {
        let r = blk * BLOCK..(blk + 1) * BLOCK;
        b.extend_from_slice(&ch0[r.clone()]);
        b.extend_from_slice(&ch1[r]);
    }

    let path = std::env::temp_dir().join(format!("pc-dsd-seek-{}.dsf", std::process::id()));
    std::fs::File::create(&path).unwrap().write_all(&b).unwrap();
    path
}

/// Read every DSD byte a source yields, from its current position.
fn read_all(src: &mut dyn DsdSource) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = Vec::new();
    while src.next(&mut buf).expect("read dsd") {
        got.extend_from_slice(&buf);
    }
    got
}

#[test]
fn dsf_seek_lands_bit_exact_on_block_boundaries() {
    const BLOCKS: usize = 5;
    let ch0: Vec<u8> = (0..BLOCKS * BLOCK).map(|i| ((i * 73 + 5) & 0xff) as u8).collect();
    let ch1: Vec<u8> = (0..BLOCKS * BLOCK)
        .map(|i| (255 - ((i * 31 + 7) & 0xff)) as u8)
        .collect();
    let path = write_dsf_blocks(&ch0, &ch1, BLOCKS);

    // Full reference stream.
    let reference = read_all(&mut *open_dsd(&path, None).expect("open .dsf"));
    // One block = BLOCK bytes/channel = 2*BLOCK interleaved DSD bytes = BLOCK/2
    // DoP frames (2 DSD bytes/channel per DoP frame).
    let frames_per_block = (BLOCK / 2) as u64;
    assert_eq!(reference.len(), BLOCKS * 2 * BLOCK);

    // Forward seek on a fresh source, to each block boundary.
    for k in 0..BLOCKS as u64 {
        let mut src = open_dsd(&path, None).expect("open .dsf");
        let landed = src
            .seek(k * frames_per_block)
            .expect("seek")
            .expect("dsf seek is supported");
        assert_eq!(landed, k * frames_per_block, "landed on the requested frame");
        let tail = read_all(&mut *src);
        assert_eq!(
            tail,
            reference[k as usize * 2 * BLOCK..],
            "forward seek to block {k} yields the exact tail"
        );
    }

    // Backward seek: play to the end, then rewind — exercises the recreate path.
    let mut src = open_dsd(&path, None).expect("open .dsf");
    let _ = read_all(&mut *src);
    let landed = src.seek(frames_per_block).expect("seek").expect("supported");
    assert_eq!(landed, frames_per_block);
    assert_eq!(
        read_all(&mut *src),
        reference[2 * BLOCK..],
        "backward seek re-yields the exact tail"
    );

    // Seek past the end clamps to the end (empty tail).
    let mut src = open_dsd(&path, None).expect("open .dsf");
    let landed = src.seek(1_000_000).expect("seek").expect("supported");
    assert_eq!(landed, BLOCKS as u64 * frames_per_block);
    assert!(read_all(&mut *src).is_empty(), "no audio past end of stream");

    let _ = std::fs::remove_file(&path);
}
