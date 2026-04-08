# me-server — TCP Matching Engine Server

Binary that wires together all crates: loads config, starts `ShardedRouterV2`, launches TCP/UDS server, and exposes Prometheus metrics + admin HTTP API.

| Feature | Details |
|---------|---------|
| Transport | TCP (binary framed protocol v2) + Unix Domain Socket (optional) |
| Config | TOML file via `--config`; built-in defaults for BTC/USDT, ETH/USDT, SOL/USDT |
| Metrics | Prometheus endpoint at `metrics_addr` (default `:9091`) |
| Admin API | HTTP REST with optional `X-Admin-Key` auth |
| Sharding | `shards = 0` = auto (one per CPU core) |
| TLS | Optional via `tls` feature (rustls, mTLS support) |
| Graceful shutdown | SIGTERM/Ctrl-C drains in-flight commands within `drain_timeout_secs` |

## Running

```bash
# Built-in defaults (BTC/USDT, ETH/USDT, SOL/USDT on 0.0.0.0:9090)
cargo run --release -p me-server

# Custom config
cargo run --release -p me-server -- --config config.toml

# Override address on the fly
cargo run --release -p me-server -- --tcp-addr 127.0.0.1:9090 --shards 8
```

## Minimal config.toml

```toml
[server]
tcp_addr     = "0.0.0.0:9090"
metrics_addr = "0.0.0.0:9091"
shards       = 0                # 0 = auto-detect CPU cores

[server.admin]
api_key = "secret"              # leave empty to disable auth

[[symbols]]
id          = 0
base        = "BTC"
quote       = "USDT"
price_scale = 2   # prices in cents: $50000.00 -> 5000000
qty_scale   = 8   # quantities: 1 BTC -> 100000000

[[symbols]]
id          = 1
base        = "ETH"
quote       = "USDT"
price_scale = 2
qty_scale   = 8
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `me_server=info` | Log level filter |

## Admin API endpoints

```
GET  /health              — liveness probe
GET  /metrics             — Prometheus text format
GET  /admin/status        — server status (requires X-Admin-Key if configured)
POST /admin/halt/{id}     — halt trading for symbol {id}
POST /admin/resume/{id}   — resume trading for symbol {id}
```

## Ports

- `9090` — matching engine TCP (binary protocol v2, see me-io README)
- `9091` — HTTP metrics + admin
