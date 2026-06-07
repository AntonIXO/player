// SPDX-License-Identifier: GPL-3.0-or-later
//! Parse the Scarletbook master + area TOCs — a port of `read_master_toc` /
//! `read_area_toc`. All offsets follow the `#pragma pack(1)` layout in
//! `scarletbook.h`; multi-byte integers are big-endian.

use std::fs::File;
use std::time::Duration;

use crate::disc::read_blocks_raw;
use crate::error::{Error, Result};
use crate::raw::{
    be16, be32, decode_text, id_is, FRAME_RATE, LSN, MASTER_TOC_LEN, START_OF_MASTER_TOC,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct TrackText {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct AreaToc {
    pub is_stereo: bool,
    pub channel_count: u32,
    /// 0 = DST-compressed, 2/3 = raw DSD.
    pub frame_format: u8,
    pub track_start: u32,
    pub track_end: u32,
    /// Per-track start LSN (from `SACDTRL1`), length = track count.
    pub track_start_lsn: Vec<u32>,
    pub tracks: Vec<TrackText>,
}

pub(crate) struct Toc {
    pub sector_size: usize,
    pub base: usize,
    pub album_title: Option<String>,
    pub album_artist: Option<String>,
    pub areas: Vec<AreaToc>,
}

impl Toc {
    /// Detect the on-disc sector size, then parse the master and area TOCs.
    pub(crate) fn open(file: &mut File) -> Result<Toc> {
        let (sector_size, base) = detect_sector_size(file)?;

        let master = read_blocks_raw(file, sector_size, START_OF_MASTER_TOC as u32, MASTER_TOC_LEN)?;
        if !id_is(&master, 0, b"SACDMTOC") {
            return Err(Error::NotSacd("missing SACDMTOC".into()));
        }
        if master[8] > 1 || master[9] > 20 {
            return Err(Error::Unsupported(format!(
                "Scarletbook version {}.{:02x}",
                master[8], master[9]
            )));
        }

        let area_1_start = be32(&master, 64);
        let area_1_size = be16(&master, 84) as usize;
        let area_2_start = be32(&master, 72);
        let area_2_size = be16(&master, 86) as usize;

        // Master album text (first SACDText sector, one logical sector in).
        let (album_title, album_artist) = parse_master_text(&master);

        let mut areas = Vec::new();
        for (start, size) in [(area_1_start, area_1_size), (area_2_start, area_2_size)] {
            if start == 0 || size == 0 {
                continue;
            }
            let data = read_blocks_raw(file, sector_size, start, size)?;
            if let Some(area) = parse_area_toc(&data) {
                areas.push(area);
            }
        }
        if areas.is_empty() {
            return Err(Error::Malformed("no readable area TOC".into()));
        }

        Ok(Toc {
            sector_size,
            base,
            album_title,
            album_artist,
            areas,
        })
    }
}

/// Probe for `SACDMTOC` at the LSN and PSN master-TOC positions.
fn detect_sector_size(file: &mut File) -> Result<(usize, usize)> {
    use crate::raw::PSN;
    use std::io::{Read, Seek, SeekFrom};

    let mut id = [0u8; 8];
    file.seek(SeekFrom::Start(START_OF_MASTER_TOC * LSN as u64))?;
    if file.read_exact(&mut id).is_ok() && &id == b"SACDMTOC" {
        return Ok((LSN, 0));
    }
    file.seek(SeekFrom::Start(START_OF_MASTER_TOC * PSN as u64 + 12))?;
    if file.read_exact(&mut id).is_ok() && &id == b"SACDMTOC" {
        return Ok((PSN, 12));
    }
    Err(Error::NotSacd("no SACDMTOC at sector 510 (LSN or PSN)".into()))
}

fn parse_master_text(master: &[u8]) -> (Option<String>, Option<String>) {
    let charset = master.get(138).copied().unwrap_or(0) & 7;
    let text = &master[LSN..]; // first SACDText sector
    if !id_is(text, 0, b"SACDText") {
        return (None, None);
    }
    let title_pos = be16(text, 16) as usize;
    let artist_pos = be16(text, 18) as usize;
    let pick = |pos: usize| -> Option<String> {
        if pos == 0 || pos >= text.len() {
            return None;
        }
        let s = decode_text(&text[pos..], charset);
        (!s.is_empty()).then_some(s)
    };
    (pick(title_pos), pick(artist_pos))
}

fn parse_area_toc(data: &[u8]) -> Option<AreaToc> {
    if !id_is(data, 0, b"TWOCHTOC") && !id_is(data, 0, b"MULCHTOC") {
        return None;
    }
    if data[8] > 1 || data[9] > 20 {
        return None;
    }
    let size = be16(data, 10) as usize;
    let frame_format = data[21] & 0x0F;
    let channel_count = data[32] as u32;
    let loudspeaker_config = data[33] & 0x1F;
    let track_count = data[69] as usize;
    let track_start = be32(data, 72);
    let track_end = be32(data, 76);
    let is_stereo = channel_count == 2 && loudspeaker_config == 0;
    let charset = data.get(90).copied().unwrap_or(0) & 7; // languages[0].character_set

    let mut track_start_lsn = vec![0u32; track_count];
    let mut tracks = vec![TrackText::default(); track_count];

    // Scan the area TOC sectors for the named sub-tables.
    let mut p = LSN;
    let limit = (size * LSN).min(data.len());
    while p + 8 <= limit {
        if id_is(data, p, b"SACDTTxt") {
            parse_track_text(&data[p..], charset, &mut tracks);
            p += LSN;
        } else if id_is(data, p, b"SACD_IGL") {
            p += LSN * 2;
        } else if id_is(data, p, b"SACD_ACC") {
            p += LSN * 32;
        } else if id_is(data, p, b"SACDTRL1") {
            for (i, lsn) in track_start_lsn.iter_mut().enumerate() {
                *lsn = be32(data, p + 8 + i * 4);
            }
            p += LSN;
        } else if id_is(data, p, b"SACDTRL2") {
            parse_durations(&data[p..], &mut tracks);
            p += LSN;
        } else {
            break;
        }
    }

    Some(AreaToc {
        is_stereo,
        channel_count,
        frame_format,
        track_start,
        track_end,
        track_start_lsn,
        tracks,
    })
}

/// Parse the `SACDTTxt` per-track title/performer table. `s` starts at the
/// sector's `SACDTTxt` id.
fn parse_track_text(s: &[u8], charset: u8, tracks: &mut [TrackText]) {
    const TYPE_TITLE: u8 = 0x01;
    const TYPE_PERFORMER: u8 = 0x02;

    for (i, track) in tracks.iter_mut().enumerate() {
        let pos = be16(s, 8 + i * 2) as usize;
        if pos == 0 || pos >= s.len() {
            continue;
        }
        let mut p = pos;
        let amount = s[p] as usize;
        p += 4;
        for j in 0..amount {
            if p >= s.len() {
                break;
            }
            let track_type = s[p];
            p += 2; // type byte + an unknown 0x20
            if p < s.len() && s[p] != 0 {
                let text = decode_text(&s[p..], charset);
                match track_type {
                    TYPE_TITLE => track.title = Some(text),
                    TYPE_PERFORMER => track.artist = Some(text),
                    _ => {}
                }
            }
            // Advance to the next entry: skip the string, then the NUL run.
            if j + 1 < amount {
                while p < s.len() && s[p] != 0 {
                    p += 1;
                }
                while p < s.len() && s[p] == 0 {
                    p += 1;
                }
            }
        }
    }
}

/// Parse the `SACDTRL2` per-track durations (`duration[i]` is min:sec:frames at
/// 75 fps). `s` starts at the sector's `SACDTRL2` id.
fn parse_durations(s: &[u8], tracks: &mut [TrackText]) {
    let dur_base = 8 + 255 * 4; // after id + start[255]
    for (i, track) in tracks.iter_mut().enumerate() {
        let o = dur_base + i * 4;
        if o + 3 > s.len() {
            break;
        }
        let (m, sec, fr) = (s[o] as u64, s[o + 1] as u64, s[o + 2] as u64);
        let millis = (m * 60 + sec) * 1000 + fr * 1000 / FRAME_RATE as u64;
        track.duration = Duration::from_millis(millis);
    }
}
