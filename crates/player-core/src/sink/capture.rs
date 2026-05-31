//! Test-only ALSA capture, used to read back the snd-aloop loopback for
//! bit-perfect transport verification. Reads exactly `n_frames` of raw bytes in
//! the negotiated format.

use alsa::pcm::PCM;
use alsa::Direction;

use crate::error::Result;
use crate::format::StreamSpec;

/// Open `device` for capture with `spec`, then read exactly `n_frames` frames,
/// returning the raw interleaved bytes (same layout the sink writes).
pub fn capture_raw(
    device: &str,
    spec: StreamSpec,
    n_frames: usize,
    period: i64,
    periods: i64,
) -> Result<Vec<u8>> {
    let pcm = PCM::new(device, Direction::Capture, false)?;
    super::alsa::configure_hw(&pcm, spec, period, periods)?;

    let frame_bytes = spec.bytes_per_frame();
    let mut out = vec![0u8; n_frames * frame_bytes];

    let io = pcm.io_bytes();
    pcm.prepare()?;
    pcm.start()?;

    let mut off = 0;
    while off < out.len() {
        match io.readi(&mut out[off..]) {
            Ok(frames) => off += frames * frame_bytes,
            Err(e) => pcm.try_recover(e, true)?,
        }
    }
    Ok(out)
}
