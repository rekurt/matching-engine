# matching-engine

[![CI](https://github.com/rekurt/matching-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/rekurt/matching-engine/actions/workflows/ci.yml)
[![Site](https://github.com/rekurt/matching-engine/actions/workflows/docs.yml/badge.svg)](https://rekurt.github.io/matching-engine/)

A Rust service for matching spot orders with price-time priority. Each symbol is processed by one shard worker; clients connect through a binary protocol over TCP or Unix domain sockets.

## Features

- Limit, market, stop-limit, stop-market, trailing-stop and iceberg orders; GTC, IOC, FOK and GTD.
- Fixed-point prices and quantities with eight decimal places.
- Per-symbol risk checks, order ownership, maker-only flags and optional self-trade prevention.
- Durable decisions, snapshots, session replay and idempotent mutation retries when WAL is enabled.
- TLS/mTLS, Prometheus metrics, health checks and authenticated trading halt/resume.
- Go client, a separate recovering Go client with durable checkpoints, and four language examples.
- Opt-in financial pilot: PostgreSQL double-entry ledger, funded reservations, quote fees, an internal mTLS gateway and durable settlement.

## Build and run

Requires Rust 1.96 or newer. From the repository root:

```sh
cargo build --release --locked -p me-server --features tls
./target/release/me-server --config config.toml
```

The sample configuration listens on `127.0.0.1:9090` for trading and `127.0.0.1:9091` for HTTP, and stores journals under `./data/wal`.

In another terminal:

```sh
curl --fail http://127.0.0.1:9091/ready
cd examples/go
go run . 127.0.0.1:9090
```

Use a fresh data directory for the example: order IDs and mutation request IDs are retained for recovery. For a remote deployment, configure TLS, a fixed shard count and persistent storage as described in the [deployment guide](docs/src/deployment.md).

## Service boundaries

This is a single-node service. The optional [financial pilot](docs/src/financial-pilot.md) adds a PostgreSQL ledger and one internal gateway; it starts with a new ledger and empty book and requires protected market prices. Funding records are privileged accounting entries, with no bank or blockchain transfer. Legacy session IDs remain bearer credentials rather than account identities. Replication, automatic failover, public account APIs, online compaction and legacy journal migration are outside this release. Journals and retained request history grow with activity.

No latency SLA is implied. Measure the complete durable path on your own storage and workload; engine microbenchmarks exclude network and synchronous journal costs.

## Documentation

[Project website](https://rekurt.github.io/matching-engine/) · [All projects by rekurt](https://rekurt.github.io/projects/)

The [online handbook](https://rekurt.github.io/matching-engine/docs/) is built from [`docs/src`](docs/src/SUMMARY.md):

- [Quick start](docs/src/quickstart.md) and [configuration](docs/src/configuration.md)
- [Wire protocol v2](docs/src/protocol.md) and [client integration](docs/src/integration.md)
- [Financial pilot and operations](docs/src/financial-pilot.md)
- [Financial pilot validation](docs/src/financial-validation.md): workload, recovery checks and measured results.
- [Architecture](docs/src/architecture.md), [HTTP API](docs/src/api.md) and [deployment/recovery](docs/src/deployment.md)
- [Benchmarks](docs/src/benchmarks.md), [contributing](CONTRIBUTING.md) and [changelog](CHANGELOG.md)

Build the handbook with `mdbook build`; generate Rust API documentation with `cargo doc --workspace --no-deps --all-features --locked`.

## License

[MIT](LICENSE), copyright nikita42 (rekurt).
