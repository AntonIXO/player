//! SQLite schema + connection setup. One `track` table, an external-content
//! FTS5 index kept in sync by triggers, and a `meta` key/value table. Album art
//! is cached as files keyed by blake3 hash (see `scan`/`Library::art_path`), so
//! no blobs live in the database.

use rusqlite::Connection;
use std::path::Path;

use crate::Result;

pub const SCHEMA_VERSION: i64 = 1;

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
    // Wait rather than fail when the writer thread holds the file briefly.
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v < 1 {
        conn.execute_batch(SCHEMA_V1)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

const SCHEMA_V1: &str = r#"
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
  mtime_ns      INTEGER NOT NULL,
  size          INTEGER NOT NULL
);
CREATE INDEX track_folder_idx ON track(folder);
CREATE INDEX track_album_idx  ON track(album, album_artist);

CREATE VIRTUAL TABLE track_fts USING fts5(
  title, artist, album_artist, album, composer, genre, folder,
  content='track', content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER track_ai AFTER INSERT ON track BEGIN
  INSERT INTO track_fts(rowid, title, artist, album_artist, album, composer, genre, folder)
  VALUES (new.id, new.title, new.artist, new.album_artist, new.album, new.composer, new.genre, new.folder);
END;
CREATE TRIGGER track_ad AFTER DELETE ON track BEGIN
  INSERT INTO track_fts(track_fts, rowid, title, artist, album_artist, album, composer, genre, folder)
  VALUES ('delete', old.id, old.title, old.artist, old.album_artist, old.album, old.composer, old.genre, old.folder);
END;
CREATE TRIGGER track_au AFTER UPDATE ON track BEGIN
  INSERT INTO track_fts(track_fts, rowid, title, artist, album_artist, album, composer, genre, folder)
  VALUES ('delete', old.id, old.title, old.artist, old.album_artist, old.album, old.composer, old.genre, old.folder);
  INSERT INTO track_fts(rowid, title, artist, album_artist, album, composer, genre, folder)
  VALUES (new.id, new.title, new.artist, new.album_artist, new.album, new.composer, new.genre, new.folder);
END;

CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);
"#;

/// Column list shared by the row-building queries (keeps SELECTs in sync with
/// `row_to_track`). Table-qualified so it is unambiguous when joined against
/// `track_fts`, whose columns share these names.
pub const TRACK_COLS: &str = "track.id, track.path, track.folder, track.title, track.artist, \
     track.album_artist, track.album, track.composer, track.genre, track.track_no, track.disc_no, \
     track.year, track.duration_ms, track.codec, track.sample_rate, track.bits, track.channels, \
     track.art_hash";

/// Build a `Track` from a row selected with [`TRACK_COLS`] (in order).
pub fn row_to_track(r: &rusqlite::Row) -> rusqlite::Result<crate::model::Track> {
    let path: String = r.get(1)?;
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
    })
}
