# Financial pilot v0.2.0-rc.1

Approved scope: Rust ledger/gateway, PostgreSQL, all existing order types, protected market prices, maker/taker quote fees, exact 20-decimal amounts; internal mTLS only; fresh funded book; one active gateway/matcher; release candidate without real funds or automatic failover.

## Acceptance
- No unbalanced postings, negative user funds, duplicate settlements, lost acknowledged executions or premature reservation release.
- Preserve legacy protocol v2 and recovery; financial capabilities must fail closed.
- Durable commands retain original IDs/payloads. Settlement and cursor advance in one PostgreSQL transaction.
- Full order/lifecycle/fault matrix, legacy checks, one-hour full-path benchmark, coordinated backup/restore and reviewed RC.

## Implementation
- [x] Ledger amounts, immutable double-entry schema, migrations, reservations, settlement and invariants.
- [x] Financial engine mode, account ownership/STP, protected execution, durable decision export.
- [x] Internal gateway, authentication/ACLs, outbox, reconciliation and readiness.
- [x] Recovering Go client and gateway transport with persisted identity/cursors/pending bytes.
- [ ] Financial model, compatibility and real process/network/storage failure tests.
- [x] Compose, monitoring/alerts, maintenance barrier, backup/restore and runbooks.
- [ ] Full checks, one-hour benchmark, revmux review/fixes, merge and v0.2.0-rc.1.

Money: NUMERIC(78,0), scale 10^20; base raw quantity *10^12; quote raw price*raw quantity*10^4; fee raw price*raw quantity*bps. Fee schedule version frozen for order lifetime. No rebates. Funding defaults to 8 decimal places; preserve dust. Amend must cover fills both before and after amendment (10@200 ->20@150 can need3500).

Release remains blocked until every acceptance item passes; never claim financial readiness from foundation tests alone.

## Working checkpoint

- Implemented ledger/contracts, financial matching journal, mTLS gateway vertical slice and a separate recovering Go client. Work remains on this branch; no RC published.
- Initial ledger revmux found rejected-version settlement and unstable placement replay; both corrected with regressions. Financial account STP is required by the approved plan; the suggestion to restore optional legacy STP here was rejected.
- Second revmux found atomic initial publication, expired GTD admission, paused engine + pending outbox recovery, and a nondeterministic crash test. Fixed with atomic `active` directory publication, durable InvalidExpiry decisions, recoverable maintenance state, and an external TLS proxy barrier proving an unsubmitted outbox before restart.
- Current verification: 127 funded order/lifecycle cases and 182 independently modelled trades; 11 PostgreSQL tests; recovering SDK race tests, slow replay and subprocess checkpoint crash; real TLS/server SDK recovery; eight financial recovery tests plus subprocess engine initialization crash; seven fault checks (WAL acknowledgement loss, duplicates, disk reserve, rollback, corruption); isolated 8-MiB tmpfs ENOSPC; Compose PostgreSQL outage, local alert delivery and backup/restore including trades, iceberg and protected stops. Clippy, rustdoc, mdBook, legacy runtime, Python, Go/TypeScript examples and dependency audit have passed at their recorded checkpoints; re-run affected gates after changes.
- Third revmux findings addressed: retryable resource errors, progress-based replay deadline, real initialization/checkpoint interruption regressions, replaced-lock detection, certificate namespace and SDK documentation.
- Still required: latest affected full checks, final-binary one-hour benchmark, further revmux review/fixes, successful final CI, merge and RC tag/release. The baseline hour run is diagnostic and is not the final release measurement.
