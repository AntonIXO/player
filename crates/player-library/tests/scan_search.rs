//! End-to-end: generate tagged FLAC fixtures with ffmpeg, then exercise scan,
//! incremental refresh (update / move / delete), and accent-folded search.
//! Skips cleanly if ffmpeg is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use player_library::{Filter, Library, Sort};

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

    let mut lib = Library::open(&db, &art).unwrap();

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

    // FTS5 accent folding: "part" matches "Pärt"
    let r = lib.search_grouped("part", Filter::All).unwrap();
    assert!(
        r.tracks.iter().any(|t| t.artist.as_deref() == Some("Arvo Pärt")),
        "accent-folded FTS finds Pärt via 'part'"
    );
    // and the accented form works too
    assert!(!lib.search_grouped("pärt", Filter::All).unwrap().tracks.is_empty());

    // grouped: searching the album name surfaces the album group
    let r = lib.search_grouped("alina", Filter::All).unwrap();
    assert!(r.albums.iter().any(|al| al.album == "Alina"));

    // filter chip: Albums filter narrows the match to the album column
    let r = lib.search_grouped("blue", Filter::Albums).unwrap();
    assert!(r.albums.is_empty(), "Kind of Blue was deleted");

    // nucleo typeahead, accent-insensitive
    let hits = lib.search_typeahead("part", 10);
    assert!(hits.iter().any(|t| t.artist.as_deref() == Some("Arvo Pärt")));
    // empty query returns the library (capped)
    assert_eq!(lib.search_typeahead("", 10).len(), 2);

    let _ = std::fs::remove_dir_all(&root);
}
