//! player-library: headless music-library index. Scans a folder tree, extracts
//! tags + embedded art (lofty) and header wire facts, caches them in a
//! SQLite/FTS5 database, refreshes incrementally, and answers fast grouped +
//! fuzzy searches. Kept headless (no GTK / no audio) so it is testable without
//! hardware and `player-core` stays bit-perfect-pure.

mod db;
mod error;
mod extract;
mod model;
mod scan;
mod search;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher};
use rusqlite::{params, Connection, OptionalExtension, ToSql};

pub use error::{Error, Result};
pub use model::{
    fmt_dur_ms, fmt_khz, Album, Artist, Filter, Folder, ScanProgress, ScanStats, SearchResults,
    Sort, Track,
};

use search::Hay;

/// Aggregate counts for the whole library.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryStats {
    pub tracks: u64,
    pub albums: u64,
    pub artists: u64,
    pub folders: u64,
}

/// The library: a SQLite connection plus an in-memory nucleo haystack for
/// typeahead. Use on one thread (e.g. the GTK main loop or the CLI).
pub struct Library {
    conn: Connection,
    db_path: PathBuf,
    art_dir: PathBuf,
    hays: Vec<Hay>,
}

impl Library {
    /// Open (creating if needed) the database at `db_path` with art cached under
    /// `art_dir`.
    pub fn open(db_path: &Path, art_dir: &Path) -> Result<Self> {
        let conn = db::open(db_path)?;
        std::fs::create_dir_all(art_dir)?;
        let mut lib = Library {
            conn,
            db_path: db_path.to_path_buf(),
            art_dir: art_dir.to_path_buf(),
            hays: Vec::new(),
        };
        lib.reload_index()?;
        Ok(lib)
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
            Some(pd) => (
                pd.data_dir().join("library.db"),
                pd.cache_dir().join("art"),
            ),
            None => (PathBuf::from("library.db"), PathBuf::from("art")),
        }
    }

    /// Filesystem path of a cached cover by its hash (may not exist).
    pub fn art_path(&self, hash: &str) -> PathBuf {
        self.art_dir.join(hash)
    }

    // --- scanning ----------------------------------------------------------

    /// Incrementally scan `root` and rebuild the typeahead index.
    pub fn scan(&mut self, root: &Path) -> Result<ScanStats> {
        self.scan_with_progress(root, false, |_| {})
    }

    /// Scan with a progress callback (called from worker threads). `force`
    /// re-extracts unchanged files too (see [`scan::scan`]).
    pub fn scan_with_progress(
        &mut self,
        root: &Path,
        force: bool,
        progress: impl Fn(ScanProgress) + Send + Sync,
    ) -> Result<ScanStats> {
        let stats = scan::scan(&self.db_path, &self.art_dir, root, force, progress)?;
        self.reload_index()?;
        Ok(stats)
    }

    /// Rebuild the in-memory typeahead index from the current database (call
    /// after an external process — e.g. a scan on another thread — has written).
    pub fn refresh(&mut self) -> Result<()> {
        self.reload_index()
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

    // --- search ------------------------------------------------------------

    /// Instant fuzzy typeahead over title/artist/album. Empty query returns the
    /// first `limit` tracks by title.
    pub fn search_typeahead(&self, query: &str, limit: usize) -> Vec<Track> {
        let q = query.trim();
        if q.is_empty() {
            return self
                .tracks_sorted(Sort::Title, Some(limit))
                .unwrap_or_default();
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pat = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let ids: Vec<i64> = pat
            .match_list(self.hays.iter(), &mut matcher)
            .into_iter()
            .take(limit)
            .map(|(h, _score)| h.id)
            .collect();
        self.tracks_by_ids(&ids).unwrap_or_default()
    }

    /// Grouped search (Albums / Folders / Tracks) for the Search screen.
    pub fn search_grouped(&self, query: &str, filter: Filter) -> Result<SearchResults> {
        let Some(expr) = search::fts_expr(query, filter) else {
            return Ok(SearchResults::default());
        };
        Ok(SearchResults {
            albums: self.fts_albums(&expr, 20)?,
            folders: self.fts_folders(&expr, 20)?,
            tracks: self.fts_tracks(&expr, 50)?,
        })
    }

    // --- browse ------------------------------------------------------------

    pub fn albums(&self, sort: Sort) -> Result<Vec<Album>> {
        let order = match sort {
            Sort::Artist => "track.album_artist COLLATE NOCASE, track.album COLLATE NOCASE",
            _ => "track.album COLLATE NOCASE",
        };
        let sql = format!(
            "SELECT track.album, track.album_artist, MAX(track.year), COUNT(*), \
                 COALESCE(SUM(track.duration_ms),0), MAX(track.art_hash) \
             FROM track WHERE track.album IS NOT NULL AND track.album <> '' \
             GROUP BY track.album_artist, track.album ORDER BY {order}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_album)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn artists(&self) -> Result<Vec<Artist>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(NULLIF(album_artist,''), artist) AS a, \
                 COUNT(DISTINCT album), COUNT(*) FROM track \
             WHERE COALESCE(NULLIF(album_artist,''), artist) IS NOT NULL \
             GROUP BY a ORDER BY a COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Artist {
                name: r.get(0)?,
                album_count: r.get::<_, i64>(1)? as u32,
                track_count: r.get::<_, i64>(2)? as u32,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT folder, COUNT(*), COALESCE(SUM(duration_ms),0) FROM track \
             GROUP BY folder ORDER BY folder COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], row_to_folder)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn tracks(&self, sort: Sort) -> Result<Vec<Track>> {
        self.tracks_sorted(sort, None)
    }

    /// All tracks of one album (ordered disc/track), ready for gapless enqueue.
    pub fn album_tracks(&self, album: &str, album_artist: Option<&str>) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track WHERE track.album = ?1 AND (track.album_artist IS ?2) \
             ORDER BY track.disc_no, track.track_no, track.title COLLATE NOCASE",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![album, album_artist], db::row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// All tracks credited to `artist` (album-artist preferred, falling back to
    /// the track artist — matching how [`Library::artists`] groups), ordered by
    /// album then disc/track.
    pub fn artist_tracks(&self, artist: &str) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track \
             WHERE COALESCE(NULLIF(track.album_artist,''), track.artist) = ?1 \
             ORDER BY track.album COLLATE NOCASE, track.disc_no, track.track_no, \
                 track.title COLLATE NOCASE",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![artist], db::row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// All tracks directly inside `folder`, ordered disc/track/title.
    pub fn folder_tracks(&self, folder: &str) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track WHERE track.folder = ?1 \
             ORDER BY track.disc_no, track.track_no, track.title COLLATE NOCASE",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![folder], db::row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Look up a single indexed track by its exact path (for restoring a saved
    /// queue/playlist with full metadata; returns `None` if not indexed).
    pub fn track_by_path(&self, path: &Path) -> Result<Option<Track>> {
        let sql = format!("SELECT {} FROM track WHERE track.path = ?1", db::TRACK_COLS);
        let p = path.to_string_lossy();
        let mut stmt = self.conn.prepare(&sql)?;
        let t = stmt
            .query_row(params![p.as_ref()], db::row_to_track)
            .optional()?;
        Ok(t)
    }

    // --- persisted key/value (settings + last session) ---------------------

    /// Read a persisted value from the `meta` table.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(v)
    }

    /// Write (upsert) a persisted value into the `meta` table.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(k, v) VALUES(?1, ?2) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let count = |sql: &str| -> Result<u64> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
        };
        Ok(LibraryStats {
            tracks: count("SELECT COUNT(*) FROM track")?,
            albums: count(
                "SELECT COUNT(*) FROM (SELECT 1 FROM track \
                 WHERE album IS NOT NULL AND album <> '' GROUP BY album_artist, album)",
            )?,
            artists: count(
                "SELECT COUNT(*) FROM (SELECT 1 FROM track \
                 WHERE COALESCE(NULLIF(album_artist,''), artist) IS NOT NULL \
                 GROUP BY COALESCE(NULLIF(album_artist,''), artist))",
            )?,
            folders: count("SELECT COUNT(DISTINCT folder) FROM track")?,
        })
    }

    // --- internals ---------------------------------------------------------

    fn tracks_sorted(&self, sort: Sort, limit: Option<usize>) -> Result<Vec<Track>> {
        let order = match sort {
            Sort::Title => "track.title COLLATE NOCASE",
            Sort::Artist => {
                "track.artist COLLATE NOCASE, track.album COLLATE NOCASE, track.disc_no, track.track_no"
            }
            Sort::Album => "track.album COLLATE NOCASE, track.disc_no, track.track_no",
        };
        let lim = match limit {
            Some(n) => format!(" LIMIT {n}"),
            None => String::new(),
        };
        let sql = format!(
            "SELECT {} FROM track ORDER BY {order}{lim}",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], db::row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Fetch tracks by id, preserving the order of `ids` (used for ranked hits).
    fn tracks_by_ids(&self, ids: &[i64]) -> Result<Vec<Track>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT {} FROM track WHERE track.id IN ({placeholders})",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let bind: Vec<&dyn ToSql> = ids.iter().map(|i| i as &dyn ToSql).collect();
        let mut by_id: HashMap<i64, Track> = HashMap::new();
        let rows = stmt.query_map(bind.as_slice(), db::row_to_track)?;
        for t in rows {
            let t = t?;
            by_id.insert(t.id, t);
        }
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    fn fts_tracks(&self, expr: &str, limit: usize) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track_fts JOIN track ON track.id = track_fts.rowid \
             WHERE track_fts MATCH ?1 ORDER BY rank LIMIT ?2",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![expr, limit as i64], db::row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn fts_albums(&self, expr: &str, limit: usize) -> Result<Vec<Album>> {
        let mut stmt = self.conn.prepare(
            "SELECT track.album, track.album_artist, MAX(track.year), COUNT(*), \
                 COALESCE(SUM(track.duration_ms),0), MAX(track.art_hash) \
             FROM track_fts JOIN track ON track.id = track_fts.rowid \
             WHERE track_fts MATCH ?1 AND track.album IS NOT NULL AND track.album <> '' \
             GROUP BY track.album_artist, track.album \
             ORDER BY track.album COLLATE NOCASE LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![expr, limit as i64], row_to_album)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn fts_folders(&self, expr: &str, limit: usize) -> Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT track.folder, COUNT(*), COALESCE(SUM(track.duration_ms),0) \
             FROM track_fts JOIN track ON track.id = track_fts.rowid \
             WHERE track_fts MATCH ?1 GROUP BY track.folder \
             ORDER BY COUNT(*) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![expr, limit as i64], row_to_folder)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Rebuild the nucleo haystack from the current DB contents.
    fn reload_index(&mut self) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, TRIM(COALESCE(title,'')||' '||COALESCE(artist,'')||' '|| \
                 COALESCE(album,'')||' '||COALESCE(album_artist,'')) FROM track",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Hay {
                id: r.get(0)?,
                text: r.get(1)?,
            })
        })?;
        self.hays = rows.collect::<rusqlite::Result<_>>()?;
        Ok(())
    }
}

/// Live filesystem watcher handle. Drop it to stop watching.
pub struct LibraryWatcher {
    _watcher: notify::RecommendedWatcher,
    /// Fires (coalesce yourself) when the watched tree changes.
    pub rx: crossbeam_channel::Receiver<()>,
}

fn row_to_album(r: &rusqlite::Row) -> rusqlite::Result<Album> {
    Ok(Album {
        album: r.get(0)?,
        album_artist: r.get(1)?,
        year: r.get(2)?,
        track_count: r.get::<_, i64>(3)? as u32,
        total_ms: r.get::<_, i64>(4)? as u64,
        art_hash: r.get(5)?,
    })
}

fn row_to_folder(r: &rusqlite::Row) -> rusqlite::Result<Folder> {
    Ok(Folder {
        path: r.get(0)?,
        track_count: r.get::<_, i64>(1)? as u32,
        total_ms: r.get::<_, i64>(2)? as u64,
    })
}
