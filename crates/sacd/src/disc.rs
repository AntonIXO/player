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
        FrameReader {
            file,
            base,
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
