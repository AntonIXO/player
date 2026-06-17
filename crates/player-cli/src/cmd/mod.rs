//! Subcommand handlers, split by area. `main.rs` parses the CLI and dispatches
//! into these modules; the byte-building helpers shared by `play`/`dump` and the
//! loopback verifiers live here.

pub mod audio;
pub mod devices;
pub mod library;
pub mod loopback;

use std::path::Path;

use player_core::{
    append_bytes, is_dsd_path, open_dsd, probe_formats, Decoder, DeviceFormats, DopPacker, Packer,
    StreamSpec,
};

/// Frame cap for a `seconds` limit (`None` = whole file / no cap).
pub(crate) fn max_frames(spec_rate: u32, seconds: f64) -> Option<u64> {
    (seconds > 0.0).then(|| (seconds * f64::from(spec_rate)) as u64)
}

/// Decode (up to `max_frames`) into the exact bytes the sink would write.
pub(crate) fn decode_to_bytes(
    file: &Path,
    maxf: Option<u64>,
) -> player_core::Result<(Vec<u8>, StreamSpec, usize)> {
    let mut dec = Decoder::open(file)?;
    let spec = dec.spec;
    let channels = spec.channels as usize;
    let mut packer = Packer::new(spec.fmt);
    let mut block: Vec<i32> = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let mut frames: u64 = 0;

    while dec.next(&mut block)? {
        let mut samples = block.as_slice();
        if let Some(m) = maxf {
            if frames >= m {
                break;
            }
            let remaining = (m - frames) as usize * channels;
            if samples.len() > remaining {
                samples = &samples[..remaining];
            }
        }
        if samples.is_empty() {
            continue;
        }
        let out = packer.pack(samples);
        append_bytes(&mut bytes, &out);
        frames += (samples.len() / channels) as u64;
    }
    let n_frames = bytes.len() / spec.bytes_per_frame();
    Ok((bytes, spec, n_frames))
}

/// Build the bit-perfect DoP byte stream for a DSD file, choosing the DoP
/// container from `device`'s formats. The DSD analogue of [`decode_to_bytes`]:
/// the bytes are exactly what the sink writes, so the loopback compare is a
/// transport bit-perfect proof for DSD.
pub(crate) fn dsd_dop_bytes(
    file: &Path,
    device: &str,
    seconds: f64,
) -> player_core::Result<(Vec<u8>, StreamSpec, usize)> {
    let formats = probe_formats(device).unwrap_or_else(|_| DeviceFormats::all());
    let fmt = formats.choose(24).ok_or_else(|| {
        player_core::Error::Unsupported("device can't carry DoP (no 24/32-bit format)".into())
    })?;
    let mut src = open_dsd(file, None)?;
    let dspec = src.spec();
    let spec = dspec.dop_spec(fmt);
    let frame_bytes = spec.bytes_per_frame();
    let maxf = max_frames(spec.rate, seconds);
    let mut dop = DopPacker::new(fmt, dspec.channels);
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    while src.next(&mut buf)? {
        out.extend_from_slice(dop.pack(&buf));
        if let Some(m) = maxf {
            if out.len() / frame_bytes >= m as usize {
                out.truncate(m as usize * frame_bytes);
                break;
            }
        }
    }
    let n = out.len() / frame_bytes;
    Ok((out, spec, n))
}

/// Pick the right byte builder for `file` (PCM decode vs DSD→DoP), capped to
/// `seconds`. The returned bytes are exactly what the sink writes, so a loopback
/// compare against them proves transport bit-perfect for either source kind.
pub(crate) fn source_to_bytes(
    file: &Path,
    device: &str,
    seconds: f64,
) -> player_core::Result<(Vec<u8>, StreamSpec, usize)> {
    if is_dsd_path(file) {
        dsd_dop_bytes(file, device, seconds)
    } else {
        let d = Decoder::open(file)?;
        decode_to_bytes(file, max_frames(d.spec.rate, seconds))
    }
}
