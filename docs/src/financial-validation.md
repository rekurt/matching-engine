# Financial pilot validation

Validation of `v0.2.0-rc.1` on 2026-10-15. This is a local financial pilot without real assets, public access or automatic failover.

## Revision and environment

The tested Rust/Go service source is commit `fdc47efe35374cc7b25fc1807c94e1a780b534a4`. Commit `67bf17c90b9262f9c4e2220af7a1059f02d0f227` adds backup checksum verification and documentation corrections; it does not change those binaries. Commit `c0cf702e8aa74ab825db834dbe271f38a3e06cea` makes runtime tests wait for protocol completion and adds operator funding steps. The release tag identifies the complete source and handbook.

The hour run uses release builds with all workspace features on an Apple M5 Pro (18 logical/physical CPUs, 24 GiB RAM), macOS 26.6.2 arm64, Rust 1.98.1 / LLVM 22.1.8. Matcher and gateway run natively. PostgreSQL 17.11 runs on Docker Desktop 29.7.2 with 8 vCPUs and 14,636,363,776 bytes of VM memory, using a private persistent volume. This shared workstation also ran review and validation tasks, including a diagnostic soak during the first part of the measurement; it was not an isolated capacity benchmark.

## Correctness and recovery

| Check | Result |
|---|---|
| Rust workspace, all features | 674 tests passed; 11 PostgreSQL tests run separately; one subprocess helper intentionally ignored |
| PostgreSQL integration | 11 passed: exact arithmetic, balanced immutable postings, frozen fees, reservations, request conflicts, concurrent funds, settlement and rollback checks |
| Financial order model | 130 order/TIF/flag and lifecycle cases, including expected rejections; 185 trades independently checked for both accounts and fees |
| GTD recovery | 600 due orders across two instruments, bounded expiry decisions, restart between batches and no execution of expired orders |
| Process/network faults | Unsent outbox; WAL confirmation lost before settlement; original-byte retry; duplicate decision; restart after settlement; partial iceberg, batch, amend, trailing stop and GTD recovery |
| Resource/integrity faults | Actual ENOSPC on isolated 8 MiB tmpfs; reserve retained and retryable; incomplete tail recovery; complete journal corruption and source rollback fail closed |
| Go SDK | Race tests, actual TLS/server reconnect, slow continuous replay, checkpoint subprocess crash, replaced-lock detection and mandatory ReplayGap reconciliation |
| Operations | PostgreSQL outage, exclusive gateway lease, mTLS ACLs, local Alertmanager delivery, coordinated backup into a new database/directory, overwrite refusal and checksum/model validation |
| Legacy integrations | Rust, Go, Python and TypeScript examples; 30 TLS/runtime tests (two intentional example skips); unchanged v2 payloads and legacy journal/snapshot regression tests |
| Build and dependencies | Formatting, Clippy with warnings denied, release build, rustdoc, mdBook, Docker and dependency audit passed |
| Fuzzing | AddressSanitizer decoder runs: 6,208,681 pending-state and 6,489,763 handshaked-state executions, 61 seconds each, no crashes |
| Review and secret scan | Seven supervised revmux rounds, four reporting lenses per round without degraded participants; significant fixes re-reviewed. Git history scan found no secrets. This is not an external security audit. |

Codebase Memory was used for structural discovery with path coverage checks. Its 2026-10-10 index predates the financial modules and excludes documentation/scripts; those areas and stale ranges were read directly. Clean graph coverage was not treated as proof of completeness.

Linux CI also runs the PostgreSQL tests, full financial matrix, short soak, real ENOSPC and Compose recovery. See [the service validation run](https://github.com/rekurt/matching-engine/actions/runs/38662813463). The final revision must pass CI before publication.

## One-hour full-path measurement

| Measurement | Result |
|---|---|
| Duration | 3600.092 s |
| Settled trades | 13,354 |
| Confirmed placement commands | 26,708 |
| Observed commands/s | 7.419 |
| Complete-pair latency p50 / p95 / p99 | 251.108 / 364.452 / 375.420 ms |
| Midpoint restart to readiness and receipt replay | 0.444 s |
| Errors | 0 |
| Matcher journal growth | 42,576,445 bytes |
| Final maintenance snapshot | 14,413,961 bytes |
| Final engine directory | 56,990,754 bytes |
| PostgreSQL relation growth | 195,624,960 bytes |

[Machine-readable results and binary SHA-256 digests](reports/financial-validation.json).

The single driver submits a resting buy then a crossing sell for 0.00001 BTC at 100 USDT, using separate funded accounts. Maker fees are 10 bps and taker fees 20 bps. Each request establishes a new mTLS connection. Timing includes both HTTP placements and polling until their intents settle; it measures a complete pair, rather than one matching operation. Gateway polling has a 100 ms interval. At most one resting order is present per pair; this does not measure large books or concurrent stop/iceberg bursts. Throughput is observed command throughput under this sequential workload, not a saturation limit.

At the midpoint the driver kills both processes, restarts them and retries the original requests. The restart timer ends after readiness and verification of both original receipts; completion of both orders is checked separately before the pair is counted. Final decimal balances, fees, zero outstanding reserves, the maintenance watermark and the accounting audit must agree. PostgreSQL size growth counts tables, indexes and TOAST in the test schema; it excludes PostgreSQL WAL and volume overhead. Matcher journal growth is measured separately. Both histories remain retained; no online compaction is implemented.

## Reproduction

Use a disposable PostgreSQL database and pass its URL through a private environment:

```sh
cargo build --release --workspace --all-features --locked
cargo test -p me-ledger --test postgres --locked -- --ignored
python3 scripts/financial-matrix.py --bin-dir target/release --output /tmp/pilot-matrix
python3 scripts/financial-faults.py --bin-dir target/release --output /tmp/pilot-faults
python3 scripts/financial-soak.py --bin-dir target/release --seconds 3600 --output /tmp/pilot-hour
```

These commands require `LEDGER_TEST_DATABASE_URL`; the scripts create isolated schemas. Retain their reports and private state outside Git. For actual disk exhaustion and the Compose backup rehearsal, follow the [pilot guide](financial-pilot.md#local-operational-rehearsal). Generated test identities expire after two days.

## Release boundary

Start with a new financial book and ledger. Financial storage/schema version 1 has no legacy-account migration or automatic downgrade. Existing v2 session payloads and legacy storage codecs remain unchanged. Financial messages require capability negotiation and mTLS. Protected market prices, immutable nonnegative quote fees and conservative amendment reserves are mandatory.

One active matcher and gateway are supported. Financial mode does not expose configurable `me-risk` limits. Snapshots are checked against full journal replay and do not accelerate recovery. Funding and withdrawals record accounting entries only. Production alert destinations, real asset transfers, public APIs, replication, automatic failover and online compaction are outside this release.
