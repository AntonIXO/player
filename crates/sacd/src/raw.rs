// SPDX-License-Identifier: GPL-3.0-or-later
//! Low-level Scarletbook constants and big-endian field readers. SACD on-disc
//! integers are big-endian; multi-byte fields are read explicitly (no packed
//! structs) to keep the port endian-correct and bounds-checked.

/// Logical sector size (the unit of the TOC and audio addressing).
pub const LSN: usize = 2048;
/// Physical sector size (raw disc reads carry a 12-byte header + 4-byte ECC).
pub const PSN: usize = 2064;
/// Master TOC starts at this logical sector.
pub const START_OF_MASTER_TOC: u64 = 510;
/// Master TOC spans this many sectors (header + 8 text + manufacturer).
pub const MASTER_TOC_LEN: usize = 10;
/// SACD audio is always DSD64.
pub const SACD_SAMPLING_FREQUENCY: u32 = 2_822_400;
/// SACD audio frame rate (frames per second).
pub const FRAME_RATE: u32 = 75;

pub fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

pub fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// True if the 8-byte ASCII id at `off` equals `id`.
pub fn id_is(b: &[u8], off: usize, id: &[u8; 8]) -> bool {
    b.len() >= off + 8 && &b[off..off + 8] == id
}

/// Decode a NUL-terminated on-disc text field at `bytes` using the SACD
/// character-set index. Latin-1/ISO-646 are handled directly; other code pages
/// (Shift-JIS, GB2312, …) fall back to a lossy UTF-8 read for now.
pub fn decode_text(bytes: &[u8], charset: u8) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    let s = &bytes[..end];
    match charset {
        // ISO-646 (ASCII) / ISO-8859-1: map each byte to the same code point.
        0 | 1 | 2 | 7 => s.iter().map(|&c| c as char).collect(),
        // CJK code pages: best-effort UTF-8 (TODO: encoding_rs).
        _ => String::from_utf8_lossy(s).into_owned(),
    }
}
