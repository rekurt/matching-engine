//! # me-engine — Matching Engine Core and Router
//!
//! This crate contains the [`engine::Engine`] (one instance per trading pair) and
//! the routing infrastructure that distributes commands across engine instances.
//!
//! ## Engine
//!
//! Each `Engine` owns an [`me_book::book::OrderBook`] and processes commands
//! sequentially. There are **no locks** — strict single-thread-per-symbol guarantees
//! deterministic replay and eliminates contention.
//!
//! ## Routing
//!
//! - [`router::Router`] — Sharded multi-threaded router with integrated Risk + Audit.
//!   Symbols are distributed across N worker threads using `symbol_id % num_shards`.
//!   Each shard runs: Risk check → Matching → Async audit (SPSC send).
//!
//! ## Command Flow
//!
//! ```text
//! Client → Frame → Codec → Command → Router → Engine → MatchEvents → Codec → Frame → Client
//! ```

pub mod affinity;
pub mod command;
pub mod engine;
pub mod router;

#[cfg(feature = "financial")]
pub mod financial;
