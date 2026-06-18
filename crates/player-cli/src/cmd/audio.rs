//! Playback / decode subcommands: `probe`, `play`, `dump`, `play-queue`. These
//! touch the engine's *oracle* path (`run_playback` / `play_queue_blocking`) and
//! the unguarded CLI dump — deliberately byte-pure, NOT the safe daily Mojo 2
//! path (see CLAUDE.md). Don't add silence guards here.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use player_core::{
    is_dsd_path, open_dsd, play_queue_blocking, run_playback, AlsaFmt, Decoder, DsdSource, Event,
    Flow, DEFAULT_PERIOD, DEFAULT_PERIODS,
};

use crate::cmd::max_frames;

pub fn probe(file: &Path) -> player_core::Result<()> {
    if is_dsd_path(file) {
        return probe_dsd(file);
    }
    let dec = Decoder::open(file)?;
    let s = dec.spec;
    let dur = s
        .rate
        .checked_into_secs(dec.n_frames).map_or_else(|| "unknown".into(), |d| format!("{d:.1} s"));
    println!("file        : {}", file.display());
    println!("codec       : {}", dec.codec_name);
    println!("sample rate : {} Hz", s.rate);
    println!("channels    : {}", s.channels);
    println!("source bits : {}", s.source_bits);
    println!("alsa format : {} ({} bytes/sample)", s.fmt.label(), s.fmt.bytes_per_sample());
    println!(
        "frames      : {}",
        dec.n_frames.map_or_else(|| "unknown".into(), |n| n.to_string())
    );
    println!("duration    : {dur}");
    Ok(())
}

/// Probe a DSD file (`.dsf`/`.dff`/`.iso`): report the DSD rate and the
/// bit-perfect DoP output it maps to.
fn probe_dsd(file: &Path) -> player_core::Result<()> {
    let src = open_dsd(file, None)?;
    let s = src.spec();
    let dop = s.dop_spec(AlsaFmt::S32);
    println!("file        : {}", file.display());
    println!("codec       : {} (DSD, 1-bit)", s.label());
    println!("dsd rate    : {} Hz", s.dsd_rate);
    println!("channels    : {}", s.channels);
    println!(
        "dop output  : {} Hz, S24_3LE/S32_LE (DSD-over-PCM) — bit-perfect",
        dop.rate
    );
    Ok(())
}

pub fn play(file: &Path, device: &str, seconds: f64) -> player_core::Result<()> {
    if is_dsd_path(file) {
        return play_dsd(file, device);
    }
    let probe_spec = Decoder::open(file)?.spec;
    let maxf = max_frames(probe_spec.rate, seconds);

    println!(
        "playing {} -> {device}  [{} Hz, {} ch, {}]",
        file.display(),
        probe_spec.rate,
        probe_spec.channels,
        probe_spec.fmt.label()
    );

    let mut last_sec = u64::MAX;
    let spec = run_playback(
        file,
        device,
        maxf,
        DEFAULT_PERIOD,
        DEFAULT_PERIODS,
        |frames| {
            let sec = frames / u64::from(probe_spec.rate);
            if sec != last_sec {
                last_sec = sec;
                print!("\r  {sec:>4} s ");
                let _ = io::stdout().flush();
            }
            Flow::Continue
        },
    )?;
    println!("\ndone (negotiated {} Hz {}).", spec.rate, spec.fmt.label());
    Ok(())
}

/// Play a DSD file (`.dsf`/`.dff`/`.iso`) as bit-perfect DoP through the gapless
/// engine. (`seconds` is ignored for DSD — pick a short fixture for quick tests.)
fn play_dsd(file: &Path, device: &str) -> player_core::Result<()> {
    println!("playing (DSD → DoP) {} -> {device}", file.display());
    let stats = play_queue_blocking(
        &[file.to_path_buf()],
        device,
        DEFAULT_PERIOD,
        DEFAULT_PERIODS,
        |ev| match ev {
            Event::Started { spec, .. } => println!(
                "  DoP {} Hz, {} ch, {} — bit-perfect",
                spec.rate,
                spec.channels,
                spec.fmt.label()
            ),
            Event::Error(e) => eprintln!("⚠ {e}"),
            Event::Ended | Event::Position(_) => {}
        },
    )?;
    println!(
        "\ndone: {} frames, {} xrun(s), scheduling {:?}.",
        stats.frames, stats.xruns, stats.sched
    );
    Ok(())
}

