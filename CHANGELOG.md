# Changelog

## 0.2.0-rc.1 — release candidate

- Add exact PostgreSQL double-entry accounting, transactional reservations/outbox, immutable fee versions and atomic two-sided settlement.
- Add an internal mTLS gateway with account permissions, idempotent requests, durable event cursors and reconciliation barriers.
- Add opt-in funded matching with account ownership/STP, protected market execution and a durable decision feed independent of session replay.
- Add a recovering Go client with private atomic checkpoints, original mutation retries, application-before-ACK ordering and mandatory ReplayGap reconciliation.
- Add funded order/model/fault tests, PostgreSQL CI, Compose monitoring, maintenance checkpoints and verified backup/restore tools.
- Update rustls to 0.23.46; restrict ledger dependencies to the PostgreSQL driver.

### Migration and limits

Legacy v2 payloads and legacy storage codecs remain unchanged. Financial messages are additive capabilities; a financial gateway refuses a server without them. Financial storage/schema version 1 requires a new ledger and empty financial book. No migration from legacy session ownership or existing balances is provided. Market, stop-market and trailing-stop-market requests require an execution protection price in financial mode.

One active matcher and gateway are supported. Financial snapshots are verified against complete journal replay; journals are retained without online compaction. Manual funding and withdrawal record accounting only. Real funds, external transfers, public APIs, automatic failover and registry/binary publication are outside the pilot. Release remains gated by the recorded validation report.

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
