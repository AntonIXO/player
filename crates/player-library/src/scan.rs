//! Scan = parallel walk + extract (rayon) feeding a single writer thread, which
//! applies an incremental diff against the existing index:
//!
//! * unchanged (mtime + size match)  → no work,
//! * new / changed                   → extract + upsert,
//! * gone                            → delete,
//! * a "new" path whose (mtime,size) matches a gone one → a **move** (keep the id).
//!
//! `mv` preserves mtime and size exactly, so move detection needs no content
//! hashing — keeping scans fast on large lossless libraries. Cover art is written
//! to `art_dir/<blake3-hex>` (deduped by hash), so no blobs flow through the DB.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{bounded, Receiver};
use ignore::WalkBuilder;
use rayon::prelude::*;
use rusqlite::{params, Transaction};

use crate::extract::{self, is_audio, Extracted};
use crate::model::{ScanProgress, ScanStats};
use crate::{db, Error, Result};

const BATCH: usize = 1000;

/// A SACD `.iso`'s `((mtime_ns, size), row_count)`, keyed by iso path.
type IsoState = ((i64, i64), u64);

#[derive(Clone, Copy)]
struct FileState {
    id: i64,
    mtime_ns: i64,
    size: i64,
}

/// A fully-extracted row headed for the writer (no art blob — only its hash).
struct Row {
    existing_id: Option<i64>,
    path: String,
    folder: String,
    mtime_ns: i64,
    size: i64,
    art_hash: Option<String>,
    e: Extracted,
}

enum Msg {
    Unchanged(i64),
    Upsert(Box<Row>),
    Failed,
}

/// Scan `root` into the database at `db_path`, caching art under `art_dir`.
///
/// `force` re-extracts every file even when `(mtime, size)` is unchanged — use it
/// to backfill data added by a newer extractor (e.g. folder-sidecar covers) into
/// an already-indexed library; the normal path skips unchanged files for speed.
pub fn scan(
    db_path: &Path,
    art_dir: &Path,
    root: &Path,
    force: bool,
    progress: impl Fn(ScanProgress) + Send + Sync,
) -> Result<ScanStats> {
    let start = Instant::now();
    std::fs::create_dir_all(art_dir)?;

    let existing = Arc::new(load_existing(db_path)?);

    // Cheap directory walk to collect candidate files; the heavy per-file work
    // is parallelized below. `.cue` sheets and SACD `.iso`s are container files,
    // each expanded into per-track rows separately.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut cue_paths: Vec<PathBuf> = Vec::new();
    let mut iso_paths: Vec<PathBuf> = Vec::new();
    for dent in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .build()
    {
        let Ok(d) = dent else { continue };
        if !d.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if is_audio(d.path()) {
            paths.push(d.path().to_path_buf());
        } else if has_ext(d.path(), "cue") {
            cue_paths.push(d.path().to_path_buf());
        } else if extract::is_sacd_iso(d.path()) {
            iso_paths.push(d.path().to_path_buf());
        }
    }

    // Parse the cue sheets up front so we can (a) skip the audio files they own
    // from standalone indexing — avoiding a duplicate whole-file track — and
    // (b) emit their per-track rows after the normal diff.
    let cues: Vec<(PathBuf, crate::cue::CueSheet)> = cue_paths
        .into_iter()
        .filter_map(|p| crate::cue::parse_file(&p).map(|s| (p, s)))
        .collect();
    let referenced = cue_referenced_files(&cues);
    paths.retain(|p| !referenced.contains(p));

    let total = paths.len() as u64;

    let (tx, rx) = bounded::<Msg>(2048);
    let writer = {
        let db_path = db_path.to_path_buf();
        let existing = existing.clone();
        std::thread::spawn(move || writer_loop(&db_path, rx, existing))
    };

    let counter = AtomicU64::new(0);
    paths.par_iter().for_each_with(tx, |tx, path| {
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        progress(ScanProgress {
            seen: n,
            total,
            path: path.clone(),
        });
        let msg = build_row(path, art_dir, &existing, force).unwrap_or(Msg::Failed);
        let _ = tx.send(msg);
    });
    // all senders dropped here → writer's recv loop ends.

    let mut stats = writer
        .join()
        .map_err(|_| Error::Other("scan writer thread panicked".into()))??;

    // Per-track rows for `.cue` sheets (incremental: a sheet whose file mtime+size
    // is unchanged is skipped; removed sheets have their rows deleted).
    scan_cues(db_path, art_dir, &cues, &mut stats)?;
    // Per-track rows for SACD `.iso` images (same incremental contract).
    scan_iso(db_path, art_dir, &iso_paths, &mut stats)?;

    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false)
}

