// SPDX-License-Identifier: GPL-3.0-or-later
//
// Pure-Rust Super Audio CD reader: Scarletbook (.iso) parsing and DST (Direct
// Stream Transfer) lossless decoding, emitting **native DSD** (this crate never
// converts DSD to PCM).
//
// Ported from the GPLv3 `sacd-ripper` project:
//   Copyright 2011-2019 Maxim V. Anisiutkin <maxim.anisiutkin@gmail.com>
//   Copyright 2015-2019 Robert Tari <robert@tari.in>
// The DST decoder derives from the MPEG-4 Audio reference (ISO/IEC 14496-3) by
// Aad Rijnberg, Fons Bruekers, Eric Knapen, Richard Theelen (Philips).
// See COPYING (GPL-3.0) at the workspace root.

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod disc;
mod dst;
mod error;
mod raw;
mod toc;

pub use error::{Error, Result};

use disc::{Checkpoint, FrameKind, FrameReader};
use toc::{AreaToc, Toc};

/// DSD64 sample rate (64 × 44.1 kHz). SACD audio is always DSD64.
pub const DSD64_RATE: u32 = raw::SACD_SAMPLING_FREQUENCY;

/// A playback area on the disc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// 2-channel area (`TWOCHTOC`).
    Stereo,
    /// Multichannel area (`MULCHTOC`), typically 5.1.
    MultiChannel,
}

/// One track within an area.
#[derive(Clone, Debug, Default)]
pub struct TrackInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub channels: u32,
    pub duration: Duration,
    /// Whether this track's frames are DST-compressed (decoded transparently by
    /// [`SacdTrackReader`]).
    pub dst: bool,
}

/// A decoded area table-of-contents.
#[derive(Clone, Debug)]
pub struct AreaInfo {
    pub area: Area,
    pub channels: u32,
    /// Always [`DSD64_RATE`] for SACD.
    pub dsd_rate: u32,
    pub tracks: Vec<TrackInfo>,
}

/// An opened SACD image: parsed TOC plus enough state to stream any track.
pub struct SacdImage {
    path: PathBuf,
    sector_size: usize,
    base: usize,
    album_title: Option<String>,
    album_artist: Option<String>,
    areas: Vec<AreaInfo>,
    raw: Vec<AreaToc>,
}

impl SacdImage {
    /// Open and parse the TOC of a SACD `.iso` at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let toc = Toc::open(&mut file)?;
        let Toc {
            sector_size,
            base,
            album_title,
            album_artist,
            areas: raw,
        } = toc;

        let areas = raw
            .iter()
            .map(|a| {
                let area = if a.is_stereo {
                    Area::Stereo
                } else {
                    Area::MultiChannel
                };
                let tracks = a
                    .tracks
                    .iter()
                    .map(|t| TrackInfo {
                        title: t.title.clone(),
                        artist: t.artist.clone(),
                        channels: a.channel_count,
                        duration: t.duration,
                        dst: a.frame_format == 0,
                    })
                    .collect();
                AreaInfo {
                    area,
                    channels: a.channel_count,
                    dsd_rate: DSD64_RATE,
                    tracks,
                }
            })
            .collect();

