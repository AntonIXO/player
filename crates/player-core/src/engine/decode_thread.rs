//! Decode thread: owns the play queue and the ring producer. It decodes each
//! track to packed bytes (identical layout to the v1 sink output) and pushes
//! them into the ring. Consecutive tracks sharing a wire spec stream through one
//! segment (gapless — the audio thread never sees the boundary); a spec change
//! opens a new segment (new ring + `Ctl::Open`), which makes the audio thread
//! drain and reopen the device.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use rtrb::Producer;

use crate::convert::{append_bytes, Packer};
use crate::decode::Decoder;
use crate::dop::{dop_silence, DopPacker};
use crate::dsd::{is_dsd_path, open_dsd, DsdSource};
use crate::error::{Error, Result};
use crate::format::{DeviceFormats, StreamSpec};
use crate::sink::probe_formats;

use super::ring::{push_block, ring_for_spec};
use super::{Cmd, Ctl, Event};

/// Milliseconds of DSD-silence DoP pre-roll written when a DoP device opens, so
/// the DAC locks the marker pattern before real audio (avoids a start click).
const DOP_PREROLL_MS: usize = 16;

/// Owns the current ring producer and emits segment-lifecycle control events.
/// Shared by the blocking queue player and the interactive [`super::Player`].
pub(crate) struct SegmentWriter {
    ctl_tx: Sender<Ctl>,
    prod: Option<Producer<u8>>,
    cur: Option<StreamSpec>,
    /// Per-track frame the *next* opened segment starts at (0 normally, the
    /// landed frame after a seek). Consumed (reset to 0) on the next `Open`.
    next_pos_base: u64,
}

impl SegmentWriter {
    pub(crate) fn new(ctl_tx: Sender<Ctl>) -> Self {
        Self {
            ctl_tx,
            prod: None,
            cur: None,
            next_pos_base: 0,
        }
    }

    /// The producer to push `spec`'s bytes into. A new segment (a new ring and
    /// an `Open` directive) starts only when the wire spec changes; otherwise
    /// this returns the current producer so tracks stream gaplessly. Returns
    /// `None` if the audio thread is gone.
    pub(crate) fn producer_for(&mut self, spec: StreamSpec) -> Option<&mut Producer<u8>> {
        let continues = self.prod.is_some() && self.cur.is_some_and(|c| c.same_wire(&spec));
        if !continues {
            let (mut prod, consumer) = ring_for_spec(spec);
            let pos_base = self.next_pos_base;
            // Send Open *before* dropping the old producer: by the time the audio
            // thread observes the old consumer abandoned, the directive is queued.
            if self
                .ctl_tx
                .send(Ctl::Open {
                    spec,
                    consumer,
                    pos_base,
                })
                .is_err()
            {
                return None;
            }
            self.next_pos_base = 0;
            // DoP: lead the segment with a few ms of DSD silence so the DAC locks
            // the DoP marker pattern before real audio (prevents a start click).
            // Leading silence only — the music samples are untouched.
            if spec.is_dop {
                let frames = (spec.rate as usize / 1000 * DOP_PREROLL_MS) & !1;
                let silence = dop_silence(spec.fmt, spec.channels, frames);
                let _ = push_block(&mut prod, &silence, spec.bytes_per_frame(), &mut || false);
            }
            self.prod = Some(prod); // dropping the old producer abandons its consumer
            self.cur = Some(spec);
        }
        self.prod.as_mut()
    }

    /// Set the per-track base frame for the next segment (after a seek). Force a
    /// fresh segment first via [`SegmentWriter::flush`] so it actually takes.
    pub(crate) fn set_next_pos_base(&mut self, base: u64) {
        self.next_pos_base = base;
    }

    /// Forward a hardware pause to the audio thread (segment state untouched).
    pub(crate) fn pause(&mut self) {
        let _ = self.ctl_tx.send(Ctl::Pause);
    }

    /// Forward a resume to the audio thread.
    pub(crate) fn resume(&mut self) {
        let _ = self.ctl_tx.send(Ctl::Resume);
    }

