//! Tag + cover-art + header extraction via lofty. One file open per track.
//! SACD `.iso` images are handled separately (via the `sacd` crate) — see
//! [`is_sacd_iso`] / [`extract_sacd`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use id3::TagLike;
use lofty::file::FileType;
use lofty::prelude::*; // AudioFile, TaggedFileExt, Accessor, ItemKey
use lofty::read_from_path;

/// Everything we pull out of one audio file.
pub struct Extracted {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub duration_ms: Option<u64>,
    pub codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub bits: Option<u32>,
    pub channels: Option<u32>,
    /// `(blake3 hex, mime, bytes)` of the first embedded cover, if any.
    pub art: Option<(String, String, Vec<u8>)>,
}

pub fn extract(path: &Path) -> crate::Result<Extracted> {
    // lofty doesn't read DSF reliably; use the DSD container reader instead.
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("dsf"))
    {
        return extract_dsf(path);
    }
    let tagged = read_from_path(path)?;
    let props = tagged.properties();

    let duration_ms = match props.duration().as_millis() as u64 {
        0 => None,
        ms => Some(ms),
    };
    let codec = Some(codec_label(tagged.file_type()));
    let sample_rate = props.sample_rate();
    let bits = props.bit_depth().map(u32::from);
    let channels = props.channels().map(u32::from);

    let mut e = Extracted {
        title: None,
        artist: None,
        album_artist: None,
        album: None,
        composer: None,
        genre: None,
        track_no: None,
        disc_no: None,
        year: None,
        duration_ms,
        codec,
        sample_rate,
        bits,
        channels,
        art: None,
    };

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        e.title = norm(tag.title());
        e.artist = norm(tag.artist());
        e.album = norm(tag.album());
        e.genre = norm(tag.genre());
        e.album_artist = norm(tag.get_string(ItemKey::AlbumArtist));
        e.composer = norm(tag.get_string(ItemKey::Composer));
        e.track_no = tag.track();
        e.disc_no = tag.disk();
        e.year = tag
            .get_string(ItemKey::Year)
            .or_else(|| tag.get_string(ItemKey::RecordingDate))
            .and_then(parse_year);

        if let Some(pic) = tag.pictures().first() {
            let bytes = pic.data().to_vec();
            if !bytes.is_empty() {
                let hash = blake3::hash(&bytes).to_hex().to_string();
                let mime = pic
                    .mime_type().map_or_else(|| "image/jpeg".into(), |m| m.as_str().to_string());
                e.art = Some((hash, mime, bytes));
            }
        }
    }

    // Fallback: many libraries keep the cover as a folder sidecar (cover.jpg,
    // folder.jpg, …) rather than embedding it — common for M4A/ALAC rips.
    Ok(with_folder_fallback(e, path))
}

/// If `e` has no embedded cover, fall back to a folder sidecar next to `path`.
/// Shared by [`extract`] and [`extract_dsf`].
fn with_folder_fallback(mut e: Extracted, path: &Path) -> Extracted {
    if e.art.is_none() {
        e.art = folder_cover(path);
    }
    e
}

/// The DSD64 sample rate (Hz); higher DSD rates are integer multiples of it.
const DSD64_RATE: u32 = 2_822_400;

/// Sidecar cover-image base names (without extension), in preference order —
/// `folder_cover` picks the lowest-ranked match, so "cover" beats "thumb".
const COVER_NAMES: &[&str] = &["cover", "folder", "front", "albumart", "album", "thumb"];
/// Image extensions we accept for a folder cover.
const COVER_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

/// Look for a cover image alongside `path` (the track's folder). Returns the
/// same `(blake3 hex, mime, bytes)` shape as embedded art so it caches/dedupes
/// identically. Picks the best-ranked name (see [`COVER_NAMES`]) deterministically
/// rather than whatever the directory happens to enumerate first.
pub(crate) fn folder_cover(path: &Path) -> Option<(String, String, Vec<u8>)> {
    let dir = path.parent()?;
    let mut best: Option<(usize, std::path::PathBuf)> = None; // (rank, path)
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let (Some(stem), Some(ext)) = (
            p.file_stem().and_then(|s| s.to_str()),
            p.extension().and_then(|s| s.to_str()),
        ) else {
            continue;
        };
        if !COVER_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        let stem = stem.to_ascii_lowercase();
        if let Some(rank) = COVER_NAMES.iter().position(|n| stem == *n) {
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, p));
                if rank == 0 {
                    break; // "cover" is the top preference; nothing can beat it.
                }
            }
        }
    }
    let p = best?.1;
    let bytes = std::fs::read(&p).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mime = match p.extension().and_then(|s| s.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("png") => "image/png",
        Some(e) if e.eq_ignore_ascii_case("webp") => "image/webp",
        Some(e) if e.eq_ignore_ascii_case("gif") => "image/gif",
        Some(e) if e.eq_ignore_ascii_case("bmp") => "image/bmp",
        _ => "image/jpeg",
    }
    .to_string();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    Some((hash, mime, bytes))
}

