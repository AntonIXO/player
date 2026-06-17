//! SQLite schema + connection setup. One `track` table plus a `meta` key/value
//! table. Search is served entirely from an in-memory fuzzy index built off
//! this table (see [`crate::SearchIndex`]) — there is no FTS5 virtual table, so
//! scans pay no per-row index-maintenance cost. Album art is cached as files
//! keyed by blake3 hash (see `scan`/`Library::art_path`), so no blobs live in
//! the database.

use rusqlite::Connection;
use std::path::Path;

use crate::Result;

pub const SCHEMA_VERSION: i64 = 6;

/// Open (creating if needed) a connection with the library pragmas applied and
/// the schema migrated to the current version.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Read-latency tuning for the typeahead workload on a memory-constrained
    // phone: memory-map up to 128 MB of the DB (skip the read() syscall + page
    // copy), keep an ~8 MB page cache, and build temp B-trees (sorts/GROUP BY
    // during index build) in RAM rather than spilling to disk.
    conn.pragma_update(None, "mmap_size", 128i64 * 1024 * 1024)?;
    conn.pragma_update(None, "cache_size", -8_000i64)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // Wait rather than fail when the writer thread holds the file briefly.
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v < 1 {
        conn.execute_batch(SCHEMA_BASE)?;
    }
    // v1 databases were created with an FTS5 mirror + sync triggers; the fuzzy
    // index replaced them, so drop them on upgrade (no data migration needed —
    // the index is rebuilt from `track` at runtime).
    if v == 1 {
        conn.execute_batch(DROP_FTS_V1)?;
    }
    // v3 adds the recently-played history table (additive — no track rebuild).
    if v < 3 {
        conn.execute_batch(SCHEMA_PLAY_HISTORY)?;
    }
    // v4 adds the .cue columns (additive). `add_column_if_missing` makes this a
    // no-op on a fresh database, whose `SCHEMA_BASE` already declares them.
    if v < 4 {
        add_column_if_missing(conn, "track", "source_path", "TEXT")?;
        add_column_if_missing(conn, "track", "start_ms", "INTEGER")?;
    }
    // v5 adds the loved-tracks table (additive — no track rebuild).
    if v < 5 {
        conn.execute_batch(SCHEMA_LOVED)?;
    }
    // v6 adds an expression index on the artist key the artists list / artist
    // detail / stats all group and filter on, turning their full-table scan into
    // an index lookup (additive — `IF NOT EXISTS` so a fresh DB is unaffected).
    if v < 6 {
        conn.execute_batch(SCHEMA_ARTIST_IDX)?;
    }
    if v < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

/// `ALTER TABLE ... ADD COLUMN` only if the column isn't already present (SQLite
/// has no `IF NOT EXISTS` for columns), so it is safe on both fresh and upgraded
/// databases.
fn add_column_if_missing(conn: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == col);
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
    }
    Ok(())
}

const SCHEMA_BASE: &str = r"
CREATE TABLE track (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,
  folder        TEXT NOT NULL,
  title         TEXT,
  artist        TEXT,
  album_artist  TEXT,
  album         TEXT,
  composer      TEXT,
  genre         TEXT,
  track_no      INTEGER,
  disc_no       INTEGER,
  year          INTEGER,
  duration_ms   INTEGER,
  codec         TEXT,
  sample_rate   INTEGER,
  bits          INTEGER,
  channels      INTEGER,
  art_hash      TEXT,
  source_path   TEXT,
  start_ms      INTEGER,
  mtime_ns      INTEGER NOT NULL,
  size          INTEGER NOT NULL
);
CREATE INDEX track_folder_idx ON track(folder);
CREATE INDEX track_album_idx  ON track(album, album_artist);
CREATE INDEX track_artist_key_idx ON track(COALESCE(NULLIF(album_artist,''), artist));

CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);

CREATE TABLE play_history (
  track_id  INTEGER PRIMARY KEY,
  played_at INTEGER NOT NULL
);

CREATE TABLE loved_tracks (
  track_id  INTEGER PRIMARY KEY,
  loved_at  INTEGER NOT NULL
);
";

/// Loved (favourite) tracks (one row per track). Added in v5; `IF NOT EXISTS` so a
/// fresh database (which already created it via `SCHEMA_BASE`) is unaffected.
const SCHEMA_LOVED: &str = r"
CREATE TABLE IF NOT EXISTS loved_tracks (
  track_id  INTEGER PRIMARY KEY,
  loved_at  INTEGER NOT NULL
);
";

/// Expression index on the artist key (`COALESCE(NULLIF(album_artist,''), artist)`)
/// the artists list, artist detail, and stats group/filter on. Added in v6;
/// `IF NOT EXISTS` so a fresh database (which already created it via `SCHEMA_BASE`)
/// is unaffected.
const SCHEMA_ARTIST_IDX: &str = r"
CREATE INDEX IF NOT EXISTS track_artist_key_idx
  ON track(COALESCE(NULLIF(album_artist,''), artist));
";

/// Recently-played history (one row per track, newest play wins). Added in v3;
/// `IF NOT EXISTS` so a fresh database (which already created it via
/// `SCHEMA_BASE`) is unaffected.
const SCHEMA_PLAY_HISTORY: &str = r"
CREATE TABLE IF NOT EXISTS play_history (
  track_id  INTEGER PRIMARY KEY,
  played_at INTEGER NOT NULL
);
";

/// Drop the legacy FTS5 mirror + triggers when upgrading a v1 database.
const DROP_FTS_V1: &str = r"
DROP TRIGGER IF EXISTS track_ai;
DROP TRIGGER IF EXISTS track_ad;
DROP TRIGGER IF EXISTS track_au;
DROP TABLE IF EXISTS track_fts;
";

/// Column list shared by the row-building queries (keeps SELECTs in sync with
/// `row_to_track`). Table-qualified so it is unambiguous when joined against
/// `track_fts`, whose columns share these names.
pub const TRACK_COLS: &str = "track.id, track.path, track.folder, track.title, track.artist, \
     track.album_artist, track.album, track.composer, track.genre, track.track_no, track.disc_no, \
     track.year, track.duration_ms, track.codec, track.sample_rate, track.bits, track.channels, \
     track.art_hash, track.source_path, track.start_ms, \
     EXISTS(SELECT 1 FROM loved_tracks WHERE loved_tracks.track_id = track.id)";

/// Build a `Track` from a row selected with [`TRACK_COLS`] (in order).
pub fn row_to_track(r: &rusqlite::Row) -> rusqlite::Result<crate::model::Track> {
    let path: String = r.get(1)?;
    // Whole-file tracks store NULL source_path; fall back to `path` so the engine
    // always has a real file to decode.
    let source_path = r
        .get::<_, Option<String>>(18)?.map_or_else(|| path.clone().into(), Into::into);
    Ok(crate::model::Track {
        id: r.get(0)?,
        path: path.into(),
        folder: r.get(2)?,
        title: r.get(3)?,
        artist: r.get(4)?,
        album_artist: r.get(5)?,
        album: r.get(6)?,
        composer: r.get(7)?,
        genre: r.get(8)?,
        track_no: r.get(9)?,
        disc_no: r.get(10)?,
        year: r.get(11)?,
        duration_ms: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        codec: r.get(13)?,
        sample_rate: r.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        bits: r.get::<_, Option<i64>>(15)?.map(|v| v as u32),
        channels: r.get::<_, Option<i64>>(16)?.map(|v| v as u32),
        art_hash: r.get(17)?,
        source_path,
        start_ms: r.get::<_, Option<i64>>(19)?.map(|v| v as u64),
        loved: r.get(20)?,
    })
}
