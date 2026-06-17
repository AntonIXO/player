//! Engine. Two playback paths share the decode/convert/pack building blocks:
//!
//! * [`run_playback`] — the v1 single-thread decode → pack → `writei` loop, kept
//!   verbatim for the CLI `play`/`dump` paths and as the bit-perfect oracle.
//! * The Phase 2 **real-time gapless engine** — a decode thread and a dedicated
//!   `SCHED_FIFO` audio thread joined by a wait-free SPSC byte ring. Exposed as
//!   [`play_queue_blocking`] (play a fixed queue to completion) and the
//!   interactive [`Player`] (commands in, events out, for the GTK shell).
//!
//! Bit-perfect invariant: the ring carries already-packed bytes and the audio
//! thread writes them unchanged, so per-segment output is byte-identical to v1.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Sender};
use rtrb::Consumer;

use crate::convert::Packer;
use crate::decode::Decoder;
use crate::error::Result;
use crate::format::{DeviceFormats, StreamSpec};
use crate::rt::Sched;
use crate::sink::{probe_formats, AlsaSink};

mod audio_thread;
mod decode_thread;
mod ring;

use decode_thread::{build_producer, SegmentWriter};

// ---------------------------------------------------------------------------
// v1 single-shot path (unchanged).
// ---------------------------------------------------------------------------

/// Returned by the per-block callback to control the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Default ALSA buffering (robustness over latency): 1024-frame periods, 8
/// periods (~186 ms at 44.1 kHz).
pub const DEFAULT_PERIOD: i64 = 1024;
pub const DEFAULT_PERIODS: i64 = 8;

/// Decode `path` and play it bit-perfect to `device`.
///
/// * `max_frames` — optional cap (for short test runs).
/// * `tick` — called after each block with the cumulative frame count; return
///   [`Flow::Stop`] to end early. Position reporting / cancellation live here.
///
/// Returns the negotiated [`StreamSpec`].
pub fn run_playback<F>(
    path: &Path,
    device: &str,
    max_frames: Option<u64>,
    period: i64,
    periods: i64,
    mut tick: F,
) -> Result<StreamSpec>
where
    F: FnMut(u64) -> Flow,
{
    let mut decoder = Decoder::open(path)?;
    let mut spec = decoder.spec;
    // Device-aware bit-perfect format: keep the source-native choice if the
    // device supports it, else widen to the narrowest supported container.
    let formats = probe_formats(device).unwrap_or_else(|_| DeviceFormats::all());
    if let Some(fmt) = formats.choose(spec.source_bits) {
        spec.fmt = fmt;
    }
    let sink = AlsaSink::open(device, spec, period, periods)?;
    let mut packer = Packer::new(spec.fmt);

    let channels = spec.channels as usize;
    let mut block: Vec<i32> = Vec::new();
    let mut frames_played: u64 = 0;

    while decoder.next(&mut block)? {
        let mut samples = block.as_slice();

        if let Some(maxf) = max_frames {
            if frames_played >= maxf {
                break;
            }
            let remaining = (maxf - frames_played) as usize * channels;
            if samples.len() > remaining {
                samples = &samples[..remaining];
            }
        }
        if samples.is_empty() {
            continue;
        }

        let out = packer.pack(samples);
        sink.write(out)?;

        frames_played += (samples.len() / channels) as u64;
        if tick(frames_played) == Flow::Stop {
            break;
        }
    }

    sink.drain()?;
    Ok(spec)
}

// ---------------------------------------------------------------------------
// Phase 2 real-time gapless engine.
// ---------------------------------------------------------------------------