/// Extract a `.dsf` (DSD Stream File) via the `dsd-reader` crate: audio facts
/// from the header, tags + cover from its ID3v2 chunk. lofty's DSF support is
/// unreliable, so the library can't lean on it for DSD.
fn extract_dsf(path: &Path) -> crate::Result<Extracted> {
    let r = dsd_reader::DsdReader::from_container(path.to_path_buf())
        .map_err(|e| crate::Error::Other(format!("DSF: {e}")))?;
    let channels = r.channels_num() as u32;
    // `dsd_rate()` is the multiple over DSD64 (1/2/4/8).
    let dsd_rate = DSD64_RATE.saturating_mul(r.dsd_rate().max(1) as u32);
    let audio_len = r.audio_length();
    let duration_ms = (channels > 0 && audio_len > 0).then(|| {
        // Total DSD bytes / channels = bytes/channel; ×8 = samples/channel.
        let samples_per_ch = (audio_len / channels as u64) * 8;
        samples_per_ch * 1000 / dsd_rate as u64
    });

    let mut e = Extracted {
        title: None,
        artist: None,
        album_artist: None,
        album: None,
        composer: None,
        genre: None,
        track_no: None,
        disc_no: None,
        year: None,
        duration_ms,
        codec: Some(format!("DSD{}", dsd_rate / 44_100)),
        // A DSD rate is not a PCM sample rate; the engine reports the DoP wire
        // rate at play time. Leave these unset so the UI shows just "DSD64".
        sample_rate: None,
        bits: None,
        channels: Some(channels),
        art: None,
    };

    if let Some(tag) = r.tag() {
        e.title = norm(tag.title());
        e.artist = norm(tag.artist());
        e.album = norm(tag.album());
        e.album_artist = norm(tag.album_artist());
        e.genre = norm(tag.genre());
        e.year = tag.year();
        e.track_no = tag.track();
        e.disc_no = tag.disc();
        if let Some(pic) = tag.pictures().next() {
            if !pic.data.is_empty() {
                let hash = blake3::hash(&pic.data).to_hex().to_string();
                let mime = if pic.mime_type.is_empty() {
                    "image/jpeg".to_string()
                } else {
                    pic.mime_type.clone()
                };
                e.art = Some((hash, mime, pic.data.clone()));
            }
        }
    }
    Ok(with_folder_fallback(e, path))
}

fn norm<S: AsRef<str>>(v: Option<S>) -> Option<String> {
    v.and_then(|s| {
        let t = s.as_ref().trim();
        (!t.is_empty()).then(|| t.to_string())
    })
}

/// Pull the first four-digit run out of a date/year string.
fn parse_year(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(3))
        .find(|&i| b[i..i + 4].iter().all(u8::is_ascii_digit))
        .and_then(|i| s[i..i + 4].parse().ok())
}

fn codec_label(ft: FileType) -> String {
    match ft {
        FileType::Flac => "FLAC",
        FileType::Wav => "WAV",
        FileType::Aiff => "AIFF",
        FileType::Mpeg => "MP3",
        FileType::Mp4 => "M4A",
        FileType::Opus => "Opus",
        FileType::Vorbis => "Vorbis",
        FileType::Speex => "Speex",
        FileType::Ape => "APE",
        FileType::WavPack => "WavPack",
        FileType::Mpc => "Musepack",
        other => return format!("{other:?}"),
    }
    .to_string()
}

/// Extensions we attempt to index (a superset of the engine's decoders, so the
/// library can show lossy files too even if playback of them is a separate path).
/// `dsf` is DSD (read by lofty's DSF support); SACD `.iso` is handled separately.
pub const AUDIO_EXTS: &[&str] = &[
    "flac", "wav", "wave", "aif", "aiff", "aifc", "m4a", "mp4", "alac", "ape", "wv", "mpc", "opus",
    "ogg", "oga", "mp3", "spx", "dsf",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            let e = e.to_ascii_lowercase();
            AUDIO_EXTS.contains(&e.as_str())
        })
}

// --- SACD .iso -------------------------------------------------------------

/// Per-track metadata pulled from a SACD area TOC.
pub struct SacdTrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: u64,
}

/// An SACD `.iso` album (its preferred — stereo — area).
pub struct SacdAlbum {
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub channels: u32,
    pub tracks: Vec<SacdTrackMeta>,
}

/// Cheap check: a `.iso` whose master TOC begins with `SACDMTOC` (at the LSN or
/// PSN position). Avoids treating a data/video `.iso` as audio.
pub fn is_sacd_iso(path: &Path) -> bool {
    if !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
    {
        return false;
    }
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut id = [0u8; 8];
    for off in [510u64 * 2048, 510 * 2064 + 12] {
        if f.seek(SeekFrom::Start(off)).is_ok() && f.read_exact(&mut id).is_ok() && &id == b"SACDMTOC"
        {
            return true;
        }
    }
    false
}

/// Parse a SACD `.iso`'s TOC into an album + per-track list (stereo area
/// preferred). Uses the `sacd` crate; no audio is decoded.
pub fn extract_sacd(path: &Path) -> crate::Result<SacdAlbum> {
    let img =
        sacd::SacdImage::open(path).map_err(|e| crate::Error::Other(format!("SACD: {e}")))?;
    let area = img
        .areas()
        .iter()
        .find(|a| a.area == sacd::Area::Stereo)
        .or_else(|| img.areas().first())
        .ok_or_else(|| crate::Error::Other("SACD image has no area".into()))?;
    let tracks = area
        .tracks
        .iter()
        .map(|t| SacdTrackMeta {
            title: t.title.clone(),
            artist: t.artist.clone(),
            duration_ms: t.duration.as_millis() as u64,
        })
        .collect();
    Ok(SacdAlbum {
        album: img.album_title().map(str::to_string),
        album_artist: img.album_artist().map(str::to_string),
        channels: area.channels,
        tracks,
    })
}
