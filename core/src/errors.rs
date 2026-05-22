use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error, Serialize)]
#[serde(tag = "kind", content = "message")]
#[serde(rename_all = "snake_case")]
pub enum HikerError {
    #[error("io: {0}")]
    Io(String),
    #[error("path escapes vault: {0}")]
    PathEscape(String),
    #[error("not utf-8: {0}")]
    NotUtf8(String),
    #[error("disk drift: file changed since load (expected hash {expected}, found {found})")]
    DiskDrift { expected: String, found: String },
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("config: {0}")]
    Config(String),
}

impl From<std::io::Error> for HikerError {
    fn from(e: std::io::Error) -> Self {
        HikerError::Io(e.to_string())
    }
}
