//! player-core: bit-perfect decode -> convert -> ALSA hw: output engine.
//! No GTK dependency; fully headless-testable.

pub mod convert;
pub mod decode;
pub mod engine;
pub mod error;
pub mod format;
pub mod rt;
pub mod sink;

pub use convert::{append_bytes, OutFrames, Packer};
pub use decode::Decoder;
pub use engine::{
    play_queue_blocking, run_playback, Cmd, Event, Flow, Player, Stats, DEFAULT_PERIOD,
    DEFAULT_PERIODS,
};
pub use rt::Sched;
pub use error::{Error, Result};
pub use format::{AlsaFmt, DeviceFormats, StreamSpec};
pub use sink::{capture::capture_raw, probe_formats, AlsaSink};
