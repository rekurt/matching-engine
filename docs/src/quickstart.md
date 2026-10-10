# Quick start

Install Rust 1.96 or newer. Clone the repository and build the TLS-capable server:

```sh
git clone https://github.com/rekurt/matching-engine.git
cd matching-engine
cargo build --release --locked -p me-server --features tls
./target/release/me-server --config config.toml
```

The sample config defines BTC/USDT, ETH/USDT and SOL/USDT. Trading listens on `127.0.0.1:9090`, HTTP on `127.0.0.1:9091`, and the WAL directory is `./data/wal`. TLS is available in the binary but is enabled only when certificate paths are configured.

In a second terminal, check readiness and run the Go example:

```sh
curl --fail http://127.0.0.1:9091/ready
cd examples/go
go run . 127.0.0.1:9090
```

The example places crossing limit orders, reads their execution events and cancels a resting order. Run it against a fresh test data directory: previously used order IDs remain part of durable history.

Stop the server with SIGINT or SIGTERM. On restart, use the same symbol/risk configuration, shard topology and whole data directory. See [deployment](deployment.md) before changing recovery settings or exposing listeners remotely.

For a temporary in-memory experiment, set `[wal] enabled = false` in a separate config. That disables crash recovery and is not a durable deployment.
