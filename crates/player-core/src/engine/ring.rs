//! Wait-free SPSC byte ring (rtrb) connecting the decode thread to the RT audio
//! thread. It carries already-packed output bytes in the negotiated wire format
//! — a pure FIFO, no transform — so per-segment output stays byte-identical to
//! the v1 single-thread path. The ring only ever holds *whole frames* (capacity
//! and every push are frame-aligned), which makes drain detection on the audio
//! side exact.

use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::format::StreamSpec;

/// Ring depth in milliseconds of audio. Deep enough to absorb decode jitter so
/// backpressure lands on the (non-RT) decoder, never the audio thread.
pub(crate) const RING_MS: u32 = 500;

/// Build a byte ring sized for ~[`RING_MS`] of `spec`, rounded to whole frames.
pub(crate) fn ring_for_spec(spec: StreamSpec) -> (Producer<u8>, Consumer<u8>) {
    let fb = spec.bytes_per_frame();
    let frames = (spec.rate as usize * RING_MS as usize / 1000).max(2048);
    RingBuffer::<u8>::new(frames * fb)
}

/// Push `bytes` (a whole number of frames) into the ring, parking briefly while
/// it is full. `fb` is bytes-per-frame; pushes stay frame-aligned so the ring
/// never holds a partial frame. `abort()` is polled while parked; returns
/// `false` if aborted or the consumer went away (audio thread flushed/stopped),
/// in which case the block was *not* fully written and the caller must drop the
/// current track.
pub(crate) fn push_block(
    prod: &mut Producer<u8>,
    mut bytes: &[u8],
    fb: usize,
    abort: &mut dyn FnMut() -> bool,
) -> bool {
    while !bytes.is_empty() {
        if prod.is_abandoned() {
            return false;
        }
        let free = (prod.slots() / fb) * fb; // whole frames only
        if free == 0 {
            if abort() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let n = free.min(bytes.len());
        match prod.write_chunk_uninit(n) {
            Ok(chunk) => {
                let written = chunk.fill_from_iter(bytes[..n].iter().copied());
                bytes = &bytes[written..];
            }
            Err(_) => return false,
        }
    }
    true
}
