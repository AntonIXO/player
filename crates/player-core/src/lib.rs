//! player-core: bit-perfect decode -> convert -> ALSA hw: output engine.
//! No GTK dependency; fully headless-testable.

pub mod convert;
pub mod decode;
pub mod devices;
pub mod dop;
pub mod dsd;
pub mod engine;
pub mod error;
pub mod fadvise;
pub mod format;
pub mod rt;
pub mod sink;

pub use convert::{append_bytes, OutFrames, Packer};
pub use decode::Decoder;
pub use devices::{auto_pick, has_usb_dac, list_devices, DeviceInfo, DeviceKind};
pub use dop::DopPacker;
pub use dsd::{is_dsd_path, open_dsd, DsdSource, DsdSpec};
pub use engine::{
    play_queue_blocking, run_playback, Cmd, Event, Flow, Player, Stats, DEFAULT_PERIOD,
    DEFAULT_PERIODS, MAX_PERIOD, MAX_PERIODS, MIN_PERIOD, MIN_PERIODS,
};
pub use rt::{CpuLatencyGuard, Sched};
pub use error::{Error, Result};
pub use format::{AlsaFmt, DeviceFormats, StreamSpec};
pub use sink::{capture::capture_raw, probe_formats, AlsaSink};