        Ok(Self {
            path: path.to_path_buf(),
            sector_size,
            base,
            album_title,
            album_artist,
            areas,
            raw,
        })
    }

    pub fn album_title(&self) -> Option<&str> {
        self.album_title.as_deref()
    }

    pub fn album_artist(&self) -> Option<&str> {
        self.album_artist.as_deref()
    }

    /// The areas present on the disc.
    pub fn areas(&self) -> &[AreaInfo] {
        &self.areas
    }

    /// Stream a single track of an area as native DSD (DST-decoded if needed).
    pub fn reader(&self, area: Area, track: usize) -> Result<SacdTrackReader> {
        let idx = self
            .raw
            .iter()
            .position(|a| a.is_stereo == matches!(area, Area::Stereo))
            .ok_or_else(|| Error::Unsupported(format!("no {area:?} area on disc")))?;
        let a = &self.raw[idx];
        let count = a.track_start_lsn.len();
        if track >= count {
            return Err(Error::Unsupported(format!(
                "track {track} out of range (area has {count})"
            )));
        }

        // set_track: resolve the track's LSN range (port of sacd_disc set_track).
        let start_lsn = if track > 0 {
            a.track_start_lsn[track]
        } else {
            a.track_start
        };
        let length_lsn = if track + 1 < count {
            a.track_start_lsn[track + 1].saturating_sub(start_lsn) + 1
        } else {
            a.track_end.saturating_sub(start_lsn)
        };

        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start_lsn as u64 * self.sector_size as u64))?;
        let frames = FrameReader::new(file, self.sector_size, self.base, start_lsn, length_lsn);

        Ok(SacdTrackReader {
            frames,
            channels: a.channel_count,
            dst: dst::Decoder::new(a.channel_count as usize),
            is_dst_area: a.frame_format == 0,
            frame_idx: 0,
            index: Vec::new(),
            indexed_through: 0,
        })
    }
}

/// Streams one track's DSD frames. Yields **interleaved, MSB-first** DSD bytes
/// (DoP-ready), transparently DST-decoding compressed frames.
/// Sample one seek checkpoint roughly every second (75 frames = 1 s of audio).
/// At ~32 bytes/entry this is ~115 KB for a 60-minute track — negligible — and
/// bounds the post-jump scan to at most this many frames.
const SEEK_INDEX_INTERVAL: u64 = raw::FRAME_RATE as u64;

pub struct SacdTrackReader {
    frames: FrameReader,
    channels: u32,
    dst: dst::Decoder,
    is_dst_area: bool,
    /// Number of frames yielded so far — the seek anchor. Each frame is a fixed
    /// 1/75 s of audio (588·64/8 = 4704 DSD bytes/channel).
    frame_idx: u64,
    /// Sparse `(frame_idx, resume position)` index, ascending in `frame_idx`,
    /// grown as the track is played or scanned. Lets a backward/replayed seek
    /// jump near the target instead of rescanning from the track start.
    index: Vec<(u64, Checkpoint)>,
    /// Highest `frame_idx` already recorded in `index` (keeps it monotonic and
    /// deduplicated across re-seeks over the same region).
    indexed_through: u64,
}

