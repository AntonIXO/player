//! Browse read-path: every query that returns albums / artists / folders /
//! tracks, plus play-history and loved-track reads/writes. Split out of `lib.rs`
//! so the `Library` facade there stays small. These run on the `Library`'s own
//! SQLite connection (a child module may touch the parent's private `conn`), and
//! the shared free helpers (`load_*`, `*_order`, `row_to_*`) are `pub(crate)` so
//! the in-memory [`crate::SearchIndex`] reuses the exact same queries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, ToSql};

use crate::db;
use crate::model::{Album, Artist, Folder, Sort, Track};
use crate::search::Hay;
use crate::{Library, LibraryStats, Result};

/// SQLite caps a statement's bound parameters (historically 999); chunk long
/// `... IN (?, ?, …)` path/id lists below that so a huge queue never overflows it.
const PARAM_CHUNK: usize = 900;

/// `prepare` + `query_map` + collect, the shape every browse query repeats.
pub(crate) fn query_collect<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    map: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, map)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `?,?,…` for an `IN (...)` clause of `n` bound parameters.
fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

impl Library {
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
        query_collect(&self.conn, &sql, params![album, album_artist], db::row_to_track)
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
        query_collect(&self.conn, &sql, params![artist], db::row_to_track)
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
        query_collect(&self.conn, sql, params![artist], row_to_album)
    }

    /// All tracks directly inside `folder`, ordered disc/track/title.
    pub fn folder_tracks(&self, folder: &str) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track WHERE track.folder = ?1 \
             ORDER BY track.disc_no, track.track_no, track.title COLLATE NOCASE",
            db::TRACK_COLS
        );
        query_collect(&self.conn, &sql, params![folder], db::row_to_track)
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

    /// Resolve many tracks by exact path in one query (instead of N
    /// [`track_by_path`](Self::track_by_path) round-trips when restoring a saved
    /// queue). Missing (unindexed) paths are simply absent from the map; the
    /// caller maps its own ordered path list through it. Chunked so a very long
    /// queue never exceeds SQLite's bound-parameter limit.
    pub fn tracks_by_paths(&self, paths: &[PathBuf]) -> Result<HashMap<PathBuf, Track>> {
        let mut by_path: HashMap<PathBuf, Track> = HashMap::new();
        for chunk in paths.chunks(PARAM_CHUNK) {
            let sql = format!(
                "SELECT {} FROM track WHERE track.path IN ({})",
                db::TRACK_COLS,
                placeholders(chunk.len())
            );
            let strs: Vec<String> = chunk.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            let bind: Vec<&dyn ToSql> = strs.iter().map(|s| s as &dyn ToSql).collect();
            for t in query_collect(&self.conn, &sql, bind.as_slice(), db::row_to_track)? {
                by_path.insert(t.path.clone(), t);
            }
        }
        Ok(by_path)
    }

    // --- recently played ---------------------------------------------------

    /// Record that `track_id` was just played (upsert; newest play wins).
    pub fn record_play(&self, track_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO play_history(track_id, played_at) VALUES(?1, ?2) \
             ON CONFLICT(track_id) DO UPDATE SET played_at = excluded.played_at",
            params![track_id, now_secs()],
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
        query_collect(&self.conn, &sql, params![limit as i64], db::row_to_track)
    }

    // --- loved (favourite) tracks ------------------------------------------

    /// Mark `track_id` loved or not. Loving inserts a row (keeping the original
    /// `loved_at` if already present); unloving deletes it.
    pub fn set_loved(&self, track_id: i64, loved: bool) -> Result<()> {
        if loved {
            self.conn.execute(
                "INSERT INTO loved_tracks(track_id, loved_at) VALUES(?1, ?2) \
                 ON CONFLICT(track_id) DO NOTHING",
                params![track_id, now_secs()],
            )?;
        } else {
            self.conn
                .execute("DELETE FROM loved_tracks WHERE track_id = ?1", params![track_id])?;
        }
        Ok(())
    }

    /// All loved tracks, most-recently-loved first (joined back to `track`, so
    /// loved rows for since-removed files are skipped).
    pub fn loved_tracks(&self) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM track \
             JOIN loved_tracks ON loved_tracks.track_id = track.id \
             ORDER BY loved_tracks.loved_at DESC",
            db::TRACK_COLS
        );
        query_collect(&self.conn, &sql, [], db::row_to_track)
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

