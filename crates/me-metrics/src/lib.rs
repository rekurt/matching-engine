//! # me-metrics — Prometheus Metrics
//!
//! Provides 50+ Prometheus metrics for monitoring the matching engine:
//!
//! - **Orders**: `me_orders_total`, `me_orders_rejected_total`, processing duration
//! - **Trades**: `me_trades_total`, volume, price
//! - **Book state**: bid/ask levels, spread, best bid/ask, depth
//! - **Connections**: active connections, bytes in/out, protocol errors
//! - **WAL**: writes, flush duration, snapshot saves
//! - **Shards**: queue depth, commands per shard
//!
//! Metrics are exported via HTTP in Prometheus text format.

pub mod engine_metrics;

pub use engine_metrics::EngineMetrics;
