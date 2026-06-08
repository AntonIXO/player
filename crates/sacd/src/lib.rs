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

use disc::{FrameKind, FrameReader};
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

        Ok(SacdImage {
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
        })
    }
}

/// Streams one track's DSD frames. Yields **interleaved, MSB-first** DSD bytes
/// (DoP-ready), transparently DST-decoding compressed frames.
pub struct SacdTrackReader {
    frames: FrameReader,
    channels: u32,
    dst: dst::Decoder,
    is_dst_area: bool,
    /// Number of frames yielded so far — the seek anchor. Each frame is a fixed
    /// 1/75 s of audio (588·64/8 = 4704 DSD bytes/channel).
    frame_idx: u64,
}

impl SacdTrackReader {
    /// Append the next block of interleaved MSB-first DSD bytes to `out`.
    /// Returns `false` at end of track.
    pub fn next(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self.frames.read_frame()? {
            Some(FrameKind::Dsd) => {
                out.extend_from_slice(self.frames.frame());
                self.frame_idx += 1;
                Ok(true)
            }
            Some(FrameKind::Dst) => {
                self.dst.decode(self.frames.frame(), out)?;
                self.frame_idx += 1;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Seek so the next `next()` yields frame `target` (a frame = 1/75 s). DST
    /// frames are self-contained, so skipped frames need no decode; a backward
    /// seek rewinds to the track start and re-skips. Returns the landed frame
    /// (clamped to end of track). Bit-exact: it lands on a real frame boundary.
    pub fn seek_frame(&mut self, target: u64) -> Result<u64> {
        if target < self.frame_idx {
            self.frames.reset()?;
            self.frame_idx = 0;
            // DST frames are independent; a fresh decoder guarantees clean state.
            self.dst = dst::Decoder::new(self.channels as usize);
        }
        while self.frame_idx < target {
            match self.frames.read_frame()? {
                Some(_) => self.frame_idx += 1,
                None => break,
            }
        }
        Ok(self.frame_idx)
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