/// The set of audio files referenced by any cue sheet (resolved against each
/// sheet's directory), so they aren't also indexed as standalone tracks.
fn cue_referenced_files(cues: &[(PathBuf, crate::cue::CueSheet)]) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    for (cue_path, sheet) in cues {
        let Some(dir) = cue_path.parent() else { continue };
        for f in &sheet.files {
            set.insert(dir.join(&f.name));
        }
    }
    set
}

fn load_existing(db_path: &Path) -> Result<HashMap<String, FileState>> {
    let conn = db::open(db_path)?;
    // Whole-file tracks only. Container-derived rows are diffed separately:
    // `.cue` rows have `start_ms NOT NULL`; SACD `.iso` rows have `start_ms NULL`
    // but `source_path NOT NULL`. Whole-file rows alone have `source_path NULL`.
    let mut stmt = conn.prepare(
        "SELECT id, path, mtime_ns, size FROM track \
         WHERE start_ms IS NULL AND source_path IS NULL",
    )?;
    let mut map = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            FileState {
                id: r.get(0)?,
                mtime_ns: r.get(2)?,
                size: r.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (k, v) = row?;
        map.insert(k, v);
    }
    Ok(map)
}

fn build_row(
    path: &Path,
    art_dir: &Path,
    existing: &HashMap<String, FileState>,
    force: bool,
) -> Result<Msg> {
    let meta = std::fs::metadata(path)?;
    let mtime_ns = mtime_ns(&meta);
    let size = meta.len() as i64;
    let key = path.to_string_lossy().into_owned();

    let existing_id = match existing.get(&key) {
        Some(fs) if !force && fs.mtime_ns == mtime_ns && fs.size == size => {
            return Ok(Msg::Unchanged(fs.id));
        }
        Some(fs) => Some(fs.id),
        None => None,
    };

    let mut e = extract::extract(path)?;
    let mut art_hash = None;
    if let Some((hash, _mime, bytes)) = e.art.take() {
        let _ = write_art(art_dir, &hash, &bytes);
        art_hash = Some(hash);
    }
    let folder = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    Ok(Msg::Upsert(Box::new(Row {
        existing_id,
        path: key,
        folder,
        mtime_ns,
        size,
        art_hash,
        e,
    })))
}

fn writer_loop(
    db_path: &Path,
    rx: Receiver<Msg>,
    existing: Arc<HashMap<String, FileState>>,
) -> Result<ScanStats> {
    let mut conn = db::open(db_path)?;
    let mut stats = ScanStats::default();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut news: Vec<Box<Row>> = Vec::new();

    // Phase 1: drain. Updates to existing rows are applied immediately (batched);
    // brand-new rows are buffered for move reconciliation.
    {
        let mut count = 0usize;
        let mut txn = conn.transaction()?;
        for msg in rx.iter() {
            match msg {
                Msg::Failed => stats.errors += 1,
                Msg::Unchanged(id) => {
                    seen.insert(id);
                    stats.unchanged += 1;
                }
                Msg::Upsert(row) => match row.existing_id {
                    Some(id) => {
                        update_track(&txn, id, &row)?;
                        seen.insert(id);
                        stats.updated += 1;
                        count += 1;
                        if count >= BATCH {
                            txn.commit()?;
                            txn = conn.transaction()?;
                            count = 0;
                        }
                    }
                    None => news.push(row),
                },
            }
        }
        txn.commit()?;
    }

    // Phase 2: reconcile deletes + moves against everything that wasn't seen.
    let mut del_ids: HashSet<i64> = HashSet::new();
    let mut del_sig: HashMap<(i64, i64), i64> = HashMap::new();
    for fs in existing.values() {
        if !seen.contains(&fs.id) {
            del_ids.insert(fs.id);
            del_sig.entry((fs.mtime_ns, fs.size)).or_insert(fs.id);
        }
    }

    {
        let txn = conn.transaction()?;
        for row in &news {
            if let Some(&id) = del_sig.get(&(row.mtime_ns, row.size)) {
                if del_ids.remove(&id) {
                    del_sig.remove(&(row.mtime_ns, row.size));
                    update_track(&txn, id, row)?; // move: same content, new path
                    stats.moved += 1;
                    continue;
                }
            }
            insert_track(&txn, row)?;
            stats.added += 1;
        }
        for id in &del_ids {
            txn.execute("DELETE FROM track WHERE id = ?1", params![id])?;
            stats.removed += 1;
        }
        txn.commit()?;
    }

    Ok(stats)
}

