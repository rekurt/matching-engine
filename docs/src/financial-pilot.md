# Financial pilot

The `codex/financial-pilot` branch adds a funded mode alongside the legacy session API. Release validation is still in progress. Start with a new PostgreSQL ledger and an empty financial data directory. This mode does not connect to banks, blockchains or real assets, and does not provide automatic failover.

## Authority and limits

PostgreSQL owns balances, reservations, immutable postings and settlement cursors. The matcher journal owns execution decisions. One gateway owns the database lease and the matcher connection. Financial mode serializes all configured symbols in shard `0`; legacy sharding is unchanged.

Every decision is applied as one database transaction, including both counterparties, maker/taker fees, order states and cursor. A duplicate decision has no financial effect. Unknown orders, rejected versions, identity mismatches and broken journal lineage require reconciliation. A timeout or disconnect never releases a reservation.

Amounts are signed integers at scale `10^20`, stored in PostgreSQL `NUMERIC(78,0)`. HTTP monetary values are decimal strings. Instruments retain the engine's eight-decimal price and quantity units. Funding and withdrawals default to eight fractional places; an asset can explicitly select another precision, up to twenty. Smaller settlement dust remains available in the account.

Fees are nonnegative integer basis points, charged in the quote asset, without rebates. Each order retains its fee schedule. A buy reserves price × full remaining quantity plus the larger maker/taker fee. A sell reserves the full base quantity and pays its fee from quote proceeds. Iceberg hidden quantity and inactive stops are fully funded. Market, stop-market and trailing stop-market orders require a positive execution ceiling for buys or floor for sells; an unfilled remainder is cancelled.

Amend reserves the largest possible combined cost of old-version fills followed by new-version fills. For example, changing `10 × 200` to `20 × 150` can require `3500`, before fees. An old-version fill cannot release the coverage still needed by a pending amendment. Stop and iceberg amend restrictions remain enforced by the engine.

## Internal HTTP API

Build `me-server` with `--features financial` and build `me-gateway`. The server's `--financial-config PATH` selects the isolated financial listener; it does not open legacy TCP or UDS order listeners. The gateway uses `--config PATH` and reads PostgreSQL credentials from `ME_LEDGER_DATABASE_URL`, or the configured environment variable name.

Both financial connections require TLS 1.3 and client certificates. Configuration binds the SHA-256 fingerprint of each leaf certificate to a service identity. The gateway's `principals` map grants explicit `read`, `trade`, `funding` and `admin` permissions, plus an `accounts` allowlist or explicit `all_accounts: true`. An admin permission does not implicitly grant trading or funding.

| Method | Route | Permission | Result |
|---|---|---|---|
| GET | `/health/live` | Known certificate | Process is running |
| GET | `/health/ready` | read | Database, lease, recovery and settlement admission state |
| GET | `/metrics` | read | Prometheus gateway metrics |
| POST | `/v1/admin/accounts` | admin | Stable account UUID for `{ "key": "...", "name": "..." }` |
| POST | `/v1/admin/maintenance` | admin | `{ "enabled": true/false }`; returns a durable matcher watermark |
| POST | `/v1/admin/reconcile` | admin | Verify accounting and matching cursor while the engine is paused |
| GET | `/v1/accounts/{account}/balances` | read + account | Available and reserved decimal strings by asset |
| POST | `/v1/accounts/{account}/orders` | trade + account | Durable placement receipt, HTTP 202 |
| GET | `/v1/accounts/{account}/orders/{id}` | read + account | Current order state |
| POST | `/v1/accounts/{account}/orders/{id}/cancel` | trade + account | `{ "key": "..." }`; saved request UUID, HTTP 202 |
| POST | `/v1/accounts/{account}/orders/{id}/amend` | trade + account | Key and optional decimal `price` / `quantity`, HTTP 202 |
| GET | `/v1/accounts/{account}/requests/{key}` | read + account | Current intent status |
| GET | `/v1/accounts/{account}/events` | read + account | Account-filtered decision pages |
| POST | `/v1/accounts/{account}/funding` | funding + account | Balanced external-input journal |
| POST | `/v1/accounts/{account}/withdrawals` | funding + account | Balanced external-output journal; no external transfer |
| POST | `/v1/accounts/{account}/batch/place` | trade + account | Array of 1–20 placement requests, per-item receipts/errors |
| POST | `/v1/accounts/{account}/batch/cancel` | trade + account | Array of 1–50 `{ "key": "...", "order_id": 1 }` requests |

