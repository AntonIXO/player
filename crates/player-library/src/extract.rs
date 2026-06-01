//! Tag + cover-art + header extraction via lofty. One file open per track.

use std::path::Path;

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
                    .mime_type()
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "image/jpeg".into());
                e.art = Some((hash, mime, bytes));
            }
        }
    }

    // Fallback: many libraries keep the cover as a folder sidecar (cover.jpg,
    // folder.jpg, …) rather than embedding it — common for M4A/ALAC rips.
    if e.art.is_none() {
        e.art = folder_cover(path);
    }

    Ok(e)
}

/// Sidecar cover-image base names (without extension), case-insensitive.
const COVER_NAMES: &[&str] = &["cover", "folder", "front", "albumart", "album", "thumb"];
/// Image extensions we accept for a folder cover.
const COVER_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

/// Look for a cover image alongside `path` (the track's folder). Returns the
/// same `(blake3 hex, mime, bytes)` shape as embedded art so it caches/dedupes
/// identically.
fn folder_cover(path: &Path) -> Option<(String, String, Vec<u8>)> {
    let dir = path.parent()?;
    let mut chosen: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        let stem = stem.to_ascii_lowercase();
        let ext = ext.to_ascii_lowercase();
        if !COVER_EXTS.contains(&ext.as_str()) {
            continue;
        }
        if COVER_NAMES.iter().any(|n| stem == *n) {
            // Prefer the canonical "cover"/"folder"; take it immediately.
            if stem == "cover" || stem == "folder" {
                chosen = Some(p);
                break;
            }
            chosen.get_or_insert(p);
        }
    }
    let p = chosen?;
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
pub const AUDIO_EXTS: &[&str] = &[
    "flac", "wav", "wave", "aif", "aiff", "aifc", "m4a", "mp4", "alac", "ape", "wv", "mpc", "opus",
    "ogg", "oga", "mp3", "spx",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            AUDIO_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}
