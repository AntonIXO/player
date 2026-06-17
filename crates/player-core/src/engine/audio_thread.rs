//! The real-time audio thread. Owns the ALSA sink, drains the ring, and writes
//! via blocking `writei` (which paces playback to real time and provides
//! natural backpressure). It plays one *segment* per [`StreamSpec`]; a segment
//! boundary — a spec change or the end of the queue — is the only place the
//! device is drained and reopened.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, TryRecvError};
use rtrb::Consumer;

use crate::dop::dop_silence;
use crate::format::StreamSpec;
use crate::rt::{self, CpuLatencyGuard};
use crate::sink::AlsaSink;

use super::{Ctl, Event, Stats};

/// SCHED_FIFO priority for the audio thread. Comfortably under the typical
/// rtprio ceiling; only matters once decode is off this thread (it now is).
const AUDIO_RT_PRIO: i32 = 80;

/// A segment-lifecycle directive, buffered locally in queue order. (`Ctl::Flush`
/// / `Ctl::Quit` are interrupts and never buffered.)
enum Next {
    Open(StreamSpec, Consumer<u8>, u64),
    Finish { quit: bool },
    Quit,
}

enum Outcome {
    /// Segment fully delivered; drain the device and move to the next directive.
    Drained,
    /// Immediate stop requested; discard buffered audio and pending segments.
    Flushed,
    /// Shut the thread down.
    Quit,
}

pub(crate) fn run(
    device: String,
    ctl_rx: Receiver<Ctl>,
    period: i64,
    periods: i64,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
) -> Stats {
    let sched = rt::try_set_realtime_fifo(AUDIO_RT_PRIO);
    // Optional big-core pin (device-specific): `PLAYER_AUDIO_CPU=<n>` pins the
    // audio thread to CPU n to cut scheduling jitter on big.LITTLE (Poco F1).
    if let Some(cpu) = std::env::var("PLAYER_AUDIO_CPU").ok().and_then(|s| s.parse().ok()) {
        rt::pin_to_cpu(cpu);
    }
    let mut stats = Stats {
        frames: 0,
        xruns: 0,
        sched,
    };

    // PM-QoS guard: held only while a segment is actively playing so deep CPU
    // C-states (whose exit latency is a common source of USB-audio xruns) can't
    // engage mid-stream; released when the thread goes idle so the CPU can sleep
    // between sessions. No-op on a dev box where `/dev/cpu_dma_latency` isn't
    // writable — bit-perfect output is unaffected either way.
    let mut latency_guard: Option<CpuLatencyGuard> = None;

    // Segment directives that arrived while an earlier segment was still
    // playing, kept in order so a decode thread racing ahead across several
    // rate changes never loses or reorders a segment.
    let mut pending: VecDeque<Next> = VecDeque::new();

    'outer: loop {
        // Acquire the next segment to play (from buffered directives, else block).
        let (spec, mut consumer, pos_base) = loop {
            let dir = if let Some(d) = pending.pop_front() { d } else {
                // Nothing queued: going idle. Release the latency guard so
                // the CPU may sleep until the next segment arrives.
                latency_guard = None;
                match ctl_rx.recv() {
                    Ok(Ctl::Open {
                        spec,
                        consumer,
                        pos_base,
                    }) => Next::Open(spec, consumer, pos_base),
                    Ok(Ctl::Finish { quit }) => Next::Finish { quit },
                    // Idle flush / pause / resume: no active segment to act on.
                    Ok(Ctl::Flush | Ctl::Pause | Ctl::Resume) => continue,
                    Ok(Ctl::Quit) | Err(_) => Next::Quit,
                }
            };
            match dir {
                Next::Open(s, c, b) => break (s, c, b),
                Next::Finish { quit } => {
                    emit(Event::Ended);
                    if quit {
                        break 'outer;
                    }
                }
                Next::Quit => break 'outer,
            }
        };

        // Playback is about to start: bound CPU wake-up latency (100 µs) for the
        // life of this and any gapless follow-on segment. Held across pauses;
        // released only when the thread returns to the idle wait above.
        if latency_guard.is_none() {
            latency_guard = CpuLatencyGuard::new(100);
        }

        let mut sink = match AlsaSink::open(&device, spec, period, periods) {
            Ok(s) => s,
            Err(e) => {
                emit(Event::Error(e.to_string()));
                continue;
            }
        };
        // Engine path: guard every stream re-init (xrun/suspend recovery) with a
        // silence band so the Mojo 2 can never emit a full-scale burst.
        sink.enable_reinit_guard();

        let outcome = play_segment(
            &mut sink,
            &mut consumer,
            spec,
            pos_base,
            period,
            &ctl_rx,
            &emit,
            &mut stats,
            &mut pending,
        );
        stats.xruns += sink.xruns();

        match outcome {
            Outcome::Drained => {
                // Play out whatever ALSA still has buffered, then loop to the
                // next directive (a reopen for a new spec, or Finish).
                let _ = sink.drain();
            }
            Outcome::Flushed => {
                pending.clear();
                drop(sink); // closing the PCM discards ALSA's buffered audio
            }
            Outcome::Quit => {
                drop(sink);
                break;
            }
        }
    }

    stats
}