A placement request is:

```json
{
  "key": "client-intent-001",
  "order": {
    "symbol": 0,
    "side": "buy",
    "order_type": "limit",
    "time_in_force": "GTC",
    "price": "100.00",
    "quantity": "1.00000000"
  }
}
```

Order types are `limit`, `market`, `stop_limit`, `stop_market`. TIF is `GTC`, `IOC`, `FOK` or `GTD`. Optional fields are `protection_price`, `stop_price`, `trail_offset`, `display_quantity`, Unix-nanosecond `expire_at`, and `flags` containing `post_only` or `maker_or_cancel`. Engine validation still determines legal combinations. An expired GTD is durably rejected before matching.

Funding and withdrawal bodies contain `asset`, decimal-string `amount`, and a globally unique `external_reference`. Reusing a reference for a different transfer is a conflict. The operation records accounting only.

HTTP 202 confirms a saved intent and funded outbox entry; it does not promise execution or final acceptance. Query the request and order after a timeout. Repeat the same key and validated intent, rather than create a new key. A different intent under the same key returns 409. Placement retries return the original receipt even after the order changes; current state has its own endpoint. Batch items are independent; no atomicity across symbols is promised.

Event queries accept `after` (decision sequence), `digest` (the preceding decision hash), and `limit` (1–256, default 32). Begin at sequence zero and a digest of 64 zeroes. A page contains account-specific events grouped by decision and the next cursor. Commit all events in a decision with the cursor in one application transaction. Re-reading a page is allowed; never infer settlement from an open-order snapshot.

## Financial wire and storage

The existing v2 payloads remain unchanged. Financial mode requires a v2 handshake payload containing version byte `2` and the capabilities TLV: tag `1` (`u16 LE`), length `8` (`u16 LE`), value `u64 LE`. Required bits are TLS (`5`), financial commands (`8`), durable decisions (`9`), protected markets (`10`) and maintenance barrier (`11`). A peer missing a required bit is rejected.

New messages retain the 22-byte v2 header. JSON payloads use the internal funded contracts from `me-financial`; raw prices and quantities there use `10^8` units.

| Type | Meaning |
|---|---|
| `0x30` | Funded place/cancel/amend with immutable request, account, order, reserve, fee and version identities |
| `0x31` | Decision page query: `after`, `digest`, `limit` |
| `0x32` | Engine identity, instrument fingerprint, capabilities and watermarks |
| `0x33` | Persist a maintenance barrier or resume |
| `0x38` | Correlated reply: `result` or `error` |

Mutation frame sequence and request ID are the persisted outbox wire request ID. Control queries use sequence zero and a nonzero correlation ID; replies use sequence zero. Financial replay comes from the durable feed, independently of legacy session replay. The gateway persists the complete encoded mutation before sending it and retransmits those bytes unchanged.

The matcher publishes an `active` directory only after its manifest and empty journal are durable. Incomplete `.initializing-*` directories were never visible to clients. A published generation missing either file fails closed. `active/financial.json` binds storage version, engine UUID and instrument fingerprint. `active/financial.wal` starts with `MEFIN001`; each record carries length, body CRC, WAL sequence, header CRC and JSON decision/replay data. Only an incomplete final write is truncated. Complete corrupt records or replay divergence stop recovery.

Event identity is `(engine UUID, shard, WAL sequence, event index)`. Account event indices enumerate command outcomes, fills, then order outcomes. A fill appears once in a financial decision, even though both counterparties receive notifications. Order bindings let the ledger check the account, reservation and frozen fee version before posting funds.

The financial database has an explicit schema version. SQLx migration checksums and version checks reject incompatible binaries or modified migrations. Legacy WAL and snapshots retain their existing format. Financial mode starts with fresh storage; it does not import a legacy book.

## Reproducible checks

PostgreSQL integration tests run separately from ordinary workspace tests:

