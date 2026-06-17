// SPDX-License-Identifier: GPL-3.0-or-later
//! Sector I/O and the audio-frame reader — a port of `sacd_disc.cpp`
//! `read_blocks_raw` and `read_frame`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::raw::{LSN, PSN};

/// A run of `count` logical sectors read from `lb_start`, packed as LSN-size
/// (2048-byte) blocks regardless of on-disc sector size (PSN strips the 12-byte
/// header). Used to pull the master/area TOCs into memory.
pub(crate) fn read_blocks_raw(
    file: &mut File,
    sector_size: usize,
    lb_start: u32,
    count: usize,
) -> Result<Vec<u8>> {
    let mut out = vec![0u8; count * LSN];
    match sector_size {
        LSN => {
            file.seek(SeekFrom::Start(lb_start as u64 * LSN as u64))?;
            file.read_exact(&mut out)?;
        }
        PSN => {
            for i in 0..count {
                file.seek(SeekFrom::Start((lb_start as u64 + i as u64) * PSN as u64 + 12))?;
                file.read_exact(&mut out[i * LSN..(i + 1) * LSN])?;
            }
        }
        other => return Err(Error::Malformed(format!("bad sector size {other}"))),
    }
    Ok(out)
}

/// Whether a completed frame is DST-compressed or raw DSD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Dsd,
    Dst,
}

/// A resumable read position captured at a frame boundary (between frames, with
/// the next frame's start packet not yet consumed). Restoring it reproduces the
/// exact mid-sector parser state, so seeking is bit-identical to having read
/// straight through. Used to build the [`crate::SacdTrackReader`] seek index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Checkpoint {
    /// File byte offset of the sector currently in `FrameReader::sector`.
    sector_pos: u64,
    /// Index of the next (unconsumed) packet within that sector.
    packet_idx: usize,
    /// Read cursor of that packet's payload within the logical-sector payload.
    offset: usize,
}

#[derive(Clone, Copy, Default)]
struct PacketInfo {
    frame_start: bool,
    data_type: u8,
    packet_length: usize,
}

const DATA_TYPE_AUDIO: u8 = 2;

/// Streams audio frames of one track. A frame is assembled from the audio
/// packets of one or more 2048-byte sectors; it is returned complete when the
/// next frame's start packet appears. Mirrors `sacd_disc_t::read_frame`.
pub(crate) struct FrameReader {
    file: File,
    /// Byte offset of the logical-sector payload within a raw sector (0 for LSN,
    /// 12 for PSN).
    base: usize,
    /// First logical sector of the track — the reset/seek anchor.
    start_lsn: u32,
    current_lsn: u32,
    end_lsn: u32,

    sector: Vec<u8>, // one raw sector (sector_size bytes)
    packets: [PacketInfo; 7],
    packet_count: usize,
    packet_idx: usize,
    /// Read cursor within the logical-sector payload.
    offset: usize,
    dst_encoded: bool,

    frame: Vec<u8>,
    frame_started: bool,
    frame_dst: bool,
    done: bool,
}

impl FrameReader {
    pub(crate) fn new(file: File, sector_size: usize, base: usize, start_lsn: u32, length_lsn: u32) -> Self {
        Self {
            file,
            base,
            start_lsn,
            current_lsn: start_lsn,
            end_lsn: start_lsn.saturating_add(length_lsn),
            sector: vec![0u8; sector_size],
            packets: [PacketInfo::default(); 7],
            packet_count: 0,
            // Force a sector load on the first call (packet_idx == packet_count).
            packet_idx: 0,
            offset: 0,
            dst_encoded: false,
            frame: Vec::with_capacity(64 * 1024),
            frame_started: false,
            frame_dst: false,
            done: false,
        }
    }

    /// Parse the 1-byte header + packet/frame info table of the freshly read
    /// sector, leaving `offset` at the first packet's payload.
    fn parse_sector_header(&mut self) {
        let b = self.base;
        let header = self.sector[b];
        // Little-endian bitfield order (as the C struct is memcpy'd on x86):
        // dst_encoded:1, reserved:1, frame_info_count:3, packet_info_count:3.
        self.dst_encoded = header & 1 != 0;
        let frame_info_count = ((header >> 2) & 7) as usize;
        let packet_info_count = ((header >> 5) & 7) as usize;

        let mut off = 1;
        for i in 0..packet_info_count {
            let b0 = self.sector[b + off];
            let b1 = self.sector[b + off + 1];
            self.packets[i] = PacketInfo {
                frame_start: (b0 >> 7) & 1 != 0,
                data_type: (b0 >> 3) & 7,
                packet_length: (((b0 & 7) as usize) << 8) | b1 as usize,
            };
            off += 2;
        }
        // Frame-info table: 4 bytes/frame when DST-encoded, else 3.
        off += if self.dst_encoded { 4 } else { 3 } * frame_info_count;

        self.packet_count = packet_info_count;
        self.packet_idx = 0;
        self.offset = off;
    }