#[allow(clippy::too_many_arguments)]
fn play_segment(
    sink: &mut AlsaSink,
    consumer: &mut Consumer<u8>,
    spec: StreamSpec,
    pos_base: u64,
    period: i64,
    ctl_rx: &Receiver<Ctl>,
    emit: &Arc<dyn Fn(Event) + Send + Sync>,
    stats: &mut Stats,
    pending: &mut VecDeque<Next>,
) -> Outcome {
    let fb = spec.bytes_per_frame();
    let cap = period as usize * fb; // at most one period between control checks
    let pos_step = (spec.rate as u64 / 10).max(1); // ~100 ms position cadence
    // Position is reported *per track*: frames written within this segment, on
    // top of `pos_base` (0 for a fresh track, the landed frame after a seek).
    let seg_start = stats.frames;
    let mut last_pos = stats.frames;

    loop {
        // Interrupts are handled now; segment directives are buffered in order.
        match ctl_rx.try_recv() {
            Ok(Ctl::Flush) => return Outcome::Flushed,
            Ok(Ctl::Quit) => return Outcome::Quit,
            Ok(Ctl::Pause) => match pause_until_resumed(sink, spec, period, ctl_rx, emit, pending) {
                Outcome::Drained => {} // resumed: fall through and keep playing
                other => return other,
            },
            Ok(Ctl::Resume) => {} // not paused; ignore
            Ok(Ctl::Open {
                spec,
                consumer,
                pos_base,
            }) => pending.push_back(Next::Open(spec, consumer, pos_base)),
            Ok(Ctl::Finish { quit }) => pending.push_back(Next::Finish { quit }),
            Err(_) => {}
        }

        let avail = (consumer.slots() / fb) * fb; // whole frames ready
        if avail >= fb {
            let n = avail.min(cap);
            if let Ok(chunk) = consumer.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                let res = sink.write_all_bytes(a).and_then(|()| {
                    if b.is_empty() {
                        Ok(())
                    } else {
                        sink.write_all_bytes(b)
                    }
                });
                chunk.commit(n);
                if let Err(e) = res {
                    emit(Event::Error(e.to_string()));
                    return Outcome::Drained;
                }
                stats.frames += (n / fb) as u64;
                if stats.frames - last_pos >= pos_step {
                    last_pos = stats.frames;
                    emit(Event::Position(pos_base + (stats.frames - seg_start)));
                }
            }
        } else if consumer.is_abandoned() {
            // Producer dropped and (whole-frame invariant) the ring is empty:
            // the segment has been fully delivered.
            return Outcome::Drained;
        } else {
            // Ring momentarily empty but more is coming. Steady state never lands
            // here (the decoder stays ahead); brief wait avoids a busy spin.
            std::thread::sleep(Duration::from_micros(500));
        }
    }
}