impl SacdTrackReader {
    /// Append the next block of interleaved MSB-first DSD bytes to `out`.
    /// Returns `false` at end of track.
    pub fn next(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self.frames.read_frame()? {
            Some(FrameKind::Dsd) => {
                out.extend_from_slice(self.frames.frame());
                self.frame_idx += 1;
                self.record_checkpoint();
                Ok(true)
            }
            Some(FrameKind::Dst) => {
                self.dst.decode(self.frames.frame(), out)?;
                self.frame_idx += 1;
                self.record_checkpoint();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Record a seek checkpoint for the current position if it is a fresh, clean
    /// frame boundary at the sampling interval. Called after each frame advance
    /// (during playback and during a seek scan) so the index fills in as the
    /// track is traversed. Cheap and idempotent over re-traversed regions.
    fn record_checkpoint(&mut self) {
        if self.frame_idx < self.indexed_through + SEEK_INDEX_INTERVAL {
            return;
        }
        if let Some(cp) = self.frames.checkpoint() {
            self.index.push((self.frame_idx, cp));
            self.indexed_through = self.frame_idx;
        }
    }

    /// Seek so the next `next()` yields frame `target` (a frame = 1/75 s). DST
    /// frames are self-contained, so skipped frames need no decode. Jumps to the
    /// nearest indexed checkpoint at or before `target` (filled in as the track
    /// is played/scanned) and only scans the bounded remainder, so a backward
    /// seek into already-played audio costs ≤ [`SEEK_INDEX_INTERVAL`] frames
    /// instead of a full rescan from the track start. Returns the landed frame
    /// (clamped to end of track). Bit-exact: it lands on a real frame boundary.
    pub fn seek_frame(&mut self, target: u64) -> Result<u64> {
        // Only reposition when we'd otherwise have to read backward, or when a
        // checkpoint lets us skip a long forward gap. The common case (seeking
        // slightly ahead of the current position) just scans forward as before.
        let nearest = self.nearest_checkpoint(target);
        match nearest {
            // A checkpoint strictly ahead of us, or any checkpoint when we must
            // rewind: jump to it (DST frames are independent → fresh decoder).
            Some((cp_frame, cp)) if cp_frame > self.frame_idx || target < self.frame_idx => {
                self.frames.seek_to_checkpoint(cp)?;
                self.frame_idx = cp_frame;
                self.dst = dst::Decoder::new(self.channels as usize);
            }
            // No usable checkpoint and we must rewind: restart from the top.
            _ if target < self.frame_idx => {
                self.frames.reset()?;
                self.frame_idx = 0;
                self.dst = dst::Decoder::new(self.channels as usize);
            }
            // Forward seek with nothing nearer than where we already are: scan on.
            _ => {}
        }
        while self.frame_idx < target {
            match self.frames.read_frame()? {
                Some(_) => {
                    self.frame_idx += 1;
                    self.record_checkpoint();
                }
                None => break,
            }
        }
        Ok(self.frame_idx)
    }

    /// The indexed checkpoint with the largest `frame_idx <= target`, if any.
    fn nearest_checkpoint(&self, target: u64) -> Option<(u64, Checkpoint)> {
        let i = self.index.partition_point(|(f, _)| *f <= target);
        (i > 0).then(|| self.index[i - 1])
    }

    pub fn dsd_rate(&self) -> u32 {
        DSD64_RATE
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    /// Whether this track is DST-compressed.
    pub fn is_dst(&self) -> bool {
        self.is_dst_area
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disc::test_support::synth_track;

    const SECTOR: usize = 64;
    const PART: usize = 16;
    const PAYLOAD: usize = 2 * PART; // bytes per frame (two sectors per frame)

    fn open(path: &Path, n_frames: usize) -> SacdTrackReader {
        let file = File::open(path).unwrap();
        SacdTrackReader {
            frames: FrameReader::new(file, SECTOR, 0, 0, (2 * n_frames) as u32),
            channels: 2,
            dst: dst::Decoder::new(2),
            is_dst_area: false, // raw DSD: next() copies the payload verbatim
            frame_idx: 0,
            index: Vec::new(),
            indexed_through: 0,
        }
    }

    /// Read every remaining frame from the current position into one buffer.
    fn drain(r: &mut SacdTrackReader) -> Vec<u8> {
        let mut buf = Vec::new();
        while r.next(&mut buf).unwrap() {}
        buf
    }

    #[test]
    fn seek_frame_lands_bit_exact_via_index() {
        // > SEEK_INDEX_INTERVAL frames so the index gets populated (at 75, 150).
        const N: usize = 200;
        let (path, payloads) = synth_track("seek", SECTOR, N, PART);
        let flat: Vec<u8> = payloads.concat();
        assert_eq!(flat.len(), N * PAYLOAD);

        // Play through once to fill the seek index.
        let mut r = open(&path, N);
        assert_eq!(drain(&mut r), flat);
        assert!(!r.index.is_empty(), "playback should populate the seek index");

        // Backward (rewind) and forward seeks to a mix of targets — those with a
        // checkpoint ≤ target jump; the rest fall back to a clean rescan.
        for &target in &[180u64, 100, 150, 30, 0, 120, 199] {
            let landed = r.seek_frame(target).unwrap();
            assert_eq!(landed, target, "seek lands exactly on the target frame");
            assert_eq!(
                drain(&mut r),
                flat[target as usize * PAYLOAD..],
                "tail after seek to {target} must be bit-exact"
            );
        }

        // Seek past the end clamps to the total frame count, no audio past EOF.
        let landed = r.seek_frame(10_000).unwrap();
        assert_eq!(landed, N as u64);
        assert!(drain(&mut r).is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