/// Internal control messages from the decode thread to the audio thread.
pub(crate) enum Ctl {
    /// Start a segment for `spec` (after the current one drains, if any),
    /// reading bytes from `consumer`. `pos_base` is the per-track frame the
    /// segment starts at (0 for a fresh track, the landed frame after a seek),
    /// used to rebase [`Event::Position`].
    Open {
        spec: StreamSpec,
        consumer: Consumer<u8>,
        pos_base: u64,
    },
    /// The queue is finished: drain and report `Ended`. If `quit`, the audio
    /// thread then terminates (the blocking player wants it to end); otherwise
    /// it stays alive and idles for the next command (interactive player).
    Finish { quit: bool },
    /// Immediate stop: discard buffered audio.
    Flush,
    /// Hold output at the device (hardware pause) until `Resume`/`Flush`/`Quit`.
    Pause,
    /// Resume output after a `Pause`.
    Resume,
    /// Shut the audio thread down.
    Quit,
}

/// Playback statistics returned by the audio thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Total frames written to the device across the whole run.
    pub frames: u64,
    /// Recovered underruns (EPIPE) — should be 0 on a healthy RT path.
    pub xruns: u64,
    /// What scheduling the audio thread actually obtained.
    pub sched: Sched,
}

/// Play `paths` end-to-end through the real-time gapless engine, blocking until
/// the whole queue has played out. Tracks sharing a wire [`StreamSpec`] stream
/// through one open device (gapless); a rate/format change drains and reopens.
/// `emit` receives [`Event`]s (track starts, position, end, errors). Returns the
/// audio thread's [`Stats`].
pub fn play_queue_blocking<E>(
    paths: &[PathBuf],
    device: &str,
    period: i64,
    periods: i64,
    emit: E,
) -> Result<Stats>
where
    E: Fn(Event) + Send + Sync + 'static,
{
    let emit: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(emit);
    let (ctl_tx, ctl_rx) = unbounded::<Ctl>();

    // Probe the device once up front (it is free here — the audio thread won't
    // open it until it gets the first segment). Format choice per track uses
    // this cache, so the decode thread never reopens the device mid-playback.
    let formats = probe_formats(device).unwrap_or_else(|_| DeviceFormats::all());

    let audio = {
        let device = device.to_string();
        let emit = emit.clone();
        thread::Builder::new()
            .name("player-audio".into())
            .spawn(move || audio_thread::run(device, ctl_rx, period, periods, emit))
            .expect("spawn audio thread")
    };

    let mut writer = SegmentWriter::new(ctl_tx);
    let mut scratch: Vec<u8> = Vec::new();
    let mut result: Result<()> = Ok(());

    'outer: for path in paths {
        // Whole files (no `.cue` range); a DSD file becomes a DoP stream.
        let (mut producer, spec) = match build_producer(path, None, None, &formats, &emit) {
            Ok(v) => v,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        let fb = spec.bytes_per_frame();
        emit(Event::Started {
            spec,
            path: path.clone(),
        });

        loop {
            match producer.next_bytes(&mut scratch) {
                Ok(true) => {
                    let Some(prod) = writer.producer_for(spec) else {
                        break 'outer; // audio thread gone
                    };
                    if !ring::push_block(prod, &scratch, fb, &mut || false) {
                        break 'outer;
                    }
                }
                Ok(false) => break,
                Err(e) => {
                    result = Err(e);
                    break 'outer;
                }
            }
        }
    }

    writer.finish(true);
    let stats = audio.join().unwrap_or_default();
    result.map(|()| stats)
}

// ---------------------------------------------------------------------------
// Interactive player (GTK shell).
// ---------------------------------------------------------------------------

pub enum Cmd {
    /// Replace the queue with this track and start it now.
    Play(PathBuf),
    /// Replace the queue with a sub-range of this file and start it now (a
    /// `.cue` track): seek to `start`, decode until `end`.
    PlayRange {
        path: PathBuf,
        start: Duration,
        end: Duration,
    },
    /// Replace the queue with track `track` of a SACD `.iso` and start it now.
    PlaySacd { path: PathBuf, track: usize },
    /// Append to the queue (gapless if it shares the current wire spec).
    Enqueue(PathBuf),
    /// Append a SACD `.iso` track to the queue (gapless across same-rate tracks).
    EnqueueSacd { path: PathBuf, track: usize },
    /// Hold output (hardware pause) without dropping the current track.
    Pause,
    /// Resume after a pause.
    Resume,
    /// Seek the current track to this position; output jumps to the new frame.
    Seek(Duration),
    /// Stop immediately and clear the queue.
    Stop,
    /// Shut the engine down (sent on drop).
    Quit,
}