fn insert_track(txn: &Transaction, row: &Row) -> Result<()> {
    let e = &row.e;
    txn.execute(
        "INSERT INTO track(path,folder,title,artist,album_artist,album,composer,genre,\
            track_no,disc_no,year,duration_ms,codec,sample_rate,bits,channels,art_hash,mtime_ns,size)\
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)\
         ON CONFLICT(path) DO UPDATE SET folder=excluded.folder,title=excluded.title,\
            artist=excluded.artist,album_artist=excluded.album_artist,album=excluded.album,\
            composer=excluded.composer,genre=excluded.genre,track_no=excluded.track_no,\
            disc_no=excluded.disc_no,year=excluded.year,duration_ms=excluded.duration_ms,\
            codec=excluded.codec,sample_rate=excluded.sample_rate,bits=excluded.bits,\
            channels=excluded.channels,art_hash=excluded.art_hash,mtime_ns=excluded.mtime_ns,\
            size=excluded.size",
        params![
            row.path, row.folder, e.title, e.artist, e.album_artist, e.album, e.composer,
            e.genre, e.track_no, e.disc_no, e.year, e.duration_ms.map(|v| v as i64), e.codec,
            e.sample_rate, e.bits, e.channels, row.art_hash, row.mtime_ns, row.size,
        ],
    )?;
    Ok(())
}

fn update_track(txn: &Transaction, id: i64, row: &Row) -> Result<()> {
    let e = &row.e;
    txn.execute(
        "UPDATE track SET path=?2,folder=?3,title=?4,artist=?5,album_artist=?6,album=?7,\
            composer=?8,genre=?9,track_no=?10,disc_no=?11,year=?12,duration_ms=?13,codec=?14,\
            sample_rate=?15,bits=?16,channels=?17,art_hash=?18,mtime_ns=?19,size=?20 WHERE id=?1",
        params![
            id, row.path, row.folder, e.title, e.artist, e.album_artist, e.album, e.composer,
            e.genre, e.track_no, e.disc_no, e.year, e.duration_ms.map(|v| v as i64), e.codec,
            e.sample_rate, e.bits, e.channels, row.art_hash, row.mtime_ns, row.size,
        ],
    )?;
    Ok(())
}

static ART_NONCE: AtomicU64 = AtomicU64::new(0);

