// SPDX-License-Identifier: GPL-3.0-or-later
//! DST decoder verification against a real DST-compressed SACD (`rr.iso`,
//! git-ignored at the workspace root). Skips when absent.
//!
//! The arithmetic decoder is self-validating: its flush must yield exactly 1, or
//! `decode` returns an error. So decoding many frames cleanly — each yielding the
//! exact DSD64-stereo frame size — is strong evidence the port is bit-correct.

use std::path::PathBuf;

use sacd::{Area, SacdImage};

fn rr() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../rr.iso");
    p.exists().then_some(p)
}

#[test]
fn decodes_dst_frames() {
    let Some(path) = rr() else {
        eprintln!("rr.iso not present — skipping DST verification");
        return;
    };

    let img = SacdImage::open(&path).expect("open DST SACD image");
    println!("album : {:?} / {:?}", img.album_artist(), img.album_title());

    let stereo = img
        .areas()
        .iter()
        .find(|a| a.area == Area::Stereo)
        .expect("stereo area");
    assert!(
        stereo.tracks.iter().all(|t| t.dst),
        "this fixture is expected to be DST-compressed"
    );
    println!(
        "stereo area: {} ch, {} track(s), DST",
        stereo.channels,
        stereo.tracks.len()
    );

    let mut reader = img.reader(Area::Stereo, 0).expect("open track 0");
    let mut total = 0usize;
    let mut nframes = 0usize;
    let mut buf = Vec::new();
    while nframes < 200 {
        buf.clear();
        // A decode error (e.g. arithmetic-coder flush mismatch) surfaces here.
        if !reader.next(&mut buf).expect("DST decode") {
            break;
        }
        assert!(!buf.is_empty(), "decoded frame must be non-empty");
        total += buf.len();
        nframes += 1;
    }

    assert!(nframes >= 200, "should decode plenty of DST frames");
    let avg = total as f64 / nframes as f64;
    println!("decoded {nframes} DST frames, {total} bytes, avg {avg:.0} B/frame");
    // 2-channel DSD64 frame = 2822400/75 * 2 / 8 = 9408 bytes.
    assert!(
        (avg - 9408.0).abs() < 1.0,
        "each DST frame must decode to exactly 9408 bytes (got {avg:.1})"
    );
}