    /// Rewind to the track's first sector and clear all streaming state, so the
    /// next `read_frame` re-reads from the top. Used to seek backward (the caller
    /// then skips forward frame-by-frame to the target).
    pub(crate) fn reset(&mut self) -> Result<()> {
        let sector_size = self.sector.len();
        self.file
            .seek(SeekFrom::Start(self.start_lsn as u64 * sector_size as u64))?;
        self.current_lsn = self.start_lsn;
        self.packet_count = 0;
        self.packet_idx = 0;
        self.offset = 0;
        self.dst_encoded = false;
        self.frame.clear();
        self.frame_started = false;
        self.frame_dst = false;
        self.done = false;
        Ok(())
    }

    /// A resumable position for the *next* frame, or `None` if not at a clean
    /// frame boundary (mid-frame, or at end of track). Valid immediately after
    /// `read_frame` returns a non-final frame: the next frame's start packet is
    /// parsed but not yet consumed.
    pub(crate) fn checkpoint(&self) -> Option<Checkpoint> {
        if self.done || self.frame_started || self.packet_idx >= self.packet_count {
            return None;
        }
        let sector_size = self.sector.len() as u64;
        Some(Checkpoint {
            sector_pos: (self.current_lsn as u64 - 1) * sector_size,
            packet_idx: self.packet_idx,
            offset: self.offset,
        })
    }

    /// Jump to a previously captured [`Checkpoint`] so the next `read_frame`
    /// resumes exactly where that checkpoint was taken (the caller restores its
    /// own frame counter). Re-reads and re-parses the saved sector — which
    /// rebuilds the identical packet table — then restores the mid-sector cursor.
    pub(crate) fn seek_to_checkpoint(&mut self, cp: Checkpoint) -> Result<()> {
        let sector_size = self.sector.len() as u64;
        self.file.seek(SeekFrom::Start(cp.sector_pos))?;
        self.file.read_exact(&mut self.sector)?;
        self.current_lsn = (cp.sector_pos / sector_size) as u32 + 1;
        self.parse_sector_header();
        self.packet_idx = cp.packet_idx;
        self.offset = cp.offset;
        self.frame.clear();
        self.frame_started = false;
        self.frame_dst = false;
        self.done = false;
        Ok(())
    }

    /// Read the next complete frame, or `None` at end of track.
    pub(crate) fn read_frame(&mut self) -> Result<Option<FrameKind>> {
        if self.done {
            return Ok(None);
        }
        let b = self.base;

        while self.current_lsn < self.end_lsn {
            if self.packet_idx == self.packet_count {
                // Load the next sector.
                self.file.read_exact(&mut self.sector)?;
                self.current_lsn += 1;
                self.parse_sector_header();
            }

            while self.packet_idx < self.packet_count {
                let p = self.packets[self.packet_idx];
                if p.data_type == DATA_TYPE_AUDIO {
                    if self.frame_started && p.frame_start {
                        // Completed the previous frame; return it *without*
                        // consuming this start packet (re-entered next call).
                        self.frame_started = false;
                        return Ok(Some(if self.frame_dst {
                            FrameKind::Dst
                        } else {
                            FrameKind::Dsd
                        }));
                    }
                    if !self.frame_started && p.frame_start {
                        self.frame.clear();
                        self.frame_dst = self.dst_encoded;
                        self.frame_started = true;
                    }
                    if self.frame_started {
                        let start = b + self.offset;
                        let end = start + p.packet_length;
                        if end <= b + LSN && end <= self.sector.len() {
                            self.frame.extend_from_slice(&self.sector[start..end]);
                        }
                    }
                }
                self.offset += p.packet_length;
                self.packet_idx += 1;
            }
        }

        // End of track: flush a final in-progress frame.
        self.done = true;
        if self.frame_started {
            self.frame_started = false;
            return Ok(Some(if self.frame_dst {
                FrameKind::Dst
            } else {
                FrameKind::Dsd
            }));
        }
        Ok(None)
    }