    /// Queue finished: drain and report `Ended`. `quit` asks the audio thread to
    /// terminate after draining (blocking player) rather than idle (interactive).
    pub(crate) fn finish(&mut self, quit: bool) {
        let _ = self.ctl_tx.send(Ctl::Finish { quit });
        self.prod = None;
        self.cur = None;
    }

    /// Immediate stop: discard buffered audio.
    pub(crate) fn flush(&mut self) {
        let _ = self.ctl_tx.send(Ctl::Flush);
        self.prod = None;
        self.cur = None;
    }

    /// Shut the audio thread down.
    pub(crate) fn quit(&mut self) {
        let _ = self.ctl_tx.send(Ctl::Quit);
    }

    pub(crate) fn has_segment(&self) -> bool {
        self.prod.is_some()
    }
}

/// A track's byte producer. PCM tracks decode then pack; DSD tracks read native
/// DSD then DoP-pack. Both yield already-packed wire bytes, so everything
/// downstream (ring, segment, audio thread) is identical.
pub(crate) enum TrackProducer {
    Pcm {
        dec: Decoder,
        packer: Packer,
        block: Vec<i32>,
    },
    Dsd {
        reader: Box<dyn DsdSource>,
        dop: DopPacker,
        buf: Vec<u8>,
    },
}