```sh
LEDGER_TEST_DATABASE_URL=postgresql://... cargo test -p me-ledger --test postgres -- --ignored
cargo build -p me-server --features financial -p me-gateway
LEDGER_TEST_DATABASE_URL=postgresql://... python3 scripts/financial-smoke.py
```

The smoke creates a private schema and test CA. It verifies a blocked, unsubmitted outbox before killing the gateway, matcher maintenance recovery, a hard matcher restart, both trading balances, fees, original receipt replay and separate funding permissions. Test credentials and state are written outside the repository when `--output` points there.

`financial-soak.py` accepts `--seconds` (default 3600), `--bin-dir`, and `--output`. Its report describes the complete HTTP/reservation/matcher/settlement workload, machine, throughput, percentiles, journal growth and midpoint restart time. Polling and client TLS overhead are part of those measurements. They are not an engine-only latency figure or an SLA.

## Local operational rehearsal

The Compose deployment binds only the internal HTTPS API and monitoring consoles to
loopback. The matcher and PostgreSQL have no published ports. Its generated CA and
certificates are disposable and expire in two days. Production identities, account
allowlists, funding privileges and alert recipients require explicit configuration.

```sh
python3 scripts/financial-configure.py --output .pilot
docker compose --env-file .pilot/compose.env -f ops/financial/compose.yml up --build -d
curl --cacert .pilot/ca.pem --cert .pilot/admin.pem --key .pilot/admin.key \
  https://localhost:9443/health/ready
```

The generator refuses to replace an existing directory. Keep `.pilot` outside Git;
it contains private keys, database credentials, the engine journal and configuration.
Never regenerate identities over an existing ledger. The example trader has access
to all pilot accounts; replace this with explicit account UUIDs before a shared pilot.
The read-only monitoring identity cannot trade, fund or administer accounts.

Create a pilot account with the administrator identity:

```sh
curl --fail --cacert .pilot/ca.pem --cert .pilot/admin.pem --key .pilot/admin.key \
  -H 'Content-Type: application/json' -d '{"key":"pilot-account-001","name":"Pilot account"}' \
  https://localhost:9443/v1/admin/accounts
```

Save the returned UUID as `ACCOUNT_ID`. Provisioning keys are scoped by the
administrator certificate's SHA-256 fingerprint. Retain the UUID mapping across
certificate rotation; repeating provisioning under a new certificate creates a
different namespace.

The generated local administrator also has an explicit funding permission. Use
that permission to record initial test funds with a unique external reference:

```sh
curl --fail --cacert .pilot/ca.pem --cert .pilot/admin.pem --key .pilot/admin.key \
  -H 'Content-Type: application/json' \
  -d '{"asset":"USDT","amount":"1000","external_reference":"pilot-funding-001"}' \
  "https://localhost:9443/v1/accounts/$ACCOUNT_ID/funding"
curl --fail --cacert .pilot/ca.pem --cert .pilot/admin.pem --key .pilot/admin.key \
  "https://localhost:9443/v1/accounts/$ACCOUNT_ID/balances"
```

Retry an uncertain funding request with the same external reference and payload.
Using a new reference records another accounting input. Withdrawals use the same
privilege and precision checks through `/withdrawals`; neither operation transfers
assets outside the ledger. Grant funding separately from trading in a shared pilot.

Readiness requires the PostgreSQL lease, valid engine identity and capabilities,
completed recovery, no reconciliation latch, no maintenance barrier, settlement
catch-up and sufficient journal disk reserve. A missing database pauses admission;
a lost gateway lease requires a restart. Existing reservations remain locked.

Prometheus runs on `http://localhost:9091`, Alertmanager on `http://localhost:9093`.
The provided receiver logs test deliveries inside the Compose network. Send a test:

```sh
curl --fail -H 'Content-Type: application/json' \
  -d '[{"labels":{"alertname":"PilotDeliveryTest","severity":"warning"},"annotations":{"summary":"Local delivery rehearsal"}}]' \
  http://localhost:9093/api/v2/alerts
docker compose --env-file .pilot/compose.env -f ops/financial/compose.yml logs alert-receiver
```

Replace the local receiver in a deployment-specific Alertmanager configuration to
route production notifications. Do not commit recipient credentials. Rules cover
admission, unavailable metrics, reconciliation, settlement backlog, stuck intentions
and journal disk reserve. Metrics include reserved amounts by currency, worker
errors, journal bytes and free space. Monitor PostgreSQL volume space separately on
its host. There is no automatic failover or online journal compaction.