    /// The last frame's bytes (valid after `read_frame` returns `Some`).
    pub(crate) fn frame(&self) -> &[u8] {
        &self.frame
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;
    use std::path::PathBuf;

    /// Write a synthetic single-track sector stream: `n_frames` frames, each
    /// spanning **two** sectors (one audio packet per sector; `frame_start` set
    /// only on the first), so `read_frame` yields exactly `n_frames` frames whose
    /// payloads (`2 * part_len` bytes) are the returned vectors. Two sectors per
    /// frame mirrors real discs — the last frame's trailing sector has no
    /// `frame_start`, so the end-of-track flush returns it. `base = 0`
    /// (LSN-style); small `sector_size`/`part_len` keep the fixture tiny while
    /// exercising the same packet/sector machinery.
    pub(crate) fn synth_track(
        tag: &str,
        sector_size: usize,
        n_frames: usize,
        part_len: usize,
    ) -> (PathBuf, Vec<Vec<u8>>) {
        assert!(part_len + 3 <= sector_size, "packet must fit one sector");
        assert!(part_len <= 0x7ff, "packet_length is 11 bits");
        let mut file_bytes = Vec::with_capacity(n_frames * 2 * sector_size);
        let mut payloads = Vec::with_capacity(n_frames);
        for f in 0..n_frames {
            let mut frame = Vec::with_capacity(2 * part_len);
            for half in 0..2 {
                let mut sector = vec![0u8; sector_size];
                // header: dst_encoded=0, frame_info_count=0, packet_info_count=1.
                sector[0] = 1 << 5;
                // packet 0: frame_start on the first half only, data_type=2 (audio).
                let frame_start = u8::from(half == 0);
                sector[1] = (frame_start << 7) | (2 << 3) | (((part_len >> 8) & 7) as u8);
                sector[2] = (part_len & 0xff) as u8;
                let part: Vec<u8> = (0..part_len)
                    .map(|i| {
                        let v = f.wrapping_mul(131).wrapping_add(half * 53).wrapping_add(i.wrapping_mul(17));
                        ((v + 7) & 0xff) as u8
                    })
                    .collect();
                sector[3..3 + part_len].copy_from_slice(&part);
                file_bytes.extend_from_slice(&sector);
                frame.extend_from_slice(&part);
            }
            payloads.push(frame);
        }
        let path = std::env::temp_dir().join(format!("sacd-synth-{tag}-{}.bin", std::process::id()));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file_bytes)
            .unwrap();
        (path, payloads)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::synth_track;
    use super::*;

    const SECTOR: usize = 64;
    const PART: usize = 16; // two parts per frame → 32-byte frames

    fn open_reader(path: &std::path::Path, n_frames: usize) -> FrameReader {
        let file = File::open(path).unwrap();
        FrameReader::new(file, SECTOR, 0, 0, (2 * n_frames) as u32)
    }

    fn read_all_frames(r: &mut FrameReader) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while r.read_frame().unwrap().is_some() {
            out.push(r.frame().to_vec());
        }
        out
    }

    #[test]
    fn synth_yields_expected_frames() {
        let (path, payloads) = synth_track("rf", SECTOR, 6, PART);
        let mut r = open_reader(&path, 6);
        assert_eq!(read_all_frames(&mut r), payloads);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn checkpoint_resumes_bit_exact_at_every_boundary() {
        const N: usize = 12;
        let (path, payloads) = synth_track("cp", SECTOR, N, PART);

        // Read straight through, capturing the checkpoint after each frame.
        let mut r = open_reader(&path, N);
        let mut checkpoints: Vec<(usize, Checkpoint)> = Vec::new();
        let mut produced = 0usize;
        while r.read_frame().unwrap().is_some() {
            produced += 1;
            if let Some(cp) = r.checkpoint() {
                checkpoints.push((produced, cp)); // resume point for frame `produced`
            }
        }
        assert_eq!(produced, N);
        // A clean boundary exists after every frame except the last (end of track).
        assert_eq!(checkpoints.len(), N - 1);

        // Each checkpoint must re-yield the exact tail from that frame onward.
        for (next_frame, cp) in checkpoints {
            let mut r = open_reader(&path, N);
            r.seek_to_checkpoint(cp).unwrap();
            assert_eq!(
                read_all_frames(&mut r),
                payloads[next_frame..],
                "resume at frame {next_frame} must be bit-exact"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
