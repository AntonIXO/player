//! End-to-end: generate tagged FLAC fixtures with ffmpeg, then exercise scan,
//! incremental refresh (update / move / delete), and accent-folded search.
//! Skips cleanly if ffmpeg is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use player_library::{Filter, Library, SearchIndex, Sort};

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn gen(path: &Path, title: &str, artist: &str, album: &str, dur: f32) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={dur}"))
        .args(["-ar", "44100", "-sample_fmt", "s16"])
        .args(["-metadata", &format!("title={title}")])
        .args(["-metadata", &format!("artist={artist}")])
        .args(["-metadata", &format!("album={album}")])
        .args(["-metadata", &format!("album_artist={artist}")])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed for {}", path.display());
}

/// Like `gen` but also stamps a release `date` (year), for sort-order tests.
fn gen_dated(path: &Path, title: &str, artist: &str, album: &str, year: i32) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=1")
        .args(["-ar", "44100", "-sample_fmt", "s16"])
        .args(["-metadata", &format!("title={title}")])
        .args(["-metadata", &format!("artist={artist}")])
        .args(["-metadata", &format!("album={album}")])
        .args(["-metadata", &format!("album_artist={artist}")])
        .args(["-metadata", &format!("date={year}")])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed for {}", path.display());
}

fn unique_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "player-lib-test-{}-{}",
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
fn cue_sheet_splits_into_tracks() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not found — skipping cue integration test");
        return;
    }

    let root = unique_dir();
    let music = root.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let db = root.join("library.db");
    let art = root.join("art");

    // One 3s audio file split by a cue into two tracks (0:00 and 0:01).
    let flac = music.join("album.flac");
    gen(&flac, "whole file", "Embedded Artist", "Embedded Album", 3.0);
    let cue = music.join("album.cue");
    std::fs::write(
        &cue,
        "PERFORMER \"Test Artist\"\n\
         TITLE \"Test Album\"\n\
         FILE \"album.flac\" WAVE\n\
         \x20\x20TRACK 01 AUDIO\n\
         \x20\x20\x20\x20TITLE \"First\"\n\
         \x20\x20\x20\x20INDEX 01 00:00:00\n\
         \x20\x20TRACK 02 AUDIO\n\
         \x20\x20\x20\x20TITLE \"Second\"\n\
         \x20\x20\x20\x20INDEX 01 00:01:00\n",
    )
    .unwrap();

    let lib = Library::open(&db, &art).unwrap();

    // Initial scan: two cue tracks added, the whole file is NOT indexed standalone.
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.added, 2, "two cue tracks added");
    assert_eq!(lib.stats().unwrap().tracks, 2, "only cue tracks (not the file)");

    let tracks = lib.album_tracks("Test Album", Some("Test Artist")).unwrap();
    assert_eq!(tracks.len(), 2, "cue album has two tracks");

    let first = tracks.iter().find(|t| t.title.as_deref() == Some("First")).unwrap();
    let second = tracks.iter().find(|t| t.title.as_deref() == Some("Second")).unwrap();

    // Offsets + the source file the engine will decode.
    assert_eq!(first.start_ms, Some(0));
    assert_eq!(first.duration_ms, Some(1000), "track 1 = gap to track 2's start");
    assert_eq!(second.start_ms, Some(1000));
    assert!(second.duration_ms.unwrap() >= 1500, "track 2 runs to file end");
    assert!(
        first.source_path.as_ref().unwrap_or(&first.path).ends_with("album.flac") && second.source_path.as_ref().unwrap_or(&second.path).ends_with("album.flac"),
        "cue tracks decode the referenced file"
    );
    assert!(first.cue_range().is_some(), "cue track exposes a decode range");

    // Re-scan with no change → the unchanged cue is skipped.
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.added, 0);
    assert_eq!(s.unchanged, 2, "unchanged cue contributes its track count");

    // Editing the cue (drop track 2) rebuilds it down to a single track.
    std::fs::write(
        &cue,
        "PERFORMER \"Test Artist\"\n\
         TITLE \"Test Album\"\n\
         FILE \"album.flac\" WAVE\n\
         \x20\x20TRACK 01 AUDIO\n\
         \x20\x20\x20\x20TITLE \"First\"\n\
         \x20\x20\x20\x20INDEX 01 00:00:00\n",
    )
    .unwrap();
    lib.scan(&music).unwrap();
    assert_eq!(lib.stats().unwrap().tracks, 1, "cue rebuilt to one track");

    // Removing the cue drops its rows; the file is no longer "owned" by a cue,
    // so it reappears as a single whole-file track.
    std::fs::remove_file(&cue).unwrap();
    lib.scan(&music).unwrap();
    let all = lib.tracks(Sort::Title).unwrap();
    assert_eq!(all.len(), 1, "cue gone → file indexed as one standalone track");
    assert_eq!(all[0].start_ms, None, "standalone track has no cue offset");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scan_incremental_and_search() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not found — skipping scan_search integration test");
        return;
    }

    let root = unique_dir();
    let music = root.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let db = root.join("library.db");
    let art = root.join("art");

    let a = music.join("a.flac");
    let b = music.join("b.flac");
    let c = music.join("c.flac");
    gen(&a, "Spiegel im Spiegel", "Arvo Pärt", "Alina", 1.0);
    gen(&b, "Für Alina", "Arvo Pärt", "Alina", 1.0);
    gen(&c, "So What", "Miles Davis", "Kind of Blue", 1.0);

    let lib = Library::open(&db, &art).unwrap();

    // initial scan
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.added, 3, "added");
    assert_eq!(s.unchanged, 0);
    let st = lib.stats().unwrap();
    assert_eq!(st.tracks, 3, "tracks");
    assert_eq!(st.albums, 2, "albums (Alina + Kind of Blue)");
    assert_eq!(st.artists, 2, "artists");

    // album browse + album_tracks ordering
    assert_eq!(lib.albums(Sort::Title).unwrap().len(), 2);
    let alina = lib.album_tracks("Alina", Some("Arvo Pärt")).unwrap();
    assert_eq!(alina.len(), 2, "Alina has two tracks");

    // re-scan with no changes → everything unchanged
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.added, 0);
    assert_eq!(s.unchanged, 3, "all unchanged on re-scan");

    // modify one file (new content → new mtime+size) → exactly one updated
    gen(&a, "Spiegel im Spiegel", "Arvo Pärt", "Alina", 2.0);
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.updated, 1, "one updated");
    assert_eq!(s.unchanged, 2);
    assert_eq!(s.added, 0);

    // rename preserves mtime+size → detected as a move, not delete+add
    let b2 = music.join("b2.flac");
    std::fs::rename(&b, &b2).unwrap();
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.moved, 1, "rename detected as move");
    assert_eq!(s.removed, 0);
    assert_eq!(s.added, 0);
    assert_eq!(lib.stats().unwrap().tracks, 3, "move keeps the count");

    // delete one file → one removed
    std::fs::remove_file(&c).unwrap();
    let s = lib.scan(&music).unwrap();
    assert_eq!(s.removed, 1, "one removed");
    assert_eq!(lib.stats().unwrap().tracks, 2);

    // unified fuzzy search, served from the in-memory index (built off the DB)
    let idx = SearchIndex::build(&lib).unwrap();

    // accent folding: ascii "part" fuzzy-matches "Pärt" (nucleo Smart normalize)
    let r = idx.query(&lib, "part", Filter::All, 20).unwrap();
    assert!(
        r.tracks.iter().any(|t| t.artist.as_deref() == Some("Arvo Pärt")),
        "ascii 'part' fuzzy-matches Pärt"
    );
    // a single All query fills artists + albums + tracks together
    assert!(
        r.artists.iter().any(|a| a.name == "Arvo Pärt"),
        "artist group surfaces Pärt"
    );
    // and the accented form works too
    assert!(!idx.query(&lib, "pärt", Filter::All, 20).unwrap().tracks.is_empty());

    // searching the album name surfaces the album group
    let r = idx.query(&lib, "alina", Filter::All, 20).unwrap();
    assert!(r.albums.iter().any(|al| al.album == "Alina"));

    // filter scoping: Albums filter returns only the albums group
    let r = idx.query(&lib, "part", Filter::Albums, 20).unwrap();
    assert!(
        r.tracks.is_empty() && r.artists.is_empty() && r.folders.is_empty(),
        "Albums filter scopes to the albums group only"
    );

    // empty query returns the library (capped) for the in-scope group
    assert_eq!(idx.query(&lib, "", Filter::Tracks, 10).unwrap().tracks.len(), 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tracks_by_paths_resolves_batch() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not found — skipping tracks_by_paths integration test");
        return;
    }

    let root = unique_dir();
    let music = root.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let db = root.join("library.db");
    let art = root.join("art");

    let a = music.join("a.flac");
    let b = music.join("b.flac");
    gen(&a, "Spiegel im Spiegel", "Arvo Pärt", "Alina", 1.0);
    gen(&b, "Für Alina", "Arvo Pärt", "Alina", 1.0);

    let lib = Library::open(&db, &art).unwrap();
    lib.scan(&music).unwrap();

    let missing = music.join("gone.flac");
    // One query resolves the indexed paths; the unindexed one is simply absent.
    let map = lib.tracks_by_paths(&[a.clone(), b.clone(), missing.clone()]).unwrap();
    assert_eq!(map.len(), 2, "both indexed paths resolved, missing one absent");
    assert!(!map.contains_key(&missing));
    assert_eq!(map.get(&a).and_then(|t| t.title.clone()).as_deref(), Some("Spiegel im Spiegel"));
    assert_eq!(map.get(&b).and_then(|t| t.title.clone()).as_deref(), Some("Für Alina"));

    // The map lets the caller preserve its own arbitrary order (b before a here).
    let order = [b, a];
    let titles: Vec<_> = order
        .iter()
        .filter_map(|p| map.get(p).and_then(|t| t.title.clone()))
        .collect();
    assert_eq!(titles, vec!["Für Alina".to_string(), "Spiegel im Spiegel".to_string()]);

    assert!(lib.tracks_by_paths(&[]).unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

/// Characterization: `tracks_by_paths` must resolve correctly even when the input
/// list is larger than the SQLite bind-variable chunk size, i.e. when the query is
/// split across multiple chunks. Guards the chunking refactor (off-by-one in the
/// chunk size would silently drop rows that land past the first chunk).
#[test]
fn tracks_by_paths_crosses_chunk_boundary() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not found — skipping chunk-boundary test");
        return;
    }

    let root = unique_dir();
    let music = root.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let db = root.join("library.db");
    let art = root.join("art");

    // Two real, indexed files; one placed well past the chunk boundary.
    let a = music.join("a.flac");
    let z = music.join("z.flac");
    gen(&a, "First", "Artist", "Album", 1.0);
    gen(&z, "Last", "Artist", "Album", 1.0);

    let lib = Library::open(&db, &art).unwrap();
    lib.scan(&music).unwrap();

    // 1500 paths total (> 900 chunk size, forcing ≥2 chunks): the real ones are at
    // index 0 and 1400, padded with non-existent paths in between.
    let mut paths: Vec<PathBuf> = Vec::with_capacity(1500);
    paths.push(a.clone());
    for i in 0..1398 {
        paths.push(music.join(format!("ghost-{i}.flac")));
    }
    paths.push(z.clone());
    paths.push(music.join("also-missing.flac"));

    let map = lib.tracks_by_paths(&paths).unwrap();
    assert_eq!(map.len(), 2, "both real paths resolve across the chunk boundary");
    assert_eq!(map.get(&a).and_then(|t| t.title.clone()).as_deref(), Some("First"));
    assert_eq!(
        map.get(&z).and_then(|t| t.title.clone()).as_deref(),
        Some("Last"),
        "a real path in the SECOND chunk still resolves"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Characterization: pin the browse sort orders (`album_order` / `track_order`) so
/// the upcoming extraction into a `browse` module is provably behaviour-preserving.
#[test]
fn browse_sort_orders() {
    if !ffmpeg_ok() {
        eprintln!("ffmpeg not found — skipping sort-order test");
        return;
    }

    let root = unique_dir();
    let music = root.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let db = root.join("library.db");
    let art = root.join("art");

    // Three single-track albums with distinct artist / album / year so each Sort
    // mode yields a different, unambiguous order.
    gen_dated(&music.join("blue.flac"), "So What", "Miles Davis", "Kind of Blue", 1959);
    gen_dated(&music.join("alina.flac"), "Für Alina", "Arvo Pärt", "Alina", 1976);
    gen_dated(&music.join("ok.flac"), "Airbag", "Radiohead", "OK Computer", 1997);

    let lib = Library::open(&db, &art).unwrap();
    lib.scan(&music).unwrap();

    // Newest first; ties (none here) aside, Year is a strict 1997 > 1976 > 1959.
    let by_year: Vec<_> = lib.albums(Sort::Year).unwrap().into_iter().map(|a| a.album).collect();
    assert_eq!(by_year, ["OK Computer", "Alina", "Kind of Blue"], "Year = newest first");

    // Album-title sort is lexicographic.
    let by_album: Vec<_> = lib.albums(Sort::Album).unwrap().into_iter().map(|a| a.album).collect();
    assert_eq!(by_album, ["Alina", "Kind of Blue", "OK Computer"], "Album = A→Z by title");

    // Artist sort orders by album_artist (Pärt < Miles? case/locale: 'A' < 'M' < 'R').
    let by_artist: Vec<_> = lib
        .albums(Sort::Artist)
        .unwrap()
        .into_iter()
        .map(|a| a.album_artist.unwrap_or_default())
        .collect();
    assert_eq!(by_artist, ["Arvo Pärt", "Miles Davis", "Radiohead"], "Artist = A→Z by album_artist");

    // Track-level Title sort is lexicographic across all tracks.
    let track_titles: Vec<_> = lib
        .tracks(Sort::Title)
        .unwrap()
        .into_iter()
        .filter_map(|t| t.title)
        .collect();
    assert_eq!(track_titles, ["Airbag", "Für Alina", "So What"], "Title = A→Z across tracks");

    let _ = std::fs::remove_dir_all(&root);
}
