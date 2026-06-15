//! player-cli — exercises the bit-perfect engine and proves correctness.
//!
//!   probe           inspect a file (no device touched)
//!   play            decode and play to an ALSA hw: device
//!   dump            write decoded full-scale s32le (compare against ffmpeg)
//!   loopback-verify play through snd-aloop, capture, byte-compare (transport)

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use player_core::{
    append_bytes, capture_raw, is_dsd_path, open_dsd, play_queue_blocking, probe_formats,
    run_playback, AlsaFmt, AlsaSink, Decoder, DeviceFormats, DopPacker, Event, Flow, Packer,
    StreamSpec, DEFAULT_PERIOD, DEFAULT_PERIODS,
};
use player_library::{Filter, Library, SearchIndex};

#[derive(Parser)]
#[command(name = "player-cli", about = "bit-perfect ALSA player core CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print codec/rate/format info without opening any device.
    Probe { file: PathBuf },

    /// Decode and play to an ALSA device (default: hw:1,0).
    Play {
        file: PathBuf,
        #[arg(long, default_value = "hw:1,0")]
        device: String,
        /// Stop after N seconds (0 = whole file).
        #[arg(long, default_value_t = 0.0)]
        seconds: f64,
    },

    /// Write decoded interleaved full-scale s32le to a file (or stdout).
    /// Compare with: ffmpeg -i FILE -f s32le -ac 2 -
    Dump {
        file: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Seek to this offset (seconds) before decoding. 0 = from the start.
        /// Exercises the seek/rewind path; for DSD it lands on a frame boundary.
        #[arg(long, default_value_t = 0.0)]
        start: f64,
        /// Decode at most this many seconds (0 = whole file). Bounds expensive
        /// decodes — e.g. a large SACD .iso — without altering any sample.
        #[arg(long, default_value_t = 0.0)]
        seconds: f64,
    },

    /// Play through snd-aloop and capture it back, then byte-compare.
    LoopbackVerify {
        file: PathBuf,
        #[arg(long, default_value = "hw:Loopback,0,0")]
        out: String,
        #[arg(long = "in", default_value = "hw:Loopback,1,0")]
        input: String,
        /// Limit to N seconds (loopback runs in real time). 0 = whole file.
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
    },

    /// Play a queue through the real-time gapless engine (Phase 2).
    /// Same-format tracks stream gaplessly; a rate/format change drains+reopens.
    PlayQueue {
        /// One or more files, played in order.
        #[arg(required = true, num_args = 1..)]
        files: Vec<PathBuf>,
        #[arg(long, default_value = "hw:1,0")]
        device: String,
    },

    /// Gapless proof: play a queue through the ring engine into snd-aloop,
    /// capture it back, and byte-compare against the concatenated decode. A
    /// MATCH means zero samples were inserted/dropped at track boundaries.
    /// All files must share the same wire format (the gapless case); use
    /// `play-queue` to exercise a rate/format change.
    LoopbackVerifyQueue {
        #[arg(required = true, num_args = 1..)]
        files: Vec<PathBuf>,
        #[arg(long, default_value = "hw:Loopback,0,0")]
        out: String,
        #[arg(long = "in", default_value = "hw:Loopback,1,0")]
        input: String,
    },

    /// Scan a music folder into the library index (incremental).
    Scan {
        /// Folder to scan recursively.
        root: PathBuf,
        /// Database path (default: $XDG_DATA_HOME/player/library.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Re-extract every file even if unchanged (backfills new data such as
        /// folder-sidecar covers into an already-indexed library).
        #[arg(long)]
        force: bool,
    },

    /// Search the library index (unified fuzzy: artists + albums + tracks).
    Search {
        /// Query terms.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long)]
        db: Option<PathBuf>,
        /// Scope the results: all | tracks | albums | artists.
        #[arg(long, default_value = "all")]
        filter: String,
    },

    /// Print library counts (tracks / albums / artists / folders).
    LibraryStats {
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// List bit-perfect ALSA output devices (USB DACs first).
    Devices,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Probe { file } => probe(&file),
        Cmd::Play {
            file,
            device,
            seconds,
        } => play(&file, &device, seconds),
        Cmd::Dump {
            file,
            out,
            start,
            seconds,
        } => dump(&file, out.as_deref(), start, seconds),
        Cmd::LoopbackVerify {
            file,
            out,
            input,
            seconds,
        } => return loopback_verify(&file, &out, &input, seconds),
        Cmd::PlayQueue { files, device } => play_queue(&files, &device),
        Cmd::LoopbackVerifyQueue { files, out, input } => {
            return loopback_verify_queue(&files, &out, &input)
        }
        Cmd::Scan { root, db, force } => lib_scan(&root, db, force).map_err(to_core_err),
        Cmd::Search { query, db, filter } => lib_search(query, db, &filter).map_err(to_core_err),
        Cmd::LibraryStats { db } => lib_stats(db).map_err(to_core_err),
        Cmd::Devices => devices(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn max_frames(spec_rate: u32, seconds: f64) -> Option<u64> {
    if seconds > 0.0 {
        Some((seconds * spec_rate as f64) as u64)
    } else {
        None
    }
}

fn devices() -> player_core::Result<()> {
    let devices = player_core::list_devices();
    if devices.is_empty() {
        println!("(no bit-perfect hw: output devices found)");
        return Ok(());
    }
    let pick = player_core::auto_pick().map(|d| d.id);
    for d in &devices {
        let kind = match d.kind {
            player_core::DeviceKind::Usb => "USB DAC",
            player_core::DeviceKind::Internal => "internal",
            player_core::DeviceKind::Other => "other",
        };
        let marker = if pick.as_deref() == Some(d.id.as_str()) {
            " *"
        } else {
            "  "
        };
        println!("{marker} {:<28} [{kind}]  {}", d.id, d.name);
        if !d.description.is_empty() && d.description != d.id {
            for line in d.description.lines() {
                println!("       {line}");
            }
        }
    }
    println!("\n  * = auto-pick default");
    Ok(())
}

fn probe(file: &Path) -> player_core::Result<()> {
    if is_dsd_path(file) {
        return probe_dsd(file);
    }
    let dec = Decoder::open(file)?;
    let s = dec.spec;
    let dur = s
        .rate
        .checked_into_secs(dec.n_frames)
        .map(|d| format!("{d:.1} s"))
        .unwrap_or_else(|| "unknown".into());
    println!("file        : {}", file.display());
    println!("codec       : {}", dec.codec_name);
    println!("sample rate : {} Hz", s.rate);
    println!("channels    : {}", s.channels);
    println!("source bits : {}", s.source_bits);
    println!("alsa format : {} ({} bytes/sample)", s.fmt.label(), s.fmt.bytes_per_sample() );
    println!("frames      : {}", dec.n_frames.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into()));
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

fn play(file: &Path, device: &str, seconds: f64) -> player_core::Result<()> {
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
            let sec = frames / probe_spec.rate as u64;
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

fn dump(file: &Path, out: Option<&Path>, start: f64, seconds: f64) -> player_core::Result<()> {
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
        let dop_rate = (dspec.dsd_rate / 16) as f64; // DoP PCM frame rate
        if start > 0.0 {
            // Seek is best-effort: sources that can't seek return None — then we
            // just stream from the top (still bounded by `--seconds`).
            let _ = src.seek((start * dop_rate) as u64)?;
        }
        // 1 DoP frame carries 16 DSD bits/ch = 2 bytes/ch of native DSD.
        let cap_bytes = (seconds > 0.0)
            .then(|| (seconds * dop_rate) as u64 * 2 * dspec.channels.max(1) as u64);
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
        sink.flush()?;
        return Ok(());
    }
    let mut dec = Decoder::open(file)?;
    // Seek first (resets decoder + sample buffer), then arm the frame limit so
    // `next()` stops after exactly `seconds` of audio from the seek point.
    if start > 0.0 {
        dec.seek(Duration::from_secs_f64(start))?;
    }
    if seconds > 0.0 {
        dec.set_limit((seconds * dec.spec.rate as f64) as u64);
    }
    let mut block: Vec<i32> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    while dec.next(&mut block)? {
        buf.clear();
        for &s in &block {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        sink.write_all(&buf)?;
    }
    sink.flush()?;
    Ok(())
}

/// Decode (up to `max_frames`) into the exact bytes the sink would write.
fn decode_to_bytes(
    file: &Path,
    maxf: Option<u64>,
) -> player_core::Result<(Vec<u8>, StreamSpec, usize)> {
    let mut dec = Decoder::open(file)?;
    let spec = dec.spec;
    let channels = spec.channels as usize;
    let mut packer = Packer::new(spec.fmt);
    let mut block: Vec<i32> = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let mut frames: u64 = 0;

    while dec.next(&mut block)? {
        let mut samples = block.as_slice();
        if let Some(m) = maxf {
            if frames >= m {
                break;
            }
            let remaining = (m - frames) as usize * channels;
            if samples.len() > remaining {
                samples = &samples[..remaining];
            }
        }
        if samples.is_empty() {
            continue;
        }
        let out = packer.pack(samples);
        append_bytes(&mut bytes, &out);
        frames += (samples.len() / channels) as u64;
    }
    let n_frames = bytes.len() / spec.bytes_per_frame();
    Ok((bytes, spec, n_frames))
}

/// Build the bit-perfect DoP byte stream for a DSD file, choosing the DoP
/// container from `device`'s formats. The DSD analogue of [`decode_to_bytes`]:
/// the bytes are exactly what the sink writes, so the loopback compare is a
/// transport bit-perfect proof for DSD.
fn dsd_dop_bytes(
    file: &Path,
    device: &str,
    seconds: f64,
) -> player_core::Result<(Vec<u8>, StreamSpec, usize)> {
    let formats = probe_formats(device).unwrap_or_else(|_| DeviceFormats::all());
    let fmt = formats.choose(24).ok_or_else(|| {
        player_core::Error::Unsupported("device can't carry DoP (no 24/32-bit format)".into())
    })?;
    let mut src = open_dsd(file, None)?;
    let dspec = src.spec();
    let spec = dspec.dop_spec(fmt);
    let frame_bytes = spec.bytes_per_frame();
    let maxf = max_frames(spec.rate, seconds);
    let mut dop = DopPacker::new(fmt, dspec.channels);
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    while src.next(&mut buf)? {
        out.extend_from_slice(dop.pack(&buf));
        if let Some(m) = maxf {
            if out.len() / frame_bytes >= m as usize {
                out.truncate(m as usize * frame_bytes);
                break;
            }
        }
    }
    let n = out.len() / frame_bytes;
    Ok((out, spec, n))
}

fn loopback_verify(file: &Path, out_dev: &str, in_dev: &str, seconds: f64) -> ExitCode {
    let built = if is_dsd_path(file) {
        dsd_dop_bytes(file, out_dev, seconds)
    } else {
        match Decoder::open(file) {
            Ok(d) => decode_to_bytes(file, max_frames(d.spec.rate, seconds)),
            Err(e) => Err(e),
        }
    };
    let (played, spec, n_frames) = match built {
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
    let cap_handle = thread::spawn(move || {
        capture_raw(&cap_dev, spec, n_frames, DEFAULT_PERIOD, DEFAULT_PERIODS)
    });
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

/// Align captured to played (snd-aloop should give offset 0) and byte-compare.
fn compare(played: &[u8], captured: &[u8], frame_bytes: usize) -> ExitCode {
    let pf = played.len() / frame_bytes;
    let cf = captured.len() / frame_bytes;

    // Anchor the search on the first non-silent played frame.
    let anchor = (0..pf)
        .find(|&f| played[f * frame_bytes..(f + 1) * frame_bytes].iter().any(|&b| b != 0))
        .unwrap_or(0);
    let window = (pf - anchor).min(8192);
    let pat = &played[anchor * frame_bytes..(anchor + window) * frame_bytes];

    let max_d = cf.saturating_sub(anchor + window);
    let mut delay = None;
    for d in 0..=max_d {
        let start = (anchor + d) * frame_bytes;
        if &captured[start..start + pat.len()] == pat {
            delay = Some(d);
            break;
        }
    }

    let Some(d) = delay else {
        eprintln!("FAIL: could not align captured stream to played stream");
        return ExitCode::FAILURE;
    };

    let comparable = pf.saturating_sub(d);
    let mut diff = 0usize;
    let mut first_diff = None;
    for f in 0..comparable {
        let p = &played[f * frame_bytes..(f + 1) * frame_bytes];
        let c = &captured[(f + d) * frame_bytes..(f + d + 1) * frame_bytes];
        if p != c {
            if first_diff.is_none() {
                first_diff = Some(f);
            }
            diff += 1;
        }
    }

    if diff == 0 {
        println!(
            "MATCH: {comparable} frames identical (alignment offset {d} frames). BIT-PERFECT ✓"
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "FAIL: {diff}/{comparable} frames differ (first at frame {:?}, offset {d})",
            first_diff
        );
        ExitCode::FAILURE
    }
}

/// Play a queue through the real-time gapless engine (Phase 2).
fn play_queue(files: &[PathBuf], device: &str) -> player_core::Result<()> {
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

/// Gapless proof through snd-aloop: play the queue via the ring engine, capture
/// it back, and byte-compare against the concatenated decode.
fn loopback_verify_queue(files: &[PathBuf], out_dev: &str, in_dev: &str) -> ExitCode {
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
    let cap_handle = thread::spawn(move || {
        capture_raw(&cap_dev, spec, n_frames, DEFAULT_PERIOD, DEFAULT_PERIODS)
    });
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

/// Small helper to format duration from frame count.
trait SecsExt {
    fn checked_into_secs(self, frames: Option<u64>) -> Option<f64>;
}
impl SecsExt for u32 {
    fn checked_into_secs(self, frames: Option<u64>) -> Option<f64> {
        frames.map(|n| n as f64 / self as f64)
    }
}

// ---------------------------------------------------------------------------
// Library index (player-library).
// ---------------------------------------------------------------------------

fn to_core_err(e: player_library::Error) -> player_core::Error {
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

fn lib_scan(root: &Path, db: Option<PathBuf>, force: bool) -> player_library::Result<()> {
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

fn lib_search(query: Vec<String>, db: Option<PathBuf>, filter: &str) -> player_library::Result<()> {
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

fn lib_stats(db: Option<PathBuf>) -> player_library::Result<()> {
    let s = open_library(db)?.stats()?;
    println!("tracks  : {}", s.tracks);
    println!("albums  : {}", s.albums);
    println!("artists : {}", s.artists);
    println!("folders : {}", s.folders);
    Ok(())
}
