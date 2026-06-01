use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tag error: {0}")]
    Tag(#[from] lofty::error::LoftyError),

    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),

    #[error("library: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
