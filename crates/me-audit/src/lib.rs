//! # me-audit — Audit Logging
//!
//! Provides a structured audit trail for all matching engine operations.
//! Events are serialized to JSON for compliance and debugging:
//!
//! - Order submitted, accepted, rejected
//! - Trades executed
//! - Orders filled, partially filled, cancelled, amended
//! - Stop orders triggered
//! - Trading halted/resumed

pub mod logger;
pub mod types;
