# Deployment and recovery

This guide describes single-node deployment, storage compatibility and client recovery. Engine benchmarks exclude durable network service costs.

## Build and exposure

Use Rust 1.96 or newer and `cargo build --release --locked -p me-server --features tls`. Run with `--config config.toml`. Configuration rejects unknown fields and invalid combinations. TCP and HTTP default to loopback. Non-loopback plaintext TCP requires the explicit `server.allow_plaintext_remote = true` setting; prefer TLS:

```toml
[server]
tcp_addr = "0.0.0.0:9090"
metrics_addr = "127.0.0.1:8080"
shards = 4

[server.tls]
cert_path = "/etc/me/server.pem"
key_path = "/etc/me/server.key"
client_ca_path = "/etc/me/client-ca.pem"

[server.admin]
api_key = "replace-with-a-long-random-secret"

[wal]
enabled = true
mode = "pre"
path = "/data/wal"
snapshot_interval = 100000
```

Add the required `[[symbols]]` definitions from the root configuration. Setting `client_ca_path` requires authenticated client certificates. TLS configuration on a binary without the TLS feature, incomplete certificate settings, and unreadable/invalid credentials prevent startup before trading listeners are exposed. A session ID is a bearer credential; TLS does not replace account authorization at the gateway. Protect HTTP metrics and admin access separately: this listener does not implement TLS. An empty admin key disables mutations; configured keys must contain at least 16 printable non-space ASCII characters.

The Dockerfile builds TLS support, runs as UID 10001, and checks `/ready` on port 8080. Mount a persistent writable `/data` volume and a read-only configuration/certificate directory. Its default loopback TCP binding requires an explicit deployment configuration for remote clients. With its ENTRYPOINT, pass `--config /etc/me/config.toml --metrics-addr 127.0.0.1:8080` as container arguments, without another `me-server` executable. Restrict port publishing to the intended network. The mounted data directory must be writable by UID 10001.

## Durability and recovery

Keep WAL enabled for durable operation. Both `pre` and `post` modes synchronously persist mutation decisions and their outcomes before replying. Replay uses the stored decision, original ownership, processing time, expiration metadata, and expected events; it does not turn rejected or unauthorized commands into privileged successful commands.

Engine journals and `sessions.wal` form one recovery unit. Session admission, request fingerprints, batch intents, replies, and acknowledgments are journaled. Never delete or replace only one of these files. Missing session history for an existing engine history prevents startup. Journal files containing bearer credentials are restricted to owner read/write permissions. A writer lock prevents two engines from recovering or mutating the same journal concurrently.

Snapshots are checkpoints. The complete append-only decision history is retained: snapshot creation does not truncate the journal. A corrupt snapshot can be bypassed only when complete validated history can reconstruct state. Detected engine journal corruption, incompatible configuration, or replay divergence prevents startup. The session journal recovers an incomplete trailing frame at the last complete record. CRC and contiguous sequence validation do not prove that entire trailing records were never deleted; consistent whole-directory backups and storage integrity remain required. Symbol/risk configuration and shard topology are recorded; incompatible changes prevent recovery. Pin `shards` instead of relying on machine-dependent automatic CPU counts.

**Upgrade boundary:** old command-only/compacted journals are not silently migrated into the decision format. Do not replace the binary over an existing legacy data directory and assume compatibility. Stop intake, reconcile outstanding orders and executions with the upstream ledger, preserve a complete backup, and use an explicitly validated migration or a reconciled empty book. There is no automated legacy migration or online resharding tool. The atomic snapshot response adds a session-journal record variant: after the new binary writes one, an older binary cannot read that journal. Rollback must use a compatible reader or a reconciled backup; never restore an old backup over acknowledged new trades.

For backups, stop intake and shut down cleanly, then copy the whole data directory with its permissions and deployment configuration. Test restoration in an isolated environment before relying on a backup. A live copy of individual files is not a consistent backup.

## Client recovery contract

Protocol v2 uses a 22-byte header and client request IDs. Persist the session ID and last applied sequence from the replay stream on the client. Reconnect by resuming that session; concurrent use of the same session is refused. Retrying an identical durable mutation returns its original outcome; reusing a retained request ID with a different payload is rejected. Read-only query responses have a bounded cache and can be regenerated after eviction.

The replay stream includes both order/query responses and positive-sequence control frames, as described in the [protocol](protocol.md). Replayed frames retain their original sequence; never apply them twice. Sequence zero is out-of-band and is not acknowledged. SessionResumed precedes replay with sequence zero. HandshakeAck, Pong, ReplayGap from ResumeSession and some Error frames in the active-session response path receive positive server sequences and must be processed before advancing the cursor. ReplayGap emitted during pending delivery instead uses sequence zero and is not journaled. Passive fills, trades, and stop events use `request_id = 0` and are delivered to their owners, including when the owner reconnects later. Preserve those events while assembling a snapshot and commit its cursor only after the complete response validates.

On ReplayGap, request GetOpenOrders. Complete snapshots are delivered in bounded write chunks even when larger than the replay ring. A snapshot is limited to 8 MiB of encoded frames, enough for the 100,000 admitted orders of one session; oversized snapshots are refused before journal mutation. A complete snapshot is persisted as one session-journal record before delivery; interruption during its append does not recover a prefix as a completed snapshot. The per-session query-response cache has a 16 MiB byte budget, with query responses and their admission metadata evicted together. This does not evict durable mutation decisions. An open-order snapshot restores current order state, **not missing historical settlement events**. Reconcile those against an authoritative execution record before resuming financial processing. The example clients demonstrate submission; they are not complete account/settlement recovery services. The Go SDK does not yet expose a high-level ResumeSession/receive-cursor/original-request-ID recovery API; implement the recovery protocol at the gateway before relying on reconnect for financial processing.

