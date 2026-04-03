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
//! - [`router::Router`] — Single-threaded router for all symbols (simple, deterministic)
//! - [`router::ShardedRouterV2`] — Multi-threaded router with crossbeam channels.
//!   Symbols are distributed across N worker threads using `symbol_id % num_shards`.
//!   Each shard runs its own command loop with bounded channel (capacity 4096).
//!
//! ## Command Flow
//!
//! ```text
//! Client → Frame → Codec → Command → Router → Engine → MatchEvents → Codec → Frame → Client
//! ```

pub mod command;
pub mod engine;
pub mod router;
