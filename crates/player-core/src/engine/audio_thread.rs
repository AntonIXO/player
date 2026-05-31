//! The real-time audio thread. Owns the ALSA sink, drains the ring, and writes
//! via blocking `writei` (which paces playback to real time and provides
//! natural backpressure). It plays one *segment* per [`StreamSpec`]; a segment
//! boundary — a spec change or the end of the queue — is the only place the
//! device is drained and reopened.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use rtrb::Consumer;

use crate::format::StreamSpec;
use crate::rt;
use crate::sink::AlsaSink;

use super::{Ctl, Event, Stats};

/// SCHED_FIFO priority for the audio thread. Comfortably under the typical
/// rtprio ceiling; only matters once decode is off this thread (it now is).
const AUDIO_RT_PRIO: i32 = 80;

/// A segment-lifecycle directive, buffered locally in queue order. (`Ctl::Flush`
/// / `Ctl::Quit` are interrupts and never buffered.)
enum Next {
    Open(StreamSpec, Consumer<u8>),
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
    let mut stats = Stats {
        frames: 0,
        xruns: 0,
        sched,
    };

    // Segment directives that arrived while an earlier segment was still
    // playing, kept in order so a decode thread racing ahead across several
    // rate changes never loses or reorders a segment.
    let mut pending: VecDeque<Next> = VecDeque::new();

    'outer: loop {
        // Acquire the next segment to play (from buffered directives, else block).
        let (spec, mut consumer) = loop {
            let dir = match pending.pop_front() {
                Some(d) => d,
                None => match ctl_rx.recv() {
                    Ok(Ctl::Open { spec, consumer }) => Next::Open(spec, consumer),
                    Ok(Ctl::Finish { quit }) => Next::Finish { quit },
                    Ok(Ctl::Flush) => continue, // idle flush: nothing to stop
                    Ok(Ctl::Quit) | Err(_) => Next::Quit,
                },
            };
            match dir {
                Next::Open(s, c) => break (s, c),
                Next::Finish { quit } => {
                    emit(Event::Ended);
                    if quit {
                        break 'outer;
                    }
                }
                Next::Quit => break 'outer,
            }
        };

        let mut sink = match AlsaSink::open(&device, spec, period, periods) {
            Ok(s) => s,
            Err(e) => {
                emit(Event::Error(e.to_string()));
                continue;
            }
        };

        let outcome = play_segment(
            &mut sink,
            &mut consumer,
            spec,
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
    period: i64,
    ctl_rx: &Receiver<Ctl>,
    emit: &Arc<dyn Fn(Event) + Send + Sync>,
    stats: &mut Stats,
    pending: &mut VecDeque<Next>,
) -> Outcome {
    let fb = spec.bytes_per_frame();
    let cap = period as usize * fb; // at most one period between control checks
    let pos_step = (spec.rate as u64 / 10).max(1); // ~100 ms position cadence
    let mut last_pos = stats.frames;

    loop {
        // Interrupts are handled now; segment directives are buffered in order.
        match ctl_rx.try_recv() {
            Ok(Ctl::Flush) => return Outcome::Flushed,
            Ok(Ctl::Quit) => return Outcome::Quit,
            Ok(Ctl::Open { spec, consumer }) => pending.push_back(Next::Open(spec, consumer)),
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
                    emit(Event::Position(stats.frames));
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
