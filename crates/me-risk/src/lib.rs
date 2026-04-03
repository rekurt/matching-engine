//! # me-risk — Risk Management
//!
//! Pre-trade risk checks including:
//!
//! - **Rate limiting** — Token-bucket per symbol (`max_orders_per_second`)
//! - **Position limits** — Maximum position size per symbol
//! - **Order size limits** — Min/max order quantity validation

pub mod config;
pub mod engine;
pub mod limits;