/// Seconds since the Unix epoch (0 if the clock is before it).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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

pub(crate) fn album_order(sort: Sort) -> &'static str {
    match sort {
        Sort::Artist => "track.album_artist COLLATE NOCASE, track.album COLLATE NOCASE",
        Sort::Year => "MAX(track.year) DESC, track.album COLLATE NOCASE",
        _ => "track.album COLLATE NOCASE",
    }
}

pub(crate) fn track_order(sort: Sort) -> &'static str {
    match sort {
        Sort::Title => "track.title COLLATE NOCASE",
        Sort::Artist => {
            "track.artist COLLATE NOCASE, track.album COLLATE NOCASE, track.disc_no, track.track_no"
        }
        Sort::Album => "track.album COLLATE NOCASE, track.disc_no, track.track_no",
        Sort::Year => "track.year DESC, track.album COLLATE NOCASE, track.disc_no, track.track_no",
    }
}

pub(crate) fn load_albums(conn: &Connection, order: &str) -> Result<Vec<Album>> {
    let sql = format!(
        "SELECT track.album, track.album_artist, MAX(track.year), COUNT(*), \
             COALESCE(SUM(track.duration_ms),0), MAX(track.art_hash) \
         FROM track WHERE track.album IS NOT NULL AND track.album <> '' \
         GROUP BY track.album_artist, track.album ORDER BY {order}"
    );
    query_collect(conn, &sql, [], row_to_album)
}

pub(crate) fn load_artists(conn: &Connection) -> Result<Vec<Artist>> {
    query_collect(
        conn,
        "SELECT COALESCE(NULLIF(album_artist,''), artist) AS a, \
             COUNT(DISTINCT album), COUNT(*) FROM track \
         WHERE COALESCE(NULLIF(album_artist,''), artist) IS NOT NULL \
         GROUP BY a ORDER BY a COLLATE NOCASE",
        [],
        |r| {
            Ok(Artist {
                name: r.get(0)?,
                album_count: r.get::<_, i64>(1)? as u32,
                track_count: r.get::<_, i64>(2)? as u32,
            })
        },
    )
}

pub(crate) fn load_folders(conn: &Connection) -> Result<Vec<Folder>> {
    query_collect(
        conn,
        "SELECT folder, COUNT(*), COALESCE(SUM(duration_ms),0) FROM track \
         GROUP BY folder ORDER BY folder COLLATE NOCASE",
        [],
        row_to_folder,
    )
}

pub(crate) fn load_tracks_sorted(
    conn: &Connection,
    order: &str,
    limit: Option<usize>,
) -> Result<Vec<Track>> {
    let lim = match limit {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let sql = format!("SELECT {} FROM track ORDER BY {order}{lim}", db::TRACK_COLS);
    query_collect(conn, &sql, [], db::row_to_track)
}

/// Fetch tracks by id, preserving the order of `ids` (used for ranked hits).
pub(crate) fn load_tracks_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<Track>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {} FROM track WHERE track.id IN ({})",
        db::TRACK_COLS,
        placeholders(ids.len())
    );
    let bind: Vec<&dyn ToSql> = ids.iter().map(|i| i as &dyn ToSql).collect();
    let mut by_id: HashMap<i64, Track> = HashMap::new();
    for t in query_collect(conn, &sql, bind.as_slice(), db::row_to_track)? {
        by_id.insert(t.id, t);
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Build the tracks haystack: `title artist album album_artist` per track id.
pub(crate) fn load_track_hays(conn: &Connection) -> Result<Vec<Hay>> {
    query_collect(
        conn,
        "SELECT id, TRIM(COALESCE(title,'')||' '||COALESCE(artist,'')||' '|| \
             COALESCE(album,'')||' '||COALESCE(album_artist,'')) FROM track",
        [],
        |r| {
            Ok(Hay {
                id: r.get(0)?,
                text: r.get(1)?,
            })
        },
    )
}
