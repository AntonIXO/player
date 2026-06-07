// SPDX-License-Identifier: GPL-3.0-or-later
//! Integration test against a real SACD image at the workspace root
//! (`pillow.iso`, git-ignored). Skips cleanly when the file is absent, so CI and
//! other machines still pass.

use std::path::PathBuf;

use sacd::{Area, SacdImage};

fn pillow() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pillow.iso");
    p.exists().then_some(p)
}

#[test]
fn parses_pillow_toc() {
    let Some(path) = pillow() else {
        eprintln!("pillow.iso not present — skipping");
        return;
    };

    let img = SacdImage::open(&path).expect("open SACD image");
    println!("album : {:?}", img.album_title());
    println!("artist: {:?}", img.album_artist());

    assert!(!img.areas().is_empty(), "must find at least one area");

    for area in img.areas() {
        println!(
            "\n== {:?} area: {} ch, {} Hz, {} track(s) ==",
            area.area,
            area.channels,
            area.dsd_rate,
            area.tracks.len()
        );
        for (i, t) in area.tracks.iter().enumerate() {
            println!(
                "  {:>2}. {:<40} {:<24} {:>6.1}s  {}",
                i + 1,
                t.title.as_deref().unwrap_or("(untitled)"),
                t.artist.as_deref().unwrap_or(""),
                t.duration.as_secs_f64(),
                if t.dst { "DST" } else { "DSD" },
            );
        }
        assert!(!area.tracks.is_empty(), "area should have tracks");
    }

    // The stereo area, if present, must report 2 channels.
    if let Some(stereo) = img.areas().iter().find(|a| a.area == Area::Stereo) {
        assert_eq!(stereo.channels, 2);
    }
}

#[test]
fn reads_dsd_frames_at_the_right_rate() {
    let Some(path) = pillow() else {
        eprintln!("pillow.iso not present — skipping");
        return;
    };
    let img = SacdImage::open(&path).unwrap();
    if img.areas().iter().all(|a| a.tracks.iter().any(|t| t.dst)) {
        eprintln!("pillow.iso is DST-compressed — frame-rate check needs the DST decoder; skipping");
        return;
    }

    let mut reader = img.reader(Area::Stereo, 0).expect("open track 0");
    // Read ~150 frames (~2 s of audio).
    let mut total = 0usize;
    let mut nframes = 0usize;
    let mut buf = Vec::new();
    while nframes < 150 {
        buf.clear();
        if !reader.next(&mut buf).expect("read frame") {
            break;
        }
        assert!(!buf.is_empty(), "frame must be non-empty");
        total += buf.len();
        nframes += 1;
    }

    assert!(nframes >= 150, "track should have plenty of frames");
    assert_eq!(total % 2, 0, "stereo byte-interleaved data is even-length");
    // SACD audio frames are 1/75 s: 2822400/75 * 2ch / 8 = 9408 bytes/frame.
    let avg = total as f64 / nframes as f64;
    println!("read {nframes} frames, {total} bytes, avg {avg:.0} B/frame");
    assert!(
        (9000.0..=9500.0).contains(&avg),
        "avg frame size {avg:.0} should be ~9408 bytes (2ch DSD64 @ 75 fps)"
    );
}

#[test]
fn selects_a_non_zero_track() {
    // Exercises the `set_track` LSN math for track > 0 (uses track_start_lsn[i],
    // a different code path than track 0) — the multi-track browse case.
    let Some(path) = pillow() else {
        eprintln!("pillow.iso not present — skipping");
        return;
    };
    let img = SacdImage::open(&path).unwrap();
    let stereo = img.areas().iter().find(|a| a.area == Area::Stereo).unwrap();
    if stereo.tracks.iter().any(|t| t.dst) {
        return; // DST disc — needs the decoder
    }
    let track = 3usize.min(stereo.tracks.len() - 1);

    let mut reader = img.reader(Area::Stereo, track).expect("open middle track");
    let mut buf = Vec::new();
    let mut nframes = 0;
    while nframes < 30 {
        buf.clear();
        if !reader.next(&mut buf).expect("read frame") {
            break;
        }
        assert!(!buf.is_empty());
        assert_eq!(buf.len() % 2, 0);
        nframes += 1;
    }
    assert!(nframes >= 30, "middle track {track} should read frames");
}
