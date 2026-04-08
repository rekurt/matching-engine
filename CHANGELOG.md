# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Protocol v2** — 22-byte frame header with `request_id` field for idempotency and response correlation
- Session recovery: `ResumeSession` (0x06), `SessionResumed` (0x1B), replay buffer
- Batch operations: `BatchPlaceOrder` (0x0B, max 20), `BatchCancelOrder` (0x0C, max 50)
- Order query: `GetOrderStatus` (0x07), `GetOpenOrders` (0x09), snapshot protocol
- Order flags: `POST_ONLY` (0x01), `REDUCE_ONLY` (0x02), `MAKER_OR_CANCEL` (0x04)
- `Acknowledge` (0x08) for cumulative ACK, `FlowControlWarning` (0x23)
- TLV extensions in `HandshakeAck`: capabilities bitmask, session_id, heartbeat_ms
- Heartbeat timeout management with configurable interval/timeout
- Invariant handling system: configurable per-symbol policy (`strict`, `halt_symbol`, `continue`)
- `SessionId` (128-bit) for per-client OpenOrders queries
- `me-clock` crate — clock abstraction (SystemClock, CoarseClock, RdtscClock)
- Optional TLS support via `tls` feature (rustls, mTLS)
- New error codes: HeartbeatTimeout(7), TlsError(8), InvalidRequestId(9), SessionExpired(10), BufferExhausted(11), InternalError(12)
- New reject reasons: WouldTakeMarket(11), InternalError(12)
- TOML configuration file support (`--config` CLI flag)
- Admin API authentication via `X-Admin-Key` header
- Connection drain timeout on graceful shutdown (configurable)
- Comprehensive bilingual documentation (English + Russian)
- Client examples in Python, TypeScript, and Rust
- `CONTRIBUTING.md` and `CHANGELOG.md`
- Workspace-level Cargo metadata (license, repository, description)
- Stress and realistic benchmark suites

### Changed
- Frame header expanded from 14 to 22 bytes (added `request_id: u64`)
- `PROTOCOL_VERSION` incremented from 1 to 2
- `Order` struct expanded from 72 to 88 bytes (added `flags`, `client_session_id`)
- `HandshakeAck` payload expanded from 1 to 41 bytes (TLV extensions)
- CLI arguments now override config file values
- Admin endpoints (`/admin/*`) protected with middleware when API key is set
- Codec `unwrap()` calls replaced with `expect()` with context messages

### Fixed
- Go client wire protocol incompatibility (13-byte vs 14-byte header)
- Go client missing Handshake/HandshakeAck support
- Broken documentation links in README.md
- Documentation inconsistencies across protocol version, ports, crate list

## [0.1.0] — 2025-05-01

### Added
- Core matching engine with price-time priority algorithm
- Order types: Limit, Market, Stop-Limit, Stop-Market
- Time-in-force: GTC, IOC, FOK
- Order book with BTreeMap + Slab + intrusive linked list
- ShardedRouterV2 for multi-threaded symbol processing
- Binary wire protocol v1 (14-byte header, little-endian)
- TCP and Unix Domain Socket servers (tokio)
- Write-Ahead Log (WAL) with CRC32 checksums
- Atomic snapshot save/load
- Prometheus metrics (50+ metrics)
- Admin REST API (health, halt, resume)
- Criterion benchmarks (throughput, latency, e2e)
- Realistic order flow generator (me-datagen)
- Risk management framework (me-risk)
- Audit logging (me-audit)
- Go client library
- Property-based testing with proptest
