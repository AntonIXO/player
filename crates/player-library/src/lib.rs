//! player-library: headless music-library index. Scans a folder tree, extracts
//! tags + embedded art (lofty) and header wire facts, caches them in a
//! SQLite/FTS5 database, refreshes incrementally, and answers fast grouped +
//! fuzzy searches. Kept headless (no GTK / no audio) so it is testable without
//! hardware and `player-core` stays bit-perfect-pure.

mod cue;
mod db;
mod error;
mod extract;
mod model;
mod scan;
mod search;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use nucleo::{Config, Matcher};
use rusqlite::{params, Connection, OptionalExtension, ToSql};

pub use error::{Error, Result};
pub use model::{
    fmt_dur_ms, fmt_khz, Album, Artist, Filter, Folder, ScanProgress, ScanStats, SearchResults,
    Sort, Track,
};

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
    match extract::extract(path) {
        Ok(e) => Track {
            id: -1,
            path: path.to_path_buf(),
            folder,
            title: e.title,
            artist: e.artist,
            album_artist: e.album_artist,
            album: e.album,
            composer: e.composer,
            genre: e.genre,
            track_no: e.track_no,
            disc_no: e.disc_no,
            year: e.year,
            duration_ms: e.duration_ms,
            codec: e.codec,
            sample_rate: e.sample_rate,
            bits: e.bits,
            channels: e.channels,
            art_hash: None,
            source_path: path.to_path_buf(),
            start_ms: None,
        },
        Err(_) => Track {
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
            source_path: path.to_path_buf(),
            start_ms: None,
        },
    }
}

use search::Hay;

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
pub struct Library {
    conn: Connection,
    db_path: PathBuf,
    art_dir: PathBuf,
}

impl Library {
    /// Open (creating if needed) the database at `db_path` with art cached under
    /// `art_dir`.
    pub fn open(db_path: &Path, art_dir: &Path) -> Result<Self> {
        let conn = db::open(db_path)?;
        std::fs::create_dir_all(art_dir)?;
        Ok(Library {
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

    // --- browse ------------------------------------------------------------

    pub fn albums(&self, sort: Sort) -> Result<Vec<Album>> {
        load_albums(&self.conn, album_order(sort))
    }

    pub fn artists(&self) -> Result<Vec<Artist>> {
        load_artists(&self.conn)
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        load_folders(&self.conn)
    }

    pub fn tracks(&self, sort: Sort) -> Result<Vec<Track>> {
        load_tracks_sorted(&self.conn, track_order(sort), None)
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

    /// All albums credited to `artist` (album-artist preferred, matching
    /// [`Library::artists`]), newest year first — for the artist detail page.
    pub fn artist_albums(&self, artist: &str) -> Result<Vec<Album>> {
        let sql = "SELECT track.album, track.album_artist, MAX(track.year), COUNT(*), \
                 COALESCE(SUM(track.duration_ms),0), MAX(track.art_hash) \
             FROM track \
             WHERE COALESCE(NULLIF(track.album_artist,''), track.artist) = ?1 \
                 AND track.album IS NOT NULL AND track.album <> '' \
             GROUP BY track.album_artist, track.album \
             ORDER BY MAX(track.year) DESC, track.album COLLATE NOCASE";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![artist], row_to_album)?;
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

    // --- recently played ---------------------------------------------------

    /// Record that `track_id` was just played (upsert; newest play wins).
    pub fn record_play(&self, track_id: i64) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO play_history(track_id, played_at) VALUES(?1, ?2) \
             ON CONFLICT(track_id) DO UPDATE SET played_at = excluded.played_at",
            params![track_id, now],
        )?;
        Ok(())
    }

    /// The most recently played tracks, newest first (joined back to `track`, so
    /// history rows for since-removed files are skipped).
    pub fn recent_plays(&self, limit: usize) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track \
             JOIN play_history ON play_history.track_id = track.id \
             ORDER BY play_history.played_at DESC LIMIT ?1",
            db::TRACK_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit as i64], db::row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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

}

/// In-memory fuzzy search index. Three nucleo haystacks (tracks, albums,
/// artists — plus a secondary folders one) built once from the `track` table,
/// so the album/artist/folder groupings (a `GROUP BY` each) run at build time
/// rather than per keystroke. Cheap to query (all matching is in RAM; only the
/// matched track rows are fetched back from the DB), and `Send`, so it can live
/// on a search worker thread.
pub struct SearchIndex {
    track_hays: Vec<Hay>,
    albums: Vec<Album>,
    album_hays: Vec<Hay>,
    artists: Vec<Artist>,
    artist_hays: Vec<Hay>,
    folders: Vec<Folder>,
    folder_hays: Vec<Hay>,
}

impl SearchIndex {
    /// Build the index from a library's current database contents. Rebuild
    /// after a scan to pick up changes.
    pub fn build(lib: &Library) -> Result<Self> {
        let conn = &lib.conn;
        let track_hays = load_track_hays(conn)?;

        let albums = load_albums(conn, album_order(Sort::Album))?;
        let album_hays = albums
            .iter()
            .enumerate()
            .map(|(i, a)| Hay {
                id: i as i64,
                text: match a.album_artist.as_deref() {
                    Some(aa) if !aa.is_empty() => format!("{} {}", a.album, aa),
                    _ => a.album.clone(),
                },
            })
            .collect();

        let artists = load_artists(conn)?;
        let artist_hays = artists
            .iter()
            .enumerate()
            .map(|(i, a)| Hay {
                id: i as i64,
                text: a.name.clone(),
            })
            .collect();

        let folders = load_folders(conn)?;
        let folder_hays = folders
            .iter()
            .enumerate()
            .map(|(i, f)| Hay {
                id: i as i64,
                text: f.path.clone(),
            })
            .collect();

        Ok(SearchIndex {
            track_hays,
            albums,
            album_hays,
            artists,
            artist_hays,
            folders,
            folder_hays,
        })
    }