/// Hold the device paused until resumed, buffering any segment directives that
/// arrive so the queue is not disturbed. Returns [`Outcome::Drained`] to mean
/// *resumed, keep playing*; `Flushed`/`Quit` propagate.
///
/// Two strategies, chosen by the device's capability:
///
/// * **Hardware pause** (`can_pause`): the DAC clock halts, ALSA's buffer is
///   preserved, resume is seamless. We block on the control channel.
/// * **Silence keep-alive** (no hardware pause — e.g. the Mojo 2): we deliberately
///   do **not** `drop()` the stream. `drop()`+`prepare()` tears down and
///   re-initializes the USB substream, and on the Mojo 2 the re-init can emit a
///   full-scale burst on resume (and, for DoP, the DAC loses its DSD lock and
///   mis-reads the resumed words as ~0 dBFS PCM). Instead we keep the device open
///   and the clock running by writing idle silence every period until resumed.
///   The DAC never re-inits, so resume is seamless and burst-free.
///
/// **Bit-perfect:** only idle silence is written while paused; the music bytes
/// still buffered in the ring are played back unaltered on resume.
fn pause_until_resumed(
    sink: &AlsaSink,
    spec: StreamSpec,
    period: i64,
    ctl_rx: &Receiver<Ctl>,
    emit: &Arc<dyn Fn(Event) + Send + Sync>,
    pending: &mut VecDeque<Next>,
) -> Outcome {
    if sink.can_pause() {
        // Hardware pause: block until commanded.
        if let Err(e) = sink.pause(true) {
            emit(Event::Error(e.to_string()));
        }
        loop {
            match ctl_rx.recv() {
                Ok(Ctl::Resume) => {
                    if let Err(e) = sink.pause(false) {
                        emit(Event::Error(e.to_string()));
                    }
                    return Outcome::Drained;
                }
                Ok(Ctl::Flush) => return Outcome::Flushed,
                Ok(Ctl::Quit) | Err(_) => return Outcome::Quit,
                Ok(Ctl::Open {
                    spec,
                    consumer,
                    pos_base,
                }) => pending.push_back(Next::Open(spec, consumer, pos_base)),
                Ok(Ctl::Finish { quit }) => pending.push_back(Next::Finish { quit }),
                Ok(Ctl::Pause) => {} // already paused
            }
        }
    }

    // Silence keep-alive: hold the stream open by writing one period of silence
    // per iteration. The blocking `writei` paces this loop to real time, so it
    // does not busy-spin while waiting for the resume command.
    eprintln!("[audio] pause: silence keep-alive (no hardware pause)");
    let silence = pause_silence(spec, period);
    loop {
        match ctl_rx.try_recv() {
            Ok(Ctl::Resume) => {
                eprintln!("[audio] resume from silence keep-alive");
                return Outcome::Drained;
            }
            Ok(Ctl::Flush) => return Outcome::Flushed,
            Ok(Ctl::Quit) => return Outcome::Quit,
            Ok(Ctl::Open {
                spec,
                consumer,
                pos_base,
            }) => pending.push_back(Next::Open(spec, consumer, pos_base)),
            Ok(Ctl::Finish { quit }) => pending.push_back(Next::Finish { quit }),
            Ok(Ctl::Pause) => {}             // already paused
            Err(TryRecvError::Empty) => {}   // nothing yet: write a period of silence
            Err(TryRecvError::Disconnected) => return Outcome::Quit,
        }
        if let Err(e) = sink.write_all_bytes(&silence) {
            // A write error here means the device is unhappy; surface it and let
            // the caller drain rather than spinning on silence forever.
            emit(Event::Error(e.to_string()));
            return Outcome::Drained;
        }
    }
}

/// One period of bit-perfect idle silence for `spec`, written repeatedly to hold
/// a non-hardware-pause stream open. PCM → zeroed samples; DoP → valid
/// DoP-silence (alternating `0x05`/`0xFA` markers, `0x69` payload) so the DAC
/// keeps its DSD lock (all-zero words would have an invalid marker and break it).
fn pause_silence(spec: StreamSpec, period: i64) -> Vec<u8> {
    if spec.is_dop {
        // Even frame count keeps the marker alternation continuous across periods.
        let frames = (period.max(2) as usize) & !1;
        dop_silence(spec.fmt, spec.channels, frames)
    } else {
        vec![0u8; period.max(1) as usize * spec.bytes_per_frame()]
    }
}
