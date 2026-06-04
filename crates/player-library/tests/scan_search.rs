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
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        first.source_path.ends_with("album.flac") && second.source_path.ends_with("album.flac"),
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
