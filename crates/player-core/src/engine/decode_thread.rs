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
}

impl SegmentWriter {
    pub(crate) fn new(ctl_tx: Sender<Ctl>) -> Self {
        Self {
            ctl_tx,
            prod: None,
            cur: None,
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
            // Send Open *before* dropping the old producer: by the time the audio
            // thread observes the old consumer abandoned, the directive is queued.
            if self.ctl_tx.send(Ctl::Open { spec, consumer }).is_err() {
                return None;
            }
            self.prod = Some(prod); // dropping the old producer abandons its consumer
            self.cur = Some(spec);
        }
        self.prod.as_mut()
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
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    let mut cur: Option<Track> = None;
    let mut block: Vec<i32> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();

    loop {
        // 1) Apply all pending commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(Cmd::Play(p)) => {
                    queue.clear();
                    queue.push_back(p);
                    cur = None;
                    writer.flush();
                    interrupt.store(false, Ordering::SeqCst);
                }
                Ok(Cmd::Enqueue(p)) => queue.push_back(p),
                Ok(Cmd::Stop) => {
                    queue.clear();
                    cur = None;
                    writer.flush();
                    interrupt.store(false, Ordering::SeqCst);
                }
                Ok(Cmd::Quit) => {
                    writer.flush();
                    writer.quit();
                    return;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    writer.flush();
                    writer.quit();
                    return;
                }
            }
        }

        // 2) Ensure a current track, or idle until commanded.
        if cur.is_none() {
            match queue.pop_front() {
                Some(p) => match Decoder::open(&p) {
                    Ok(dec) => {
                        let mut spec = dec.spec;
                        if let Some(fmt) = formats.choose(spec.source_bits) {
                            spec.fmt = fmt;
                        }
                        emit(Event::Started { spec, path: p });
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
                        Ok(Cmd::Play(p)) => {
                            queue.push_back(p);
                            interrupt.store(false, Ordering::SeqCst);
                        }
                        Ok(Cmd::Enqueue(p)) => queue.push_back(p),
                        Ok(Cmd::Stop) => interrupt.store(false, Ordering::SeqCst),
                        Ok(Cmd::Quit) | Err(_) => {
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
