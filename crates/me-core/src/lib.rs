#![forbid(unsafe_code)]
//! # me-core — Foundational Types for the Matching Engine
//!
//! This crate provides the core type definitions used across all other crates
//! in the matching engine workspace.
//!
//! ## Key Types
//!
//! - [`FixedPrice`] / [`FixedQty`] — Fixed-point decimal types (scale 10^8, no floating-point)
//! - [`Order`] — Complete order representation with all fields
//! - [`Trade`] — Executed trade record
//! - [`MatchEvent`] — Events emitted by the matching engine (accepted, rejected, filled, etc.)
//! - [`SymbolConfig`] — Per-symbol configuration (tick size, lot size, precision)
//! - [`EngineError`] — Typed error hierarchy
//!
//! ## Fixed-Point Arithmetic
//!
//! All prices and quantities use 8-decimal fixed-point representation:
//! - Price `50000.50` is stored as `i64` value `5_000_050_000_000`
//! - Quantity `0.5` is stored as `u64` value `50_000_000`
//! - Scale factor: `10^8 = 100_000_000`
//!
//! This eliminates floating-point rounding issues in the matching hot path.

pub mod errors;
pub mod events;
pub mod invariant;
pub mod order;
pub mod session;
pub mod symbol;
pub mod trade;
pub mod types;

// Re-export commonly used types at crate root.
pub use errors::EngineError;
pub use events::{EventBuffer, MatchEvent, RejectReason};
pub use invariant::{InvariantKind, InvariantPolicy};
pub use order::{Order, OrderStatus, OrderType, TimeInForce};
pub use session::SessionId;
pub use symbol::SymbolConfig;
pub use trade::Trade;
pub use types::{FixedPrice, FixedQty, OrderId, SequenceId, Side, SymbolId, TradeId};