## Maintenance, backup and restore

1. Stop client submission. POST `{"enabled":true}` to `/v1/admin/maintenance`
   using the administrator certificate. Gateway admission stops first; reserved
   intentions drain, then the matcher persists a barrier that also freezes GTD.
2. GET `/v1/admin/checkpoint`. It requires matching engine/ledger watermarks,
   empty outbox, no pending intentions and a successful accounting audit. A timeout
   or a failed checkpoint leaves admission paused; investigate rather than releasing
   funds or deleting requests.
3. Run the backup utility. It checks the checkpoint before and after copying:

   ```sh
   python3 scripts/financial-backup.py --config-dir .pilot --output /secure/pilot-backup
   ```

   The archive contains a PostgreSQL custom dump, complete engine journal,
   maintenance snapshot, configuration and private identities. Store it with access
   restricted to its operator. The manifest records checksums and the agreed
   watermark. Admission remains paused after backup.
4. For shutdown, stop the gateway before the matcher. Compose uses SIGINT and a
   45-second grace period. An emergency SIGKILL requires journal replay on restart.
5. Restore only into a **new empty database** and a **new directory**. Set
   `ME_RESTORE_DATABASE_URL` through a private environment, then run:

   ```sh
   python3 scripts/financial-restore.py --archive /secure/pilot-backup --output /secure/restored-pilot
   ```

   The utility verifies checksums and refuses an existing destination or nonempty
   database. Update the restored Compose paths and database connection to the new
   resources; do not reuse the old volume. Start matcher, then gateway. Confirm the
   checkpoint matches the backup manifest and balances. POST `/v1/admin/reconcile`
   if required, then explicitly resume with `{"enabled":false}`.
6. Upgrade only while paused, after a tested backup. Re-run instrument/identity,
   accounting, cursor and compatibility checks before resuming. Financial schema
   version mismatches and migration checksum changes stop startup. There is no
   automatic downgrade path; test a new deployment before cutover.

Never restore an old backup over newer confirmed executions. Preserve both sources
and reconcile their complete durable decision histories before planning recovery.
A stop caused by corruption, unknown orders or source rollback requires operator
investigation; `GetOpenOrders` and a cleared timeout are insufficient evidence.
Financial snapshots retain ownership, reservations, fee versions, price protection,
order versions and book state. They are verified against full journal replay; this
RC implementation does not truncate the journal or accelerate replay from snapshots.


Fees may be changed only while the ledger is in maintenance, using a new schedule
UUID in the gateway configuration. Existing orders retain their original schedule.
A schedule UUID can never be reused with different rates.

Expiry processing uses durable batches of at most 128 heap entries per instrument.
Before a new command matches, all due expiries are committed in bounded decisions.
This prevents a simultaneous GTD sweep across many instruments from exceeding a
journal record. A restart between expiry batches continues from the recorded cursor.

The backup utility verifies the snapshot's internal checksum and compares the complete engine snapshot with ledger orders:
account ownership, order/fee/reservation versions, hidden remaining quantities and
exact reserve requirements. This check supplements the balanced-postings audit.
A discrepancy stops backup and leaves admission paused.

Additional rehearsals, with outputs outside the checkout:

```sh
LEDGER_TEST_DATABASE_URL=postgresql://... python3 scripts/financial-matrix.py --output /tmp/pilot-matrix
LEDGER_TEST_DATABASE_URL=postgresql://... python3 scripts/financial-faults.py --output /tmp/pilot-faults
LEDGER_TEST_DATABASE_URL=postgresql://... python3 scripts/financial-disk-smoke.py --output /tmp/pilot-disk
python3 scripts/financial-compose-smoke.py --output /tmp/pilot-operations
```

The disk rehearsal fills an isolated 8 MiB tmpfs volume, leaving the host filesystem
untouched. It needs Docker and the locally built `matching-engine:financial-pilot`
image. The Compose rehearsal uses local port 9443 and monitoring ports 9091/9093;
stop another local pilot before running it. Its database volumes and private files
are retained after shutdown for inspection.
