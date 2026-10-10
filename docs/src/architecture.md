# Architecture

The service separates symbol matching from network I/O. Each symbol maps to a shard worker; that worker serializes commands, risk decisions and book mutations. Independent symbols have no global execution order.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `me-core` | Fixed-point types, orders, events, symbols and session IDs |
| `me-book` | Price levels, order pool, stop book and price-time matching |
| `me-engine` | Per-symbol engine, routing, shard workers, risk/durability orchestration |
| `me-persist` | Journal encoding, CRC checks and atomic snapshots |
| `me-io` | Protocol codec, TCP/UDS, session storage, replay and delivery |
| `me-server` | Configuration, startup/recovery, TLS, HTTP and shutdown |
| `me-risk` | Per-symbol admission and execution risk policy |
| `me-metrics` | Prometheus instrumentation |
| `me-audit` | Structured audit telemetry |
| `me-clock` | System, coarse and CPU-counter clocks |
| `me-bench` | Criterion benchmark suites |
| `me-datagen` | Seeded order-flow generator |

## Matching

The order book uses sorted price levels, FIFO order links and indexed order storage. Crossing orders take the best opposing prices first. Cancels use the order index; price/quantity amendments can alter time priority according to the matching rules. Stops activate from trade prices, GTD orders expire from recorded processing time, and iceberg replenishment participates in the same matching work budget.

Prices are signed 64-bit fixed-point values; quantities are unsigned 64-bit fixed-point values, both with scale `10^8`. The binary wire layout is explicitly encoded and is independent of Rust struct layout.

Risk policy runs before admission and also covers amendments and triggered stops. Ownership is based on the client session. Internal invariant violations use the configured policy, defaulting to halting the affected symbol.

## Request path

1. Decode a frame and validate protocol/connection state.
2. Check session/request identity and admission capacity.
3. Route the command to its symbol worker, enforcing session ownership and risk.
4. Persist the decision/outcome when durability is enabled.
5. Persist replies and passive owner events in session history before delivery.

Network tasks use blocking-task boundaries for synchronous shard operations. TCP and UDS share codec and session semantics; TLS wraps TCP transport.

## Recovery and observability

Engine journals and the session journal are a single recovery unit. Snapshots are checkpoints; retained decisions reconstruct subsequent state and verify expected events. Startup fails on incompatible configuration, detected corruption or replay divergence.

HTTP readiness reflects listener, session-storage and engine health. Intentional symbol halts are reported by status without making the service unhealthy. Audit telemetry may drop records under pressure and is not the authoritative execution journal. See [deployment](deployment.md) for the operational contract and [HTTP API](api.md) for endpoints.