    /// Unified fuzzy search. [`Filter::All`] fills artists, albums and tracks
    /// (plus the secondary folders group) from one call; a specific filter
    /// scopes to that single group. `limit` caps each group. An empty query
    /// returns the first `limit` of each in-scope group (title order for
    /// tracks). `lib` is used only to fetch the matched track rows.
    pub fn query(
        &self,
        lib: &Library,
        query: &str,
        filter: Filter,
        limit: usize,
    ) -> Result<SearchResults> {
        let conn = &lib.conn;
        let q = query.trim();
        let want = |f: Filter| filter == Filter::All || filter == f;
        let mut out = SearchResults::default();

        if q.is_empty() {
            if want(Filter::Tracks) {
                out.tracks = load_tracks_sorted(conn, track_order(Sort::Title), Some(limit))?;
            }
            if want(Filter::Albums) {
                out.albums = self.albums.iter().take(limit).cloned().collect();
            }
            if want(Filter::Artists) {
                out.artists = self.artists.iter().take(limit).cloned().collect();
            }
            if filter == Filter::All {
                out.folders = self.folders.iter().take(limit).cloned().collect();
            }
            return Ok(out);
        }

        // One matcher, reused across every group of this query.
        let mut matcher = Matcher::new(Config::DEFAULT);
        if want(Filter::Tracks) {
            let ids = search::match_topn(&mut matcher, &self.track_hays, q, limit);
            out.tracks = load_tracks_by_ids(conn, &ids)?;
        }
        if want(Filter::Albums) {
            out.albums = search::match_topn(&mut matcher, &self.album_hays, q, limit)
                .into_iter()
                .filter_map(|i| self.albums.get(i as usize).cloned())
                .collect();
        }
        if want(Filter::Artists) {
            out.artists = search::match_topn(&mut matcher, &self.artist_hays, q, limit)
                .into_iter()
                .filter_map(|i| self.artists.get(i as usize).cloned())
                .collect();
        }
        if filter == Filter::All {
            out.folders = search::match_topn(&mut matcher, &self.folder_hays, q, limit)
                .into_iter()
                .filter_map(|i| self.folders.get(i as usize).cloned())
                .collect();
        }
        Ok(out)
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

// --- shared query helpers (used by both `Library` browse and `SearchIndex`) --

fn album_order(sort: Sort) -> &'static str {
    match sort {
        Sort::Artist => "track.album_artist COLLATE NOCASE, track.album COLLATE NOCASE",
        Sort::Year => "MAX(track.year) DESC, track.album COLLATE NOCASE",
        _ => "track.album COLLATE NOCASE",
    }
}

fn track_order(sort: Sort) -> &'static str {
    match sort {
        Sort::Title => "track.title COLLATE NOCASE",
        Sort::Artist => {
            "track.artist COLLATE NOCASE, track.album COLLATE NOCASE, track.disc_no, track.track_no"
        }
        Sort::Album => "track.album COLLATE NOCASE, track.disc_no, track.track_no",
        Sort::Year => "track.year DESC, track.album COLLATE NOCASE, track.disc_no, track.track_no",
    }
}

fn load_albums(conn: &Connection, order: &str) -> Result<Vec<Album>> {
    let sql = format!(
        "SELECT track.album, track.album_artist, MAX(track.year), COUNT(*), \
             COALESCE(SUM(track.duration_ms),0), MAX(track.art_hash) \
         FROM track WHERE track.album IS NOT NULL AND track.album <> '' \
         GROUP BY track.album_artist, track.album ORDER BY {order}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_album)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_artists(conn: &Connection) -> Result<Vec<Artist>> {
    let mut stmt = conn.prepare(
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

fn load_folders(conn: &Connection) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(
        "SELECT folder, COUNT(*), COALESCE(SUM(duration_ms),0) FROM track \
         GROUP BY folder ORDER BY folder COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], row_to_folder)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_tracks_sorted(conn: &Connection, order: &str, limit: Option<usize>) -> Result<Vec<Track>> {
    let lim = match limit {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let sql = format!("SELECT {} FROM track ORDER BY {order}{lim}", db::TRACK_COLS);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], db::row_to_track)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Fetch tracks by id, preserving the order of `ids` (used for ranked hits).
fn load_tracks_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<Track>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {} FROM track WHERE track.id IN ({placeholders})",
        db::TRACK_COLS
    );
    let mut stmt = conn.prepare(&sql)?;
    let bind: Vec<&dyn ToSql> = ids.iter().map(|i| i as &dyn ToSql).collect();
    let mut by_id: HashMap<i64, Track> = HashMap::new();
    let rows = stmt.query_map(bind.as_slice(), db::row_to_track)?;
    for t in rows {
        let t = t?;
        by_id.insert(t.id, t);
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Build the tracks haystack: `title artist album album_artist` per track id.
fn load_track_hays(conn: &Connection) -> Result<Vec<Hay>> {
    let mut stmt = conn.prepare(
        "SELECT id, TRIM(COALESCE(title,'')||' '||COALESCE(artist,'')||' '|| \
             COALESCE(album,'')||' '||COALESCE(album_artist,'')) FROM track",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Hay {
            id: r.get(0)?,
            text: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
