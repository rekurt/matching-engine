#![forbid(unsafe_code)]
mod model;
pub use model::*;
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid financial contract: {0}")]
    Invalid(&'static str),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(feature = "transport")]
pub mod transport;
