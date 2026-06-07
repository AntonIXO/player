// SPDX-License-Identifier: GPL-3.0-or-later
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The file is not a recognized SACD/DSD container.
    #[error("not a SACD/DSD container: {0}")]
    NotSacd(String),

    /// Structurally valid but uses something we don't handle yet.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The container is recognized but its contents are inconsistent/corrupt.
    #[error("malformed SACD data: {0}")]
    Malformed(String),
}

pub type Result<T> = std::result::Result<T, Error>;
