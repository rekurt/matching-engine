# Financial pilot v0.2.0-rc.1

Scope: exact PostgreSQL accounting, an internal mTLS gateway, funded matching, recovering Go sessions, fault tests and a reproducible operational environment. Start with a new ledger and empty financial book. One active gateway and matcher; no real assets, public access, automatic failover or online compaction.

## Acceptance

- [x] Immutable, balanced double-entry journals; nonnegative user funds and reservations; integer amounts at scale 10^20.
- [x] Atomic intent, reserve and outbox; original IDs and command bytes; immutable fee versions; conservative concurrent-amend coverage.
- [x] Account ownership/STP, execution protection, durable result feed and one-transaction two-sided settlement with cursor.
- [x] Internal API permissions, idempotency, source identity checks, fail-closed reconciliation and resource admission.
- [x] Separate recovering Go client with persisted mutations, application-before-ACK checkpoints and mandatory ReplayGap reconciliation.
- [x] Order/model matrix, legacy compatibility, process/network/storage failures and PostgreSQL integration CI.
- [x] Compose, readiness, metrics, local alert delivery, maintenance barrier, coordinated backup/restore and operator guide.
- [x] Formatting, Clippy, Rust/Go/Python/TypeScript/TLS checks, release build, audit, fuzz, rustdoc, mdBook and Docker.
- [x] One-hour complete financial-path measurement with immutable release binaries, midpoint kill/restart and final balance/reserve audit.
- [x] Supervised review and regression coverage for significant fixes.

The [validation report](../src/financial-validation.md) records workload, hardware, measurements, evidence and limits. The [pilot guide](../src/financial-pilot.md) describes operation and migration restrictions. Validation covers the RC's stated scope; it does not establish real-asset custody or external disaster-recovery guarantees.

## Publication

Implementation branch: `codex/financial-pilot`. Preserve existing history and merge only after CI on the final revision succeeds. The publication record is [GitHub Release v0.2.0-rc.1](https://github.com/rekurt/matching-engine/releases/tag/v0.2.0-rc.1); it contains source and documentation without registry publication or prebuilt binaries.
