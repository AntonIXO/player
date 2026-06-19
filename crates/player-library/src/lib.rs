//! player-library: headless music-library index. Scans a folder tree, extracts
//! tags + embedded art (lofty) and header wire facts, caches them in a
//! SQLite/FTS5 database, refreshes incrementally, and answers fast grouped +
//! fuzzy searches. Kept headless (no GTK / no audio) so it is testable without
//! hardware and `player-core` stays bit-perfect-pure.
//!
//! `lib.rs` is the facade: the [`Library`] lifecycle + scan surface and the
//! public types. The implementation is split across sibling modules — the browse
//! read-path ([`browse`]), the key/value store ([`meta`]), and the in-memory
//! fuzzy index ([`searchindex`]) — which add their own `impl Library` blocks.

mod browse;
mod cue;
mod db;
mod error;
mod extract;
mod meta;
mod model;
mod scan;
mod search;
mod searchindex;

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use rusqlite::Connection;

pub use error::{Error, Result};
pub use model::{
    fmt_dur_ms, fmt_khz, Album, Artist, Filter, Folder, ScanProgress, ScanStats, SearchResults,
    Sort, Track,
};
pub use searchindex::SearchIndex;

/// Build a standalone [`Track`] from an audio file by extracting its tags and
/// header facts (no DB row, no saved cover art). Used for ad-hoc "Open file"
/// playback so the now-playing view shows a real title, duration, and sample
/// rate even for files outside the indexed library — the seek bar and elapsed
/// clock both depend on a known sample rate / duration, so a metadata-less stub
/// would freeze them. Falls back to a path-only [`Track`] if extraction fails.
pub fn track_from_path(path: &Path) -> Track {
    let folder = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut t = track_stub(path, folder);
    // Overlay whatever the extractor recovered; a failed extract leaves the stub
    // (path-only) so the now-playing view still has a real source to play.
    if let Ok(e) = extract::extract(path) {
        t.title = e.title;
        t.artist = e.artist;
        t.album_artist = e.album_artist;
        t.album = e.album;
        t.composer = e.composer;
        t.genre = e.genre;
        t.track_no = e.track_no;
        t.disc_no = e.disc_no;
        t.year = e.year;
        t.duration_ms = e.duration_ms;
        t.codec = e.codec;
        t.sample_rate = e.sample_rate;
        t.bits = e.bits;
        t.channels = e.channels;
    }
    t
}

/// A metadata-less [`Track`] for `path` (id `-1`, no DB row). `track_from_path`
/// overlays extracted tags on top; on extraction failure this stub is returned
/// as-is so playback still has a valid `source_path`.
fn track_stub(path: &Path, folder: String) -> Track {
    Track {
        id: -1,
        path: path.to_path_buf(),
        folder,
        title: None,
        artist: None,
        album_artist: None,
        album: None,
        composer: None,
        genre: None,
        track_no: None,
        disc_no: None,
        year: None,
        duration_ms: None,
        codec: None,
        sample_rate: None,
        bits: None,
        channels: None,
        art_hash: None,
        source_path: None,
        start_ms: None,
        loved: false,
    }
}

/// Aggregate counts for the whole library.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryStats {
    pub tracks: u64,
    pub albums: u64,
    pub artists: u64,
    pub folders: u64,
}

/// The library: a SQLite connection for writes (scan), browse queries, and the
/// persisted key/value store. Search is served separately by a [`SearchIndex`]
/// (typically owned by a worker thread). Use a `Library` on one thread.
///
/// The browse / play-history / loved queries live in [`crate::browse`], the
/// `meta` key/value store in [`crate::meta`]; both add `impl Library` blocks.
pub struct Library {
    pub(crate) conn: Connection,
    db_path: PathBuf,
    art_dir: PathBuf,
}

