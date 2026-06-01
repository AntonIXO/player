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
    // is parallelized below.
    let mut paths: Vec<PathBuf> = Vec::new();
    for dent in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .build()
    {
        let Ok(d) = dent else { continue };
        if d.file_type().map(|t| t.is_file()).unwrap_or(false) && is_audio(d.path()) {
            paths.push(d.path().to_path_buf());
        }
    }
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
    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

fn load_existing(db_path: &Path) -> Result<HashMap<String, FileState>> {
    let conn = db::open(db_path)?;
    let mut stmt = conn.prepare("SELECT id, path, mtime_ns, size FROM track")?;
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

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
