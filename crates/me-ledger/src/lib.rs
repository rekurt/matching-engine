#![forbid(unsafe_code)]

mod amount;
mod gateway;
pub use gateway::{GatewayLease, LedgerStatus, OutboxItem};
mod model;
mod mutation;
pub use mutation::Amend;
mod settlement;
mod store;
pub use amount::{amend_buy_reserve, Amount};
pub use model::*;
pub use store::Ledger;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] me_financial::Error),
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("amount exceeds NUMERIC(78,0)")]
    Overflow,
    #[error("idempotency key reused with a different payload")]
    Conflict,
    #[error("insufficient available funds")]
    InsufficientFunds,
    #[error("reconciliation required: {0}")]
    Reconciliation(String),
    #[error("database: {0}")]
    Database(#[from] sqlx::error::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("migration: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}