impl Library {
    /// Open (creating if needed) the database at `db_path` with art cached under
    /// `art_dir`.
    pub fn open(db_path: &Path, art_dir: &Path) -> Result<Self> {
        let conn = db::open(db_path)?;
        std::fs::create_dir_all(art_dir)?;
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
            art_dir: art_dir.to_path_buf(),
        })
    }

    /// Open at the platform-default XDG locations (`$XDG_DATA_HOME/player/library.db`
    /// and `$XDG_CACHE_HOME/player/art`).
    pub fn open_default() -> Result<Self> {
        let (db, art) = Self::default_paths();
        Self::open(&db, &art)
    }

    /// The default `(database, art_dir)` paths.
    pub fn default_paths() -> (PathBuf, PathBuf) {
        match ProjectDirs::from("org", "player", "player") {
            Some(pd) => (pd.data_dir().join("library.db"), pd.cache_dir().join("art")),
            None => (PathBuf::from("library.db"), PathBuf::from("art")),
        }
    }

    /// Filesystem path of a cached cover by its hash (may not exist).
    pub fn art_path(&self, hash: &str) -> PathBuf {
        self.art_dir.join(hash)
    }

    // --- scanning ----------------------------------------------------------

    /// Incrementally scan `root`. Callers that hold a [`SearchIndex`] should
    /// rebuild it afterwards (`SearchIndex::build`) to pick up the changes.
    pub fn scan(&self, root: &Path) -> Result<ScanStats> {
        self.scan_with_progress(root, false, |_| {})
    }

    /// Scan with a progress callback (called from worker threads). `force`
    /// re-extracts unchanged files too (see [`scan::scan`]).
    pub fn scan_with_progress(
        &self,
        root: &Path,
        force: bool,
        progress: impl Fn(ScanProgress) + Send + Sync,
    ) -> Result<ScanStats> {
        scan::scan(&self.db_path, &self.art_dir, root, force, progress)
    }

    /// Start watching `root` for live changes (opt-in). The returned receiver
    /// fires when something under `root` is created/modified/removed; the caller
    /// re-runs [`scan`](Self::scan). Default usage is off.
    pub fn watch(root: &Path) -> Result<LibraryWatcher> {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.kind.is_create() || ev.kind.is_modify() || ev.kind.is_remove() {
                    let _ = tx.send(());
                }
            }
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(LibraryWatcher {
            _watcher: watcher,
            rx,
        })
    }
}

/// Live filesystem watcher handle. Drop it to stop watching.
pub struct LibraryWatcher {
    _watcher: notify::RecommendedWatcher,
    /// Fires (coalesce yourself) when the watched tree changes.
    pub rx: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp directory for an isolated on-disk database (no fixtures or
    /// ffmpeg needed — these exercise pure key/value + path-resolution paths).
    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "player-lib-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn set_meta_many_upserts_in_one_tx() {
        let root = tmp();
        let lib = Library::open(&root.join("library.db"), &root.join("art")).unwrap();

        lib.set_meta_many(&[("queue", "a\nb"), ("current", "1"), ("pos_ms", "500")])
            .unwrap();
        assert_eq!(lib.get_meta("queue").unwrap().as_deref(), Some("a\nb"));
        assert_eq!(lib.get_meta("current").unwrap().as_deref(), Some("1"));
        assert_eq!(lib.get_meta("pos_ms").unwrap().as_deref(), Some("500"));

        // Re-running upserts (overwrites) the existing keys, not duplicates them.
        lib.set_meta_many(&[("current", "2"), ("pos_ms", "0")]).unwrap();
        assert_eq!(lib.get_meta("queue").unwrap().as_deref(), Some("a\nb"));
        assert_eq!(lib.get_meta("current").unwrap().as_deref(), Some("2"));
        assert_eq!(lib.get_meta("pos_ms").unwrap().as_deref(), Some("0"));

        // An empty batch is a no-op (and must not error).
        lib.set_meta_many(&[]).unwrap();
    }

    #[test]
    fn tracks_by_paths_empty_and_missing() {
        let root = tmp();
        let lib = Library::open(&root.join("library.db"), &root.join("art")).unwrap();

        assert!(lib.tracks_by_paths(&[]).unwrap().is_empty());
        // Unindexed paths are simply absent — never an error, never a panic.
        let got = lib
            .tracks_by_paths(&[PathBuf::from("/no/such/a.flac"), PathBuf::from("/no/such/b.flac")])
            .unwrap();
        assert!(got.is_empty());
    }
}
