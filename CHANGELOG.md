# Changelog

## 0.1.0 — 2026-10-10

First public release.

- Price-time matching with limit, market, stop, trailing-stop and iceberg orders; GTC, IOC, FOK and GTD.
- Sharded symbol processing, fixed-point arithmetic and per-symbol risk controls.
- Protocol v2 over TCP/UDS, TLS/mTLS, session replay, order snapshots and durable mutation deduplication.
- Decision journals and recovery checkpoints, including ownership and durable halt/resume state.
- Prometheus metrics, HTTP health/admin endpoints, Go SDK and four language examples.
- Bounded matching work and session admission; regression coverage for recovery, protocol and delivery.
- Consolidated English handbook and corrected snapshot completion handling in the runtime smoke client.
- Corrected TLS build target and documented control-frame sequences separately from replay cursors.
- Updated rustls to 0.23.45 to address RUSTSEC-2026-0285.

### Compatibility and limits

This release retains the current protocol v2 and storage formats. It does not migrate older command-only or compacted journals. Changed historical matching semantics can also prevent event-equivalence replay; test upgrades against a copy of existing storage. See the [deployment guide](docs/src/deployment.md).

The service does not provide balances, settlement, replication or failover. Durable request history and journals grow with activity; no online compaction is provided.
