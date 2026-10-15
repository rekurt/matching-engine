# Architecture

The service separates symbol matching from network I/O. In legacy mode each symbol maps to a shard worker; that worker serializes commands, risk decisions and book mutations. Independent legacy symbols have no global execution order. Financial mode uses one serial shard and a durable decision sequence for all configured symbols.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `me-core` | Fixed-point types, orders, events, symbols and session IDs |
| `me-book` | Price levels, order pool, stop book and price-time matching |
| `me-engine` | Per-symbol engine, routing, shard workers, risk/durability orchestration |
| `me-persist` | Journal encoding, CRC checks and atomic snapshots |
| `me-io` | Protocol codec, TCP/UDS, session storage, replay and delivery |
| `me-server` | Configuration, startup/recovery, TLS, HTTP and shutdown |
| `me-financial` | Shared funded-order contracts, decision identities and mTLS transport |
| `me-ledger` | Exact PostgreSQL accounting, reservations, outbox and settlement |
| `me-gateway` | Internal mTLS HTTP API, account permissions and financial recovery |
| `me-risk` | Per-symbol admission and execution risk policy |
| `me-metrics` | Prometheus instrumentation |
| `me-audit` | Structured audit telemetry |
| `me-clock` | System, coarse and CPU-counter clocks |
| `me-bench` | Criterion benchmark suites |
| `me-datagen` | Seeded order-flow generator |

## Matching

The order book uses sorted price levels, FIFO order links and indexed order storage. Crossing orders take the best opposing prices first. Cancels use the order index; price/quantity amendments can alter time priority according to the matching rules. Stops activate from trade prices, GTD orders expire from recorded processing time, and iceberg replenishment participates in the same matching work budget.

Prices are signed 64-bit fixed-point values; quantities are unsigned 64-bit fixed-point values, both with scale `10^8`. The binary wire layout is explicitly encoded and is independent of Rust struct layout.

Legacy risk policy runs before admission and also covers amendments and triggered stops. Legacy ownership is based on the client session; its server configuration defaults invariant violations to halting the affected symbol.

Financial orders retain account, reservation, fee and order-version bindings; self-trade prevention uses the account identity. Funded admission and instrument constraints apply, but financial mode does not expose configurable `me-risk` limits. Its JSON symbol configuration selects the invariant policy, defaulting to `strict` (panic); the generated pilot explicitly uses `strict`.

## Legacy request path

1. Decode a frame and validate protocol/connection state.
2. Check session/request identity and admission capacity.
3. Route the command to its symbol worker, enforcing session ownership and risk.
4. Persist the decision/outcome when durability is enabled.
5. Persist replies and passive owner events in session history before delivery.

Network tasks use blocking-task boundaries for synchronous shard operations. TCP and UDS share codec and session semantics; TLS wraps TCP transport.

## Financial request path

1. Authenticate the service certificate and its account/operation permissions.
2. Commit the intent, full reservation and stable outbox command in PostgreSQL.
3. Send the original command bytes to the financial matcher over mTLS.
4. Commit a matching decision to the durable journal before confirming it.
5. Apply the whole decision, both counterparties, fees and settlement cursor in one PostgreSQL transaction.

A disconnect preserves the reservation. Outbox retries retain their original IDs and payloads. Duplicate decisions have no financial effect; an identity or lineage mismatch blocks admission until reconciliation. The matcher journal owns executions, while PostgreSQL owns financial accounting. See the [financial pilot](financial-pilot.md) for the API, limits and operational procedure.

## Recovery and observability

Legacy engine journals and the session journal are a single recovery unit. Snapshots are checkpoints; retained decisions reconstruct subsequent state and verify expected events. Startup fails on incompatible configuration, detected corruption or replay divergence. Financial recovery instead pairs the complete financial journal and configuration with a coordinated PostgreSQL backup; financial snapshots are verified against full replay.

Legacy HTTP readiness reflects listener, session-storage and engine health. Intentional symbol halts are reported by status without making the service unhealthy. Gateway readiness also checks database ownership, recovery, reconciliation, settlement lag and disk reserve. Audit telemetry may drop records under pressure and is not the authoritative execution journal. See [deployment](deployment.md) for the operational contract and [HTTP API](api.md) for legacy endpoints.
