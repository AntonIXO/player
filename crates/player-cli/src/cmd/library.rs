//! Library-index subcommands: `scan`, `search`, `library-stats`. These return
//! `player_library::Result`; `main` maps the error into the CLI's error type via
//! [`to_core_err`].

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use player_library::{Filter, Library, SearchIndex};

/// Map a library error into the CLI's `player_core` error for uniform reporting.
pub fn to_core_err(e: player_library::Error) -> player_core::Error {
    player_core::Error::Unsupported(e.to_string())
}

/// Open the library at an explicit `--db` (art cached beside it) or the XDG default.
fn open_library(db: Option<PathBuf>) -> player_library::Result<Library> {
    match db {
        Some(p) => {
            let art = p
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("art");
            Library::open(&p, &art)
        }
        None => Library::open_default(),
    }
}

fn parse_filter(s: &str) -> Filter {
    match s.to_ascii_lowercase().as_str() {
        "tracks" => Filter::Tracks,
        "albums" => Filter::Albums,
        "artists" => Filter::Artists,
        _ => Filter::All,
    }
}

pub fn lib_scan(root: &Path, db: Option<PathBuf>, force: bool) -> player_library::Result<()> {
    let lib = open_library(db)?;
    println!("scanning {} {}…", root.display(), if force { "(force) " } else { "" });
    let stats = lib.scan_with_progress(root, force, |p| {
        if p.seen % 500 == 0 || p.seen == p.total {
            print!("\r  {} / {} files", p.seen, p.total);
            let _ = io::stdout().flush();
        }
    })?;
    println!(
        "\nadded {} · updated {} · moved {} · removed {} · unchanged {} · errors {}  ({} ms)",
        stats.added,
        stats.updated,
        stats.moved,
        stats.removed,
        stats.unchanged,
        stats.errors,
        stats.elapsed_ms
    );
    Ok(())
}

pub fn lib_search(query: Vec<String>, db: Option<PathBuf>, filter: &str) -> player_library::Result<()> {
    let lib = open_library(db)?;
    let idx = SearchIndex::build(&lib)?;
    let q = query.join(" ");
    let r = idx.query(&lib, &q, parse_filter(filter), 30)?;

    if !r.artists.is_empty() {
        println!("Artists");
        for a in &r.artists {
            println!(
                "  {}  [{} album{} · {} track{}]",
                a.name,
                a.album_count,
                if a.album_count == 1 { "" } else { "s" },
                a.track_count,
                if a.track_count == 1 { "" } else { "s" },
            );
        }
    }
    if !r.albums.is_empty() {
        println!("Albums");
        for a in &r.albums {
            println!(
                "  {} — {}  [{}]",
                a.album,
                a.album_artist.as_deref().unwrap_or("Unknown Artist"),
                a.meta()
            );
        }
    }
    if !r.folders.is_empty() {
        println!("Folders");
        for f in &r.folders {
            println!("  {}  [{}]", f.name(), f.meta());
        }
    }
    if !r.tracks.is_empty() {
        println!("Tracks");
        for t in &r.tracks {
            println!("  {} — {}  [{}]", t.display_title(), t.subtitle(), t.format_spec());
        }
    }
    Ok(())
}

pub fn lib_stats(db: Option<PathBuf>) -> player_library::Result<()> {
    let s = open_library(db)?.stats()?;
    println!("tracks  : {}", s.tracks);
    println!("albums  : {}", s.albums);
    println!("artists : {}", s.artists);
    println!("folders : {}", s.folders);
    Ok(())
}