pub fn dump(
    file: &Path,
    out: Option<&Path>,
    start: f64,
    seconds: f64,
    rewind: Option<f64>,
) -> player_core::Result<()> {
    let mut sink: Box<dyn Write> = match out {
        Some(p) => Box::new(io::BufWriter::new(std::fs::File::create(p)?)),
        None => Box::new(io::BufWriter::new(io::stdout().lock())),
    };
    // DSD: dump raw native DSD bytes (interleaved, MSB-first) — comparable to a
    // reference DSD extraction (e.g. for DST-decode verification). `--start`
    // seeks to a frame boundary (best-effort) and `--seconds` caps the output so
    // a huge SACD .iso can be bounded; neither alters a single bit.
    if is_dsd_path(file) {
        let mut src = open_dsd(file, None)?;
        let dspec = src.spec();
        let dop_rate = f64::from(dspec.dsd_rate / 16); // DoP PCM frame rate
        // 1 DoP frame carries 16 DSD bits/ch = 2 bytes/ch of native DSD.
        let cap_bytes = (seconds > 0.0)
            .then(|| (seconds * dop_rate) as u64 * 2 * u64::from(dspec.channels.max(1)));
        if start > 0.0 {
            // Seek is best-effort: sources that can't seek return None — then we
            // just stream from the top (still bounded by `--seconds`).
            let _ = src.seek((start * dop_rate) as u64)?;
        }
        dsd_pass(&mut *src, &mut *sink, cap_bytes)?;
        // Optional rewind: seek BACK and decode again — exercises the SACD .iso
        // sparse frame→sector backward-seek index (or the .dsf/.dff byte rewind).
        if let Some(r) = rewind {
            let _ = src.seek((r * dop_rate) as u64)?;
            dsd_pass(&mut *src, &mut *sink, cap_bytes)?;
        }
        sink.flush()?;
        return Ok(());
    }
    let mut dec = Decoder::open(file)?;
    let limit = (seconds > 0.0).then(|| (seconds * f64::from(dec.spec.rate)) as u64);
    // Seek first (resets decoder + sample buffer), then arm the frame limit so
    // `next()` stops after exactly `seconds` of audio from the seek point.
    if start > 0.0 {
        dec.seek(Duration::from_secs_f64(start))?;
    }
    if let Some(l) = limit {
        dec.set_limit(l);
    }
    pcm_pass(&mut dec, &mut *sink)?;
    if let Some(r) = rewind {
        dec.seek(Duration::from_secs_f64(r))?;
        if let Some(l) = limit {
            dec.set_limit(l); // re-arm: set_limit resets the per-pass frame counter
        }
        pcm_pass(&mut dec, &mut *sink)?;
    }
    sink.flush()?;
    Ok(())
}

/// One bounded DSD decode pass from the source's current position. `cap_bytes`
/// limits native-DSD bytes written (None = to EOF). Pure: writes only what it
/// decodes, so a `--start`/`--rewind` pass never alters a sample.
fn dsd_pass(
    src: &mut dyn DsdSource,
    sink: &mut dyn Write,
    cap_bytes: Option<u64>,
) -> player_core::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut written: u64 = 0;
    while src.next(&mut buf)? {
        let mut chunk: &[u8] = &buf;
        if let Some(cap) = cap_bytes {
            let remaining = cap.saturating_sub(written) as usize;
            if remaining == 0 {
                break;
            }
            if chunk.len() > remaining {
                chunk = &chunk[..remaining];
            }
        }
        sink.write_all(chunk)?;
        written += chunk.len() as u64;
    }
    Ok(())
}

/// One PCM decode pass from the decoder's current position (re-arm the frame
/// limit via `set_limit` before calling for a bounded pass). Writes interleaved
/// full-scale s32le.
fn pcm_pass(dec: &mut Decoder, sink: &mut dyn Write) -> player_core::Result<()> {
    let mut block: Vec<i32> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    while dec.next(&mut block)? {
        buf.clear();
        for &s in &block {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        sink.write_all(&buf)?;
    }
    Ok(())
}

/// Play a queue through the real-time gapless engine (Phase 2).
pub fn play_queue(files: &[PathBuf], device: &str) -> player_core::Result<()> {
    println!("queue: {} track(s) -> {device}", files.len());
    let stats = play_queue_blocking(
        files,
        device,
        DEFAULT_PERIOD,
        DEFAULT_PERIODS,
        |ev| match ev {
            Event::Started { spec, path } => println!(
                "▶ {}  [{} Hz, {} ch, {}-bit → {}]",
                path.display(),
                spec.rate,
                spec.channels,
                spec.source_bits,
                spec.fmt.label()
            ),
            Event::Ended => println!("■ queue ended"),
            Event::Error(e) => eprintln!("⚠ {e}"),
            Event::Position(_) => {}
        },
    )?;
    println!(
        "done: {} frames, {} xrun(s), scheduling {:?}",
        stats.frames, stats.xruns, stats.sched
    );
    Ok(())
}

/// Small helper to format duration from frame count.
trait SecsExt {
    fn checked_into_secs(self, frames: Option<u64>) -> Option<f64>;
}
impl SecsExt for u32 {
    fn checked_into_secs(self, frames: Option<u64>) -> Option<f64> {
        frames.map(|n| n as f64 / f64::from(self))
    }
}
