#![forbid(unsafe_code)]
//! # me-io — Wire Protocol and Network Servers
//!
//! This crate implements the binary wire protocol and async network servers
//! (TCP and Unix Domain Socket) for the matching engine.
//!
//! ## Wire Protocol
//!
//! Protocol v2 uses a 22-byte header: payload length (`u32`), version (`u8`),
//! message type (`u8`), sequence (`u64`) and request ID (`u64`). All integers
//! are little-endian; the length excludes the header.
//!
//! Client sequences increase within a connection. Positive server sequences
//! identify the session replay stream; sequence zero is out-of-band.
//!
//! ## Servers
//!
//! - [`tcp_server::TcpServer`] — Async TCP server (tokio), one task per connection
//! - [`uds_server::UdsServer`] — Unix Domain Socket server for co-located processes
//!
//! Both servers support graceful shutdown via watch channel and optional Prometheus metrics.
//!
//! See `docs/src/protocol.md` for the full wire protocol specification.

pub mod codec;
pub mod protocol;
pub mod session_store;
pub mod tcp_server;
pub mod uds_server;