/// Write a cover to `art_dir/<hash>` atomically, skipping if it already exists.
fn write_art(art_dir: &Path, hash: &str, bytes: &[u8]) -> std::io::Result<()> {
    let dst = art_dir.join(hash);
    if dst.exists() {
        return Ok(());
    }
    let nonce = ART_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = art_dir.join(format!(".{hash}.{nonce}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &dst)?;
    Ok(())
}

// --- .cue sheets ----------------------------------------------------------

/// Diff the parsed cue sheets against existing cue rows and (re)insert their
/// per-track rows. A sheet's rows carry the *cue file's* mtime+size, so an
/// unchanged sheet is skipped, a changed one is rebuilt, and a vanished one's
/// rows are deleted.
fn scan_cues(
    db_path: &Path,
    art_dir: &Path,
    cues: &[(PathBuf, crate::cue::CueSheet)],
    stats: &mut ScanStats,
) -> Result<()> {
    let mut conn = db::open(db_path)?;
    let existing = load_existing_cues(&conn)?;
    let mut current: HashSet<String> = HashSet::new();

    let txn = conn.transaction()?;
    for (cue_path, sheet) in cues {
        let key = cue_path.to_string_lossy().into_owned();
        current.insert(key.clone());
        let Ok(meta) = std::fs::metadata(cue_path) else {
            continue;
        };
        let sig = (mtime_ns(&meta), meta.len() as i64);
        if existing.get(&key) == Some(&sig) {
            stats.unchanged += sheet.files.iter().map(|f| f.tracks.len() as u64).sum::<u64>();
            continue;
        }
        delete_cue_rows(&txn, &key)?;
        stats.added += insert_cue_tracks(&txn, art_dir, cue_path, sheet, sig)?;
    }
    // Sheets that no longer exist on disk: drop their rows.
    for old in existing.keys() {
        if !current.contains(old) {
            stats.removed += delete_cue_rows(&txn, old)?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Map each existing cue's path to its (mtime, size) signature, recovered from
/// the synthetic `<cue>#<seq>` track paths.
fn load_existing_cues(conn: &rusqlite::Connection) -> Result<HashMap<String, (i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT path, mtime_ns, size FROM track WHERE start_ms IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, mtime, size) = row?;
        if let Some(key) = strip_cue_suffix(&path) {
            map.entry(key).or_insert((mtime, size));
        }
    }
    Ok(map)
}

/// Recover the owning cue path from a synthetic `<cue>#<seq>` track path.
fn strip_cue_suffix(path: &str) -> Option<String> {
    let hash = path.rfind('#')?;
    let suffix = &path[hash + 1..];
    (!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        .then(|| path[..hash].to_string())
}

/// Escape SQLite `LIKE` metacharacters (used with `ESCAPE '\'`).
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn delete_cue_rows(txn: &Transaction, cue_key: &str) -> Result<u64> {
    let pattern = format!("{}#%", escape_like(cue_key));
    let n = txn.execute(
        "DELETE FROM track WHERE path LIKE ?1 ESCAPE '\\'",
        params![pattern],
    )?;
    Ok(n as u64)
}

/// Insert one synthetic row per cue track. Each file is opened once for its wire
/// facts + cover; a track's duration is the gap to the next track's start (the
/// last in a file runs to the file's end).
fn insert_cue_tracks(
    txn: &Transaction,
    art_dir: &Path,
    cue_path: &Path,
    sheet: &crate::cue::CueSheet,
    sig: (i64, i64),
) -> Result<u64> {
    let key = cue_path.to_string_lossy().into_owned();
    let dir = cue_path.parent().unwrap_or_else(|| Path::new(""));
    let folder = dir.to_string_lossy().into_owned();
    let (mtime_ns, size) = sig;
    let mut seq: u32 = 0;
    let mut added = 0u64;

    for file in &sheet.files {
        let audio = dir.join(&file.name);
        if !audio.exists() {
            continue;
        }
        let Ok(mut e) = extract::extract(&audio) else {
            continue;
        };
        let mut art_hash = None;
        if let Some((hash, _mime, bytes)) = e.art.take() {
            let _ = write_art(art_dir, &hash, &bytes);
            art_hash = Some(hash);
        }
        let file_total = e.duration_ms.unwrap_or(0);
        let source = audio.to_string_lossy().into_owned();
        let n = file.tracks.len();
        for (i, tr) in file.tracks.iter().enumerate() {
            seq += 1;
            let start = tr.start_ms;
            let end = if i + 1 < n {
                file.tracks[i + 1].start_ms
            } else {
                file_total
            };
            let dur = end.saturating_sub(start) as i64;
            let path = format!("{key}#{seq:03}");
            let title = tr
                .title
                .clone()
                .unwrap_or_else(|| format!("Track {:02}", tr.number));
            let artist = tr.performer.clone().or_else(|| sheet.performer.clone());
            txn.execute(
                "INSERT INTO track(path,folder,title,artist,album_artist,album,composer,genre,\
                    track_no,disc_no,year,duration_ms,codec,sample_rate,bits,channels,art_hash,\
                    source_path,start_ms,mtime_ns,size)\
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)\
                 ON CONFLICT(path) DO UPDATE SET folder=excluded.folder,title=excluded.title,\
                    artist=excluded.artist,album_artist=excluded.album_artist,album=excluded.album,\
                    composer=excluded.composer,genre=excluded.genre,track_no=excluded.track_no,\
                    disc_no=excluded.disc_no,year=excluded.year,duration_ms=excluded.duration_ms,\
                    codec=excluded.codec,sample_rate=excluded.sample_rate,bits=excluded.bits,\
                    channels=excluded.channels,art_hash=excluded.art_hash,\
                    source_path=excluded.source_path,start_ms=excluded.start_ms,\
                    mtime_ns=excluded.mtime_ns,size=excluded.size",
                params![
                    path,
                    folder,
                    title,
                    artist,
                    sheet.performer,
                    sheet.title,
                    None::<String>,   // composer
                    sheet.genre,
                    tr.number,
                    None::<i64>,      // disc_no
                    sheet.date,
                    dur,
                    e.codec,
                    e.sample_rate,
                    e.bits,
                    e.channels,
                    art_hash,
                    source,
                    start as i64,
                    mtime_ns,
                    size,
                ],
            )?;
            added += 1;
        }
    }
    Ok(added)
}

// --- SACD .iso ------------------------------------------------------------

/// Diff SACD `.iso` images against existing iso-derived rows and (re)insert their
/// per-track rows. Mirrors [`scan_cues`]: an iso whose `(mtime, size)` is
/// unchanged is skipped, a changed one is rebuilt, a vanished one's rows dropped.
/// SACD rows are identified by `start_ms IS NULL AND source_path IS NOT NULL`.
fn scan_iso(
    db_path: &Path,
    art_dir: &Path,
    iso_paths: &[PathBuf],
    stats: &mut ScanStats,
) -> Result<()> {
    if iso_paths.is_empty() {
        return Ok(());
    }
    let mut conn = db::open(db_path)?;
    let existing = load_existing_iso(&conn)?;
    let mut current: HashSet<String> = HashSet::new();

    let txn = conn.transaction()?;
    for iso in iso_paths {
        let key = iso.to_string_lossy().into_owned();
        current.insert(key.clone());
        let Ok(meta) = std::fs::metadata(iso) else {
            continue;
        };
        let sig = (mtime_ns(&meta), meta.len() as i64);
        if let Some((old_sig, count)) = existing.get(&key) {
            if *old_sig == sig {
                stats.unchanged += count;
                continue;
            }
        }
        delete_cue_rows(&txn, &key)?; // generic: deletes `<key>#%`
        match extract::extract_sacd(iso) {
            Ok(album) => stats.added += insert_iso_tracks(&txn, art_dir, iso, &album, sig)?,
            Err(_) => stats.errors += 1,
        }
    }
    for old in existing.keys() {
        if !current.contains(old) {
            stats.removed += delete_cue_rows(&txn, old)?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Map each existing SACD `.iso` path to its `(mtime, size)` signature and row
/// count, recovered from the synthetic `<iso>#<seq>` track paths.
fn load_existing_iso(conn: &rusqlite::Connection) -> Result<HashMap<String, IsoState>> {
    let mut stmt = conn.prepare(
        "SELECT path, mtime_ns, size FROM track \
         WHERE start_ms IS NULL AND source_path IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut map: HashMap<String, IsoState> = HashMap::new();
    for row in rows {
        let (path, mtime, size) = row?;
        if let Some(key) = strip_cue_suffix(&path) {
            let e = map.entry(key).or_insert(((mtime, size), 0));
            e.0 = (mtime, size);
            e.1 += 1;
        }
    }
    Ok(map)
}

/// Insert one synthetic row per SACD track (`<iso>#NN`, `source_path` = the iso,
/// `start_ms` NULL). The album cover, if a sidecar sits beside the iso, is
/// cached like embedded art.
fn insert_iso_tracks(
    txn: &Transaction,
    art_dir: &Path,
    iso_path: &Path,
    album: &extract::SacdAlbum,
    sig: (i64, i64),
) -> Result<u64> {
    let key = iso_path.to_string_lossy().into_owned();
    let folder = iso_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (mtime_ns, size) = sig;
    let art_hash = extract::folder_cover(iso_path).map(|(hash, _mime, bytes)| {
        let _ = write_art(art_dir, &hash, &bytes);
        hash
    });

    let mut added = 0u64;
    for (i, t) in album.tracks.iter().enumerate() {
        let seq = (i + 1) as u32;
        let path = format!("{key}#{seq:03}");
        let title = t
            .title
            .clone()
            .unwrap_or_else(|| format!("Track {seq:02}"));
        let artist = t.artist.clone().or_else(|| album.album_artist.clone());
        txn.execute(
            "INSERT INTO track(path,folder,title,artist,album_artist,album,track_no,\
                duration_ms,codec,channels,art_hash,source_path,mtime_ns,size)\
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)\
             ON CONFLICT(path) DO UPDATE SET folder=excluded.folder,title=excluded.title,\
                artist=excluded.artist,album_artist=excluded.album_artist,album=excluded.album,\
                track_no=excluded.track_no,duration_ms=excluded.duration_ms,codec=excluded.codec,\
                channels=excluded.channels,art_hash=excluded.art_hash,\
                source_path=excluded.source_path,mtime_ns=excluded.mtime_ns,size=excluded.size",
            params![
                path,
                folder,
                title,
                artist,
                album.album_artist,
                album.album,
                seq,
                t.duration_ms as i64,
                "DSD64",
                album.channels,
                art_hash,
                key,
                mtime_ns,
                size,
            ],
        )?;
        added += 1;
    }
    Ok(added)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
