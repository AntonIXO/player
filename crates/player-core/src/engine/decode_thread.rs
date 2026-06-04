//! Decode thread: owns the play queue and the ring producer. It decodes each
//! track to packed bytes (identical layout to the v1 sink output) and pushes
//! them into the ring. Consecutive tracks sharing a wire spec stream through one
//! segment (gapless — the audio thread never sees the boundary); a spec change
//! opens a new segment (new ring + `Ctl::Open`), which makes the audio thread
//! drain and reopen the device.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use rtrb::Producer;

use crate::convert::{append_bytes, Packer};
use crate::decode::Decoder;
use crate::format::{DeviceFormats, StreamSpec};
use crate::sink::probe_formats;

use super::ring::{push_block, ring_for_spec};
use super::{Cmd, Ctl, Event};

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
            let (prod, consumer) = ring_for_spec(spec);
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

struct Track {
    dec: Decoder,
    packer: Packer,
    spec: StreamSpec,
}

/// A queued item: a file plus an optional decode range (`Some` for a `.cue`
/// track — seek to `start`, stop at `end`).
struct Source {
    path: PathBuf,
    range: Option<(Duration, Duration)>,
}

impl Source {
    fn whole(path: PathBuf) -> Self {
        Self { path, range: None }
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
            });
            *cur = None;
            writer.flush();
            *paused = false;
            interrupt.store(false, Ordering::SeqCst);
        }
        Cmd::Enqueue(p) => queue.push_back(Source::whole(p)),
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
            if let Some(track) = cur.as_mut() {
                writer.flush();
                match track.dec.seek(d) {
                    Ok(landed) => writer.set_next_pos_base(landed),
                    Err(e) => emit(Event::Error(e.to_string())),
                }
                *paused = false;
                interrupt.store(false, Ordering::SeqCst);
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
    let mut block: Vec<i32> = Vec::new();
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
                Some(src) => match Decoder::open(&src.path) {
                    Ok(mut dec) => {
                        // A .cue track decodes a sub-range: seek to its start and
                        // cap the decode at its length. Per-track position rebases
                        // to 0 because the GTK shell issues a fresh Play(Range) per
                        // track (flush → new segment, pos_base 0).
                        if let Some((start, end)) = src.range {
                            if let Err(e) = dec.seek(start) {
                                emit(Event::Error(e.to_string()));
                            }
                            if end > start {
                                let frames = (end - start).as_millis() as u64
                                    * dec.spec.rate as u64
                                    / 1000;
                                dec.set_limit(frames);
                            }
                        }
                        let mut spec = dec.spec;
                        if let Some(fmt) = formats.choose(spec.source_bits) {
                            spec.fmt = fmt;
                        }
                        emit(Event::Started {
                            spec,
                            path: src.path,
                        });
                        cur = Some(Track {
                            dec,
                            packer: Packer::new(spec.fmt),
                            spec,
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

        // 3) Decode and push one block.
        if let Some(track) = cur.as_mut() {
            let spec = track.spec;
            let fb = spec.bytes_per_frame();
            match track.dec.next(&mut block) {
                Ok(true) => {
                    scratch.clear();
                    append_bytes(&mut scratch, &track.packer.pack(&block));
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
