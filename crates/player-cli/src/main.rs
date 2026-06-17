//! player-cli — exercises the bit-perfect engine and proves correctness.
//!
//!   probe           inspect a file (no device touched)
//!   play            decode and play to an ALSA hw: device
//!   dump            write decoded full-scale s32le (compare against ffmpeg)
//!   loopback-verify play through snd-aloop, capture, byte-compare (transport)
//!
//! `main` only parses the CLI and dispatches; each subcommand lives in [`cmd`].

mod cmd;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Default playback device when `--device` is omitted (a USB DAC at `hw:1,0` on
/// most boxes; pass `--device` or use `devices` to find the right one).
const DEFAULT_DEVICE: &str = "hw:1,0";
/// snd-aloop playback / capture endpoints for the loopback verifiers.
const LOOPBACK_OUT: &str = "hw:Loopback,0,0";
const LOOPBACK_IN: &str = "hw:Loopback,1,0";

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
        #[arg(long, default_value = DEFAULT_DEVICE)]
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
        #[arg(long, default_value = LOOPBACK_OUT)]
        out: String,
        #[arg(long = "in", default_value = LOOPBACK_IN)]
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
        #[arg(long, default_value = DEFAULT_DEVICE)]
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
        #[arg(long, default_value = LOOPBACK_OUT)]
        out: String,
        #[arg(long = "in", default_value = LOOPBACK_IN)]
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
        Cmd::Probe { file } => cmd::audio::probe(&file),
        Cmd::Play {
            file,
            device,
            seconds,
        } => cmd::audio::play(&file, &device, seconds),
        Cmd::Dump {
            file,
            out,
            start,
            seconds,
        } => cmd::audio::dump(&file, out.as_deref(), start, seconds),
        Cmd::LoopbackVerify {
            file,
            out,
            input,
            seconds,
        } => return cmd::loopback::loopback_verify(&file, &out, &input, seconds),
        Cmd::PlayQueue { files, device } => cmd::audio::play_queue(&files, &device),
        Cmd::LoopbackVerifyQueue { files, out, input } => {
            return cmd::loopback::loopback_verify_queue(&files, &out, &input)
        }
        Cmd::Scan { root, db, force } => {
            cmd::library::lib_scan(&root, db, force).map_err(cmd::library::to_core_err)
        }
        Cmd::Search { query, db, filter } => {
            cmd::library::lib_search(query, db, &filter).map_err(cmd::library::to_core_err)
        }
        Cmd::LibraryStats { db } => {
            cmd::library::lib_stats(db).map_err(cmd::library::to_core_err)
        }
        Cmd::Devices => cmd::devices::devices(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
