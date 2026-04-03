# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- TOML configuration file support (`--config` CLI flag)
- Admin API authentication via `X-Admin-Key` header
- Connection drain timeout on graceful shutdown (configurable)
- Comprehensive bilingual documentation (English + Russian):
  - `docs/PROTOCOL.md` — Wire protocol specification
  - `docs/ARCHITECTURE.md` — System architecture
  - `docs/DEPLOYMENT.md` — Deployment and configuration guide
  - `docs/API.md` — Admin HTTP API reference
- Client examples in Python, TypeScript, and Rust
- `CONTRIBUTING.md` and `CHANGELOG.md`
- Workspace-level Cargo metadata (license, repository, description)

### Changed
- CLI arguments now override config file values
- Admin endpoints (`/admin/*`) protected with middleware when API key is set
- Codec `unwrap()` calls replaced with `expect()` with context messages

### Fixed
- Go client wire protocol incompatibility (13-byte vs 14-byte header)
- Go client missing Handshake/HandshakeAck support
- Broken documentation links in README.md

## [0.1.0] — 2025-05-01

### Added
- Core matching engine with price-time priority algorithm
- Order types: Limit, Market, Stop-Limit, Stop-Market
- Time-in-force: GTC, IOC, FOK
- Order book with BTreeMap + Slab + intrusive linked list
- ShardedRouterV2 for multi-threaded symbol processing
- Binary wire protocol (14-byte header, little-endian)
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
