//! v1 engine: a single thread that decodes -> converts -> writes to ALSA.
//! Blocking `writei` provides natural backpressure, so no ring buffer is needed
//! yet (the decode/RT-audio split is Phase 2). `run_playback` is the shared core
//! used by the CLI today and the GTK shell later.

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::convert::Packer;
use crate::decode::Decoder;
use crate::error::Result;
use crate::format::StreamSpec;
use crate::sink::AlsaSink;

/// Returned by the per-block callback to control the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Default ALSA buffering for v1 (robustness over latency): 1024-frame periods,
/// 8 periods (~186 ms at 44.1 kHz).
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
    let spec = decoder.spec;
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
// Threaded player for the GTK shell.
//
// Commands flow in over a crossbeam channel; events flow out through a
// caller-supplied callback so `player-core` stays UI-agnostic (the GTK crate
// bridges that callback to `async-channel` + `glib::spawn_future_local`).
// ---------------------------------------------------------------------------

pub enum Cmd {
    Play(PathBuf),
    Stop,
    Quit,
}

#[derive(Debug, Clone)]
pub enum Event {
    Started { spec: StreamSpec, path: PathBuf },
    Position(u64),
    Ended,
    Error(String),
}

pub struct Player {
    cmd_tx: Sender<Cmd>,
    handle: Option<JoinHandle<()>>,
}

impl Player {
    /// Spawn the engine thread for `device`. `emit` is invoked from the engine
    /// thread for every [`Event`].
    pub fn spawn<E>(device: String, emit: E) -> Self
    where
        E: Fn(Event) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = bounded::<Cmd>(16);
        let handle = thread::Builder::new()
            .name("player-engine".into())
            .spawn(move || engine_thread(device, cmd_rx, emit))
            .expect("spawn engine thread");
        Player {
            cmd_tx,
            handle: Some(handle),
        }
    }

    pub fn play(&self, path: PathBuf) {
        let _ = self.cmd_tx.send(Cmd::Play(path));
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// What the engine thread should do after a file finishes/stops.
enum After {
    Idle,
    Next(PathBuf),
    Quit,
}

fn engine_thread<E: Fn(Event)>(device: String, cmd_rx: Receiver<Cmd>, emit: E) {
    let mut pending = After::Idle;
    loop {
        let path = match pending {
            After::Next(p) => p,
            After::Quit => break,
            After::Idle => match cmd_rx.recv() {
                Ok(Cmd::Play(p)) => p,
                Ok(Cmd::Stop) => continue,
                Ok(Cmd::Quit) | Err(_) => break,
            },
        };
        pending = play_one(&device, &path, &cmd_rx, &emit);
    }
}

/// Play one file, polling for commands between blocks.
fn play_one<E: Fn(Event)>(
    device: &str,
    path: &Path,
    cmd_rx: &Receiver<Cmd>,
    emit: &E,
) -> After {
    // Probe once up front so the UI can show the format before audio starts.
    match Decoder::open(path) {
        Ok(d) => emit(Event::Started {
            spec: d.spec,
            path: path.to_path_buf(),
        }),
        Err(e) => {
            emit(Event::Error(e.to_string()));
            return After::Idle;
        }
    }

    let mut after = After::Idle;
    let res = run_playback(
        path,
        device,
        None,
        DEFAULT_PERIOD,
        DEFAULT_PERIODS,
        |frames| match cmd_rx.try_recv() {
            Ok(Cmd::Stop) => Flow::Stop,
            Ok(Cmd::Quit) => {
                after = After::Quit;
                Flow::Stop
            }
            Ok(Cmd::Play(p)) => {
                after = After::Next(p);
                Flow::Stop
            }
            Err(_) => {
                emit(Event::Position(frames));
                Flow::Continue
            }
        },
    );

    if let Err(e) = res {
        emit(Event::Error(e.to_string()));
    } else if matches!(after, After::Idle) {
        emit(Event::Ended);
    }
    after
}