#[derive(Debug, Clone)]
pub enum Event {
    Started { spec: StreamSpec, path: PathBuf },
    Position(u64),
    Ended,
    Error(String),
}

/// Threaded player: a decode thread + a real-time audio thread. Commands flow in
/// over a channel; events flow out through a caller callback (the GTK shell
/// bridges that to `async-channel` + `glib::spawn_future_local`).
pub struct Player {
    cmd_tx: Sender<Cmd>,
    interrupt: Arc<AtomicBool>,
    decode: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<Stats>>,
}

impl Player {
    /// Spawn the engine for `device`. `emit` is called (from engine threads) for
    /// every [`Event`].
    pub fn spawn<E>(device: String, emit: E) -> Self
    where
        E: Fn(Event) + Send + Sync + 'static,
    {
        let emit: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(emit);
        let (cmd_tx, cmd_rx) = bounded::<Cmd>(64);
        let (ctl_tx, ctl_rx) = unbounded::<Ctl>();
        let interrupt = Arc::new(AtomicBool::new(false));

        let audio = {
            let device = device.clone();
            let emit = emit.clone();
            thread::Builder::new()
                .name("player-audio".into())
                .spawn(move || {
                    audio_thread::run(device, ctl_rx, DEFAULT_PERIOD, DEFAULT_PERIODS, emit)
                })
                .expect("spawn audio thread")
        };
        let decode = {
            let interrupt = interrupt.clone();
            thread::Builder::new()
                .name("player-decode".into())
                .spawn(move || {
                    decode_thread::run_interactive(cmd_rx, ctl_tx, interrupt, device, emit);
                })
                .expect("spawn decode thread")
        };

        Self {
            cmd_tx,
            interrupt,
            decode: Some(decode),
            audio: Some(audio),
        }
    }

    pub fn play(&self, path: PathBuf) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Play(path));
    }

    /// Play a sub-range `[start, end)` of `path` (a `.cue` track). Like
    /// [`Player::play`] but the decoder seeks to `start` and stops at `end`.
    pub fn play_range(&self, path: PathBuf, start: Duration, end: Duration) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::PlayRange { path, start, end });
    }

    /// Play track `track` of a SACD `.iso` now (replaces the queue). Like
    /// [`Player::play`] but routed through the SACD/DoP path.
    pub fn play_sacd(&self, path: PathBuf, track: usize) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::PlaySacd { path, track });
    }

    pub fn enqueue(&self, path: PathBuf) {
        let _ = self.cmd_tx.send(Cmd::Enqueue(path));
    }

    /// Append a SACD `.iso` track to the queue (gapless across same-rate tracks).
    pub fn enqueue_sacd(&self, path: PathBuf, track: usize) {
        let _ = self.cmd_tx.send(Cmd::EnqueueSacd { path, track });
    }

    /// Hold output at the device without dropping the current track. Does not set
    /// `interrupt` (that would abort the in-flight decode and lose the track); the
    /// decode thread picks the command up at its next loop top.
    pub fn pause(&self) {
        let _ = self.cmd_tx.send(Cmd::Pause);
    }

    /// Resume after [`Player::pause`].
    pub fn resume(&self) {
        let _ = self.cmd_tx.send(Cmd::Resume);
    }

    /// Seek the current track to `pos`. Output jumps to the new frame and
    /// (re)starts playing from there. No-op if nothing is loaded.
    pub fn seek(&self, pos: Duration) {
        let _ = self.cmd_tx.send(Cmd::Seek(pos));
    }

    pub fn stop(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Stop);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Quit);
        if let Some(h) = self.decode.take() {
            let _ = h.join();
        }
        if let Some(h) = self.audio.take() {
            let _ = h.join();
        }
    }
}
