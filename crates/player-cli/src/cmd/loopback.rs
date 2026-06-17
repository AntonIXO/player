//! Loopback verification: play through snd-aloop, capture it back, and byte-
//! compare. This is the transport bit-perfect proof — keep it byte-pure (no
//! silence guards, no sample touching), it defines correctness.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use player_core::{
    capture_raw, play_queue_blocking, AlsaSink, Event, StreamSpec, DEFAULT_PERIOD, DEFAULT_PERIODS,
};

use crate::cmd::{decode_to_bytes, source_to_bytes};

pub fn loopback_verify(file: &Path, out_dev: &str, in_dev: &str, seconds: f64) -> ExitCode {
    let (played, spec, n_frames) = match source_to_bytes(file, out_dev, seconds) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let frame_bytes = spec.bytes_per_frame();
    println!(
        "loopback: {} frames, {} Hz, {} ch, {} -> capture {}",
        n_frames,
        spec.rate,
        spec.channels,
        spec.fmt.label(),
        in_dev
    );

    // Start the capture first so it is blocked in readi before playback begins;
    // snd-aloop then delivers from frame 0 with no head loss.
    let cap_dev = in_dev.to_string();
    let cap_handle =
        thread::spawn(move || capture_raw(&cap_dev, spec, n_frames, DEFAULT_PERIOD, DEFAULT_PERIODS));
    thread::sleep(Duration::from_millis(250));

    // Open playback and stream the pre-decoded bytes. Keep `sink` alive until
    // capture has drained the loopback (drop would cut off the tail).
    let sink = match AlsaSink::open(out_dev, spec, DEFAULT_PERIOD, DEFAULT_PERIODS) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("playback open error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = sink.write_all_bytes(&played).and_then(|()| sink.drain()) {
        eprintln!("playback error: {e}");
        return ExitCode::FAILURE;
    }

    let captured = match cap_handle.join() {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            eprintln!("capture error: {e}");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("capture thread panicked");
            return ExitCode::FAILURE;
        }
    };
    drop(sink);

    compare(&played, &captured, frame_bytes)
}

/// Gapless proof through snd-aloop: play the queue via the ring engine, capture
/// it back, and byte-compare against the concatenated decode.
pub fn loopback_verify_queue(files: &[PathBuf], out_dev: &str, in_dev: &str) -> ExitCode {
    // Build the reference (decode each track); require a single wire spec so the
    // capture reads one continuous stream at one rate.
    let mut reference: Vec<u8> = Vec::new();
    let mut spec0: Option<StreamSpec> = None;
    for f in files {
        let (bytes, spec, _n) = match decode_to_bytes(f, None) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("decode error ({}): {e}", f.display());
                return ExitCode::FAILURE;
            }
        };
        match spec0 {
            None => spec0 = Some(spec),
            Some(s0) if !s0.same_wire(&spec) => {
                eprintln!(
                    "FAIL: queue mixes wire formats ({} vs {}); use `play-queue` for a rate/format change.",
                    s0.fmt.label(),
                    spec.fmt.label()
                );
                return ExitCode::FAILURE;
            }
            _ => {}
        }
        reference.extend_from_slice(&bytes);
    }
    let spec = spec0.expect("clap guarantees >= 1 file");
    let frame_bytes = spec.bytes_per_frame();
    let n_frames = reference.len() / frame_bytes;
    println!(
        "loopback-queue: {} track(s), {} frames, {} Hz, {} ch, {} -> capture {}",
        files.len(),
        n_frames,
        spec.rate,
        spec.channels,
        spec.fmt.label(),
        in_dev
    );

    // Start capture (it blocks in readi), then drive the ring engine.
    let cap_dev = in_dev.to_string();
    let cap_handle =
        thread::spawn(move || capture_raw(&cap_dev, spec, n_frames, DEFAULT_PERIOD, DEFAULT_PERIODS));
    thread::sleep(Duration::from_millis(250));

    let play_res = play_queue_blocking(files, out_dev, DEFAULT_PERIOD, DEFAULT_PERIODS, |ev| {
        if let Event::Error(e) = ev {
            eprintln!("engine: {e}");
        }
    });
    let stats = match play_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("playback error: {e}");
            let _ = cap_handle.join();
            return ExitCode::FAILURE;
        }
    };

    let captured = match cap_handle.join() {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            eprintln!("capture error: {e}");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("capture thread panicked");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "engine: {} frames, {} xrun(s), scheduling {:?}",
        stats.frames, stats.xruns, stats.sched
    );

    compare(&reference, &captured, frame_bytes)
}

/// Find the frame offset at which `captured` aligns to `played` (snd-aloop should
/// give 0). Anchors on the first non-silent played frame and searches for it.
fn find_alignment(played: &[u8], captured: &[u8], frame_bytes: usize) -> Option<usize> {
    let pf = played.len() / frame_bytes;
    let cf = captured.len() / frame_bytes;

    let anchor = (0..pf)
        .find(|&f| played[f * frame_bytes..(f + 1) * frame_bytes].iter().any(|&b| b != 0))
        .unwrap_or(0);
    let window = (pf - anchor).min(8192);
    let pat = &played[anchor * frame_bytes..(anchor + window) * frame_bytes];

    let max_d = cf.saturating_sub(anchor + window);
    (0..=max_d).find(|&d| {
        let start = (anchor + d) * frame_bytes;
        &captured[start..start + pat.len()] == pat
    })
}

/// Count differing frames between `played` and `captured` shifted by `delay`,
/// returning `(diff_count, first_diff_frame, comparable_frames)`.
fn diff_frames(
    played: &[u8],
    captured: &[u8],
    frame_bytes: usize,
    delay: usize,
) -> (usize, Option<usize>, usize) {
    let pf = played.len() / frame_bytes;
    let comparable = pf.saturating_sub(delay);
    let mut diff = 0usize;
    let mut first_diff = None;
    for f in 0..comparable {
        let p = &played[f * frame_bytes..(f + 1) * frame_bytes];
        let c = &captured[(f + delay) * frame_bytes..(f + delay + 1) * frame_bytes];
        if p != c {
            if first_diff.is_none() {
                first_diff = Some(f);
            }
            diff += 1;
        }
    }
    (diff, first_diff, comparable)
}

/// Align captured to played (snd-aloop should give offset 0) and byte-compare.
fn compare(played: &[u8], captured: &[u8], frame_bytes: usize) -> ExitCode {
    let Some(d) = find_alignment(played, captured, frame_bytes) else {
        eprintln!("FAIL: could not align captured stream to played stream");
        return ExitCode::FAILURE;
    };

    let (diff, first_diff, comparable) = diff_frames(played, captured, frame_bytes, d);

    if diff == 0 {
        println!(
            "MATCH: {comparable} frames identical (alignment offset {d} frames). BIT-PERFECT ✓"
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "FAIL: {diff}/{comparable} frames differ (first at frame {first_diff:?}, offset {d})"
        );
        ExitCode::FAILURE
    }
}
