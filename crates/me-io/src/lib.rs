//! # me-io — Wire Protocol and Network Servers
//!
//! This crate implements the binary wire protocol and async network servers
//! (TCP and Unix Domain Socket) for the matching engine.
//!
//! ## Wire Protocol
//!
//! Custom binary protocol optimized for minimal latency:
//!
//! ```text
//! ┌──────────┬──────────┬──────────┬──────────┬─────────────────┐
//! │ len: u32 │ ver: u8  │ type: u8 │ seq: u64 │ payload: [u8]   │
//! │ 4 bytes  │ 1 byte   │ 1 byte   │ 8 bytes  │ variable        │
//! └──────────┴──────────┴──────────┴──────────┴─────────────────┘
//! ```
//!
//! - **14-byte header**, `len` covers payload only
//! - All integers little-endian
//! - Protocol version negotiated via Handshake/HandshakeAck
//! - Monotonically increasing sequence numbers per connection
//!
//! ## Servers
//!
//! - [`tcp_server::TcpServer`] — Async TCP server (tokio), one task per connection
//! - [`uds_server::UdsServer`] — Unix Domain Socket server (lower latency for co-located processes)
//!
//! Both servers support graceful shutdown via watch channel and optional Prometheus metrics.
//!
//! See `docs/PROTOCOL.md` for the full wire protocol specification.

pub mod codec;
pub mod protocol;
pub mod tcp_server;
pub mod uds_server;