impl TrackProducer {
    /// Fill `out` (cleared here) with the next block of packed wire bytes.
    /// Returns `false` at end of track.
    pub(crate) fn next_bytes(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self {
            TrackProducer::Pcm { dec, packer, block } => {
                if dec.next(block)? {
                    out.clear();
                    append_bytes(out, &packer.pack(block));
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            TrackProducer::Dsd { reader, dop, buf } => {
                if reader.next(buf)? {
                    out.clear();
                    out.extend_from_slice(dop.pack(buf));
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }
}

struct Track {
    producer: TrackProducer,
    spec: StreamSpec,
    /// Decode range for a `.cue` track (`start`, `end` within the source file);
    /// `None` for a whole-file track (and always `None` for DSD). Used to make an
    /// in-place seek track-relative.
    range: Option<(Duration, Duration)>,
}

/// Open a queued source into a packed-byte [`TrackProducer`] and its wire
/// [`StreamSpec`], applying device-aware bit-perfect format choice. A DSD file
/// (`.dsf`/`.dff`/`.iso`) becomes a DoP stream (`S24_3`/`S32` carrying DSD); a
/// PCM file decodes as before. `.cue` ranges apply to PCM only; a seek error is
/// reported via `emit` but is non-fatal (parity with the whole-file path).
pub(crate) fn build_producer(
    path: &Path,
    range: Option<(Duration, Duration)>,
    sacd_track: Option<usize>,
    formats: &DeviceFormats,
    emit: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<(TrackProducer, StreamSpec)> {
    if is_dsd_path(path) {
        let reader = open_dsd(path, sacd_track)?;
        let dspec = reader.spec();
        // DoP needs a 24-bit-capable container; pick the narrowest the device
        // supports (S24_3, else S32). Hard-error if it has neither.
        let fmt = formats.choose(24).ok_or_else(|| {
            Error::Unsupported("device has no 24/32-bit PCM format to carry DoP".into())
        })?;
        let spec = dspec.dop_spec(fmt);
        let dop = DopPacker::new(fmt, dspec.channels);
        Ok((
            TrackProducer::Dsd {
                reader,
                dop,
                buf: Vec::new(),
            },
            spec,
        ))
    } else {
        let mut dec = Decoder::open(path)?;
        if let Some((start, end)) = range {
            if let Err(e) = dec.seek(start) {
                emit(Event::Error(e.to_string()));
            }
            if end > start {
                let frames = (end - start).as_millis() as u64 * dec.spec.rate as u64 / 1000;
                dec.set_limit(frames);
            }
        }
        let mut spec = dec.spec;
        if let Some(fmt) = formats.choose(spec.source_bits) {
            spec.fmt = fmt;
        }
        let packer = Packer::new(spec.fmt);
        Ok((
            TrackProducer::Pcm {
                dec,
                packer,
                block: Vec::new(),
            },
            spec,
        ))
    }
}

/// A queued item: a file plus, optionally, a `.cue` decode range (seek to
/// `start`, stop at `end`) or a SACD `.iso` track selector.
struct Source {
    path: PathBuf,
    range: Option<(Duration, Duration)>,
    /// Track index within a multi-track SACD `.iso` (`None` = whole file / first).
    sacd_track: Option<usize>,
}

impl Source {
    fn whole(path: PathBuf) -> Self {
        Self {
            path,
            range: None,
            sacd_track: None,
        }
    }

    fn sacd(path: PathBuf, track: usize) -> Self {
        Self {
            path,
            range: None,
            sacd_track: Some(track),
        }
    }
}

/// Whether the decode loop should keep running or shut down after a command.
enum CmdOutcome {
    Continue,
    Quit,
}

/// Apply one [`Cmd`] to the interactive decode state. Shared by the three places
/// commands are read (the try_recv drain, the paused block, and the idle block)
/// so their semantics never drift.
fn apply_cmd(
    cmd: Cmd,
    queue: &mut VecDeque<Source>,
    cur: &mut Option<Track>,
    writer: &mut SegmentWriter,
    interrupt: &Arc<AtomicBool>,
    paused: &mut bool,
    emit: &Arc<dyn Fn(Event) + Send + Sync>,
) -> CmdOutcome {
    match cmd {
        Cmd::Play(p) => {
            queue.clear();
            queue.push_back(Source::whole(p));
            *cur = None;
            writer.flush();
            *paused = false;
            interrupt.store(false, Ordering::SeqCst);
        }
        Cmd::PlayRange { path, start, end } => {
            queue.clear();
            queue.push_back(Source {
                path,
                range: Some((start, end)),
                sacd_track: None,
            });
            *cur = None;
            writer.flush();
            *paused = false;
            interrupt.store(false, Ordering::SeqCst);
        }
        Cmd::PlaySacd { path, track } => {
            queue.clear();
            queue.push_back(Source::sacd(path, track));
            *cur = None;
            writer.flush();
            *paused = false;
            interrupt.store(false, Ordering::SeqCst);
        }
        Cmd::Enqueue(p) => queue.push_back(Source::whole(p)),
        Cmd::EnqueueSacd { path, track } => queue.push_back(Source::sacd(path, track)),
        Cmd::Pause => {
            writer.pause();
            *paused = true;
        }
        Cmd::Resume => {
            writer.resume();
            *paused = false;
        }
        Cmd::Seek(d) => {
            // Seek the current track in place: discard buffered audio, reposition
            // the decoder, and rebase the next segment to the landed frame. Seek
            // (re)starts playback at the new position.
            //
            // For a `.cue` track the incoming `d` is *track-relative* (the UI knows
            // only the sub-range). Offset it into the file by the range start, then
            // re-apply the decode limit so playback still stops at the range end and
            // rebase the per-track position so it stays 0-based at the track start.
            if let Some(track) = cur.as_mut() {
                let range = track.range;
                // Seek is PCM-only; DSD (DoP) is not seekable yet, so ignore it.
                if let TrackProducer::Pcm { dec, .. } = &mut track.producer {
                    writer.flush();
                    let target = match range {
                        Some((start, end)) => start + d.min(end.saturating_sub(start)),
                        None => d,
                    };
                    match dec.seek(target) {
                        Ok(landed) => match range {
                            Some((start, end)) => {
                                let rate = dec.spec.rate as u64;
                                let start_fr = (start.as_millis() as u64) * rate / 1000;
                                let end_fr = (end.as_millis() as u64) * rate / 1000;
                                dec.set_limit(end_fr.saturating_sub(landed));
                                writer.set_next_pos_base(landed.saturating_sub(start_fr));
                            }
                            None => writer.set_next_pos_base(landed),
                        },
                        Err(e) => emit(Event::Error(e.to_string())),
                    }
                    *paused = false;
                    interrupt.store(false, Ordering::SeqCst);
                }
            }
        }
        Cmd::Stop => {
            queue.clear();
            *cur = None;
            writer.flush();
            *paused = false;
            interrupt.store(false, Ordering::SeqCst);
        }
        Cmd::Quit => {
            writer.flush();
            writer.quit();
            return CmdOutcome::Quit;
        }
    }
    CmdOutcome::Continue
}

/// Interactive driver behind [`super::Player`]: maintains a mutable queue and
/// reacts to commands. `interrupt` is set by Play/Stop/Quit so an in-flight
/// block push aborts promptly (Enqueue never interrupts — it just appends, which
/// is what enables gapless).
pub(crate) fn run_interactive(
    cmd_rx: Receiver<Cmd>,
    ctl_tx: Sender<Ctl>,
    interrupt: Arc<AtomicBool>,
    device: String,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
) {
    // Probe the output device once (it is free until the audio thread opens it).
    let formats = probe_formats(&device).unwrap_or_else(|_| DeviceFormats::all());
    let mut writer = SegmentWriter::new(ctl_tx);
    let mut queue: VecDeque<Source> = VecDeque::new();
    let mut cur: Option<Track> = None;
    let mut paused = false;
    let mut scratch: Vec<u8> = Vec::new();

    loop {
        // 1) Apply all pending commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if let CmdOutcome::Quit =
                        apply_cmd(cmd, &mut queue, &mut cur, &mut writer, &interrupt, &mut paused, &emit)
                    {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    writer.flush();
                    writer.quit();
                    return;
                }
            }
        }

        // 1b) While paused, output is held at the device; don't decode (which
        // would only spin filling the ring). Block for the next command instead.
        if paused {
            match cmd_rx.recv() {
                Ok(cmd) => {
                    if let CmdOutcome::Quit =
                        apply_cmd(cmd, &mut queue, &mut cur, &mut writer, &interrupt, &mut paused, &emit)
                    {
                        return;
                    }
                }
                Err(_) => {
                    writer.quit();
                    return;
                }
            }
            continue;
        }

        // 2) Ensure a current track, or idle until commanded.
        if cur.is_none() {
            match queue.pop_front() {
                // A .cue track decodes a sub-range (seek to start, cap the
                // decode); a DSD file becomes a DoP stream. `build_producer`
                // applies device-aware format choice. Per-track position rebases
                // to 0 because the GTK shell issues a fresh Play(Range) per track.
                Some(src) => match build_producer(&src.path, src.range, src.sacd_track, &formats, &emit) {
                    Ok((producer, spec)) => {
                        cur = Some(Track {
                            producer,
                            spec,
                            range: src.range,
                        });
                        emit(Event::Started {
                            spec,
                            path: src.path,
                        });
                    }
                    Err(e) => emit(Event::Error(e.to_string())),
                },
                None => {
                    // Nothing queued: finish the open segment (drain + Ended),
                    // then block for the next command.
                    if writer.has_segment() {
                        writer.finish(false);
                    }
                    match cmd_rx.recv() {
                        Ok(cmd) => {
                            if let CmdOutcome::Quit = apply_cmd(
                                cmd, &mut queue, &mut cur, &mut writer, &interrupt, &mut paused, &emit,
                            ) {
                                return;
                            }
                        }
                        Err(_) => {
                            writer.quit();
                            return;
                        }
                    }
                    continue;
                }
            }
        }

        // 3) Decode/read and push one packed block.
        if let Some(track) = cur.as_mut() {
            let spec = track.spec;
            let fb = spec.bytes_per_frame();
            match track.producer.next_bytes(&mut scratch) {
                Ok(true) => {
                    match writer.producer_for(spec) {
                        Some(prod) => {
                            let pushed =
                                push_block(prod, &scratch, fb, &mut || interrupt.load(Ordering::SeqCst));
                            if !pushed {
                                // Interrupted (Play/Stop/Quit pending) or the audio
                                // thread vanished: drop this track. Any pending
                                // command is handled at the loop top.
                                cur = None;
                            }
                        }
                        None => cur = None,
                    }
                }
                // Track ended: next iteration pops the next one (gapless if it
                // shares the wire spec).
                Ok(false) => cur = None,
                Err(e) => {
                    emit(Event::Error(e.to_string()));
                    cur = None;
                }
            }
        }
    }
}
