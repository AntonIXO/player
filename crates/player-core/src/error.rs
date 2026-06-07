use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("decode error: {0}")]
    Symphonia(#[from] symphonia::core::errors::Error),

    #[error("ALSA error: {0}")]
    Alsa(#[from] alsa::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The device could not be configured for the exact source rate. We refuse
    /// to let ALSA resample, so this is a hard error rather than silent SRC.
    #[error("device rejected exact rate {requested} Hz (set {actual} Hz) — refusing to resample")]
    RateMismatch { requested: u32, actual: u32 },

    #[error("no decodable audio track found")]
    NoTrack,

    /// A DSD/SACD source (`.dsf`/`.dff`/`.iso`) could not be read.
    #[error("DSD source error: {0}")]
    Dsd(String),

    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;
