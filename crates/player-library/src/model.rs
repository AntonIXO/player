//! Public data types returned by the library.

use std::path::PathBuf;
use std::time::Duration;

/// One indexed track. Wire facts (codec/rate/bits/channels/duration) are read
/// from the file header by lofty; they are header-accurate for display. The
/// *playback* signal path is still reported authoritatively by the engine at
/// play time (`player_core::Event::Started`).
#[derive(Debug, Clone)]
pub struct Track {
    pub id: i64,
    pub path: PathBuf,
    pub folder: String,
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
    /// blake3 hex of the embedded cover, if any; the file lives at
    /// `Library::art_path(hash)`.
    pub art_hash: Option<String>,
    /// The audio file the engine should decode. Equals `path` for a whole-file
    /// track; for a `.cue` track it is the referenced audio file (while `path`
    /// is a synthetic per-track id like `album.cue#02`).
    pub source_path: PathBuf,
    /// Start offset into `source_path` for a `.cue` track; `None` (whole file)
    /// otherwise. With `duration_ms` this gives the track's decode range.
    pub start_ms: Option<u64>,
}

impl Track {
    /// Title, falling back to the file stem.
    pub fn display_title(&self) -> String {
        self.title.clone().unwrap_or_else(|| {
            self.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.display().to_string())
        })
    }

    /// Artist · Album, for list subtitles.
    pub fn subtitle(&self) -> String {
        match (&self.artist, &self.album) {
            (Some(a), Some(al)) => format!("{a} · {al}"),
            (Some(a), None) => a.clone(),
            (None, Some(al)) => al.clone(),
            (None, None) => "Unknown Artist".into(),
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        self.duration_ms.map(Duration::from_millis)
    }

    /// The `(start, end)` decode range for a `.cue` track, or `None` for a
    /// whole-file track (decode from the beginning to EOF).
    pub fn cue_range(&self) -> Option<(Duration, Duration)> {
        self.start_ms.map(|s| {
            let start = Duration::from_millis(s);
            (start, start + Duration::from_millis(self.duration_ms.unwrap_or(0)))
        })
    }

    /// The signal-path string for the bit-perfect chip, without duration:
    /// `24-bit · 96 kHz · FLAC`.
    pub fn signal_spec(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(b) = self.bits {
            parts.push(format!("{b}-bit"));
        }
        if let Some(r) = self.sample_rate {
            parts.push(fmt_khz(r));
        }
        if let Some(c) = &self.codec {
            parts.push(c.clone());
        }
        parts.join(" · ")
    }

    /// The mono signal-path string the UI shows, e.g. `3:26 · FLAC · 192 kHz · 24-bit`.
    pub fn format_spec(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(ms) = self.duration_ms {
            parts.push(fmt_dur_ms(ms));
        }
        if let Some(c) = &self.codec {
            parts.push(c.clone());
        }
        if let Some(r) = self.sample_rate {
            parts.push(fmt_khz(r));
        }
        if let Some(b) = self.bits {
            parts.push(format!("{b}-bit"));
        }
        parts.join(" · ")
    }
}

/// A derived album (grouped by album_artist + album).
#[derive(Debug, Clone)]
pub struct Album {
    pub album: String,
    pub album_artist: Option<String>,
    pub year: Option<i32>,
    pub track_count: u32,
    pub total_ms: u64,
    pub art_hash: Option<String>,
}

impl Album {
    /// `N tracks · 42:56 · 1973`, for the Search/Albums rows.
    pub fn meta(&self) -> String {
        let mut s = format!(
            "{} track{} · {}",
            self.track_count,
            if self.track_count == 1 { "" } else { "s" },
            fmt_dur_ms(self.total_ms)
        );
        if let Some(y) = self.year {
            s.push_str(&format!(" · {y}"));
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct Artist {
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub path: String,
    pub track_count: u32,
    pub total_ms: u64,
}

impl Folder {
    /// Last path component, for the row title.
    pub fn name(&self) -> &str {
        self.path
            .rsplit(['/', '\\'])
            .find(|s| !s.is_empty())
            .unwrap_or(&self.path)
    }
    pub fn meta(&self) -> String {
        format!(
            "{} track{} · {}",
            self.track_count,
            if self.track_count == 1 { "" } else { "s" },
            fmt_dur_ms(self.total_ms)
        )
    }
}

/// Grouped search output for the Search screen. A single fuzzy query fills all
/// of these at once (see [`crate::SearchIndex::query`]); `Folders` is secondary
/// and only populated for [`Filter::All`].
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
    pub folders: Vec<Folder>,
}

/// Which entity group(s) the search filter chips scope to. `All` returns
/// artists, albums and tracks together (the default unified search).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Tracks,
    Albums,
    Artists,
}

/// Sort order for browse lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Title,
    Artist,
    Album,
    /// Newest first (tracks/albums with no year sort last).
    Year,
}

/// Counters returned by a scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanStats {
    pub added: u64,
    pub updated: u64,
    pub moved: u64,
    pub removed: u64,
    pub unchanged: u64,
    pub errors: u64,
    pub elapsed_ms: u128,
}

impl ScanStats {
    pub fn total_seen(&self) -> u64 {
        self.added + self.updated + self.moved + self.unchanged
    }
}

/// Progress tick during a scan (called from worker threads).
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub seen: u64,
    pub total: u64,
    pub path: PathBuf,
}

// --- formatting helpers ----------------------------------------------------

/// `44100 -> "44.1 kHz"`, `96000 -> "96 kHz"`.
pub fn fmt_khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

/// `ms -> "m:ss"` (or `h:mm:ss`).
pub fn fmt_dur_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