Batches retain individual durable item identities and recover partially completed execution without repeating completed items. They are not an atomic transaction across symbols. Per-symbol execution and delivery are serialized; there is no global order across independent symbols.

## Capacity and risk policy

Request history and the append-only journals currently grow with activity. Replay buffers are bounded, but total durable request history is not a constant-memory store. Monitor process memory, disk space, write latency, and restart duration; provision capacity and schedule reconciled maintenance. Online history compaction/retention is not implemented. Benchmark the durable path on the intended filesystem and workload before setting an SLA; synchronous persistence changes the cost relative to an in-memory benchmark.

Session admission is bounded: at most 10,000 sessions; each session has 100,000 place/amend units and 16 MiB of admitted mutation payload, with a separate 100,000-unit cancellation reserve. Batches count individual items. Invalid/unauthorized cancellation attempts do not consume that reserve. Existing request retries remain usable. Unused idle sessions can expire; sessions with durable activity are retained to preserve ownership and idempotency. Plan session lifecycle and capacity at the gateway; expiration is not a way to discard active trading history.

Risk checks cover amendments and activated stops as well as placements. A market order requiring a reference price fails closed when none is available; seed `symbols.risk.reference_price` explicitly or establish a traded reference. Hard notional checks use a conservative execution-price bound from the book and total quantity, including hidden quantity. They can reject an IOC that would only partly fill. With a hard notional cap, amendments after any fill may only decrease or preserve both price and quantity; increases return AmendNotSupported because historical filled quote notional is not an account ledger. Zero rate limit disables that limit. Position limits without an account ledger are unsupported and rejected rather than advertised as enforced.

## Matching work limits

A matching command has a shared budget of 4,096 fills, including triggered orders and iceberg replenishment. An order whose predicted matching work exceeds the remaining budget is rejected before that matching pass mutates the book; FOK does not partially execute. An iceberg may contain at most 4,096 display slices. Each symbol may have at most 4,096 live pending stops and 4,096 live GTD orders. These limits apply alongside any stricter configured risk limits and are checked during snapshot restoration. Retired/cancelled orders release their capacity.

The preflight is conservative and can reject work that a less conservative simulation might admit. Existing snapshots above these limits are rejected. Journals created with different matching semantics, including the former delayed iceberg refill behavior, may fail event-equivalence replay; validate upgrade and reconciliation against a copied data directory before deployment.

Matching corrections also preserve FIFO after snapshot restoration, remove the full visible remainder when an amendment cancels a filled-down order, and consume each trade price only once when advancing trailing stops. Journals containing decisions produced by the former behavior can fail equivalence replay. Validate the new binary against a copy of existing storage before upgrading; do not discard or rewrite acknowledged executions to bypass a mismatch.

These bounds prevent a tiny display quantity or a large stop/expiry wave from expanding one command into an oversized durable decision. They are service capacity limits, not financial account limits. Gateway admission and capacity planning must account for them.

## Health and shutdown

`/health` and `/ready` return 503 while draining, before configured trading listeners bind, or when session storage/engine recovery/workers fail. An intentionally halted symbol remains operationally healthy; `/admin/status` reports its actual halted state. Halt/resume state is durable. SIGINT/SIGTERM closes intake and drains connections within the configured timeout; HTTP shutdown is bounded as well. A failed durable write stops further safe processing rather than acknowledging an unpersisted result.

The default log filter is `info`, including audit events and storage/network warnings. `RUST_LOG` overrides it; excluding the `audit` target deliberately suppresses audit telemetry. Asynchronous audit telemetry can drop records under pressure; monitor `me_audit_dropped_total` and `me_audit_disconnected_total`. It is not the authoritative durable execution journal. The binary audit representation uses explicit serialization and does not expose struct padding.

This repository supplies a single-node matching service. It does not implement account balances, reserved funds, settlement, replication, consensus, automatic failover, or disaster-recovery orchestration. Those must be provided and tested by the surrounding trading platform. Passing local tests is not evidence of those external guarantees.

## Verification

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build -p me-server --features tls --locked
ME_TEST_TLS=1 python3 scripts/runtime_smoke.py
python3 -m unittest discover -s examples/python
cargo test --manifest-path examples/rust/Cargo.toml --locked
(cd clients/go/meclient && go test -race ./...)
(cd examples/typescript && npm ci --ignore-scripts && npm test)
cargo audit
```

Runtime tests start actual processes, exercise TCP and mTLS, kill/restart the server, resume sessions, verify ownership/cancellation and idempotent replies, and check readiness/startup failures. Additional Rust integration tests cover UDS, replay, journal corruption, risk boundaries, and partial-batch recovery.

## Monitoring example

Set `GRAFANA_ADMIN_PASSWORD` to a unique secret, then run `docker compose -f monitoring/docker-compose.yml up -d`. Grafana is published only on `127.0.0.1:3000`; Prometheus is on `127.0.0.1:19090`, leaving port 9090 for trading. No default Grafana password is supplied.

The sample scrape target is `host.docker.internal:9091`. It must reach the engine HTTP listener from the container network: loopback binding on a Linux host is not reachable through the bridge gateway. Bind HTTP to a dedicated private interface reachable only by your monitoring network, or use an authenticated private proxy and update the target. Do not expose the unauthenticated metrics endpoint publicly. Validate `up{job="matching-engine"} == 1` after deployment. These rules require an Alertmanager deployment for notifications; the example does not configure one.
