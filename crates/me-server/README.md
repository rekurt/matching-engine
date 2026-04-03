# me-server — TCP Matching Engine Server

Binary that wires together all crates: loads config, starts `ShardedRouterV2`, launches TCP/UDS server, and exposes Prometheus metrics + admin HTTP API.

| Feature | Details |
|---------|---------|
| Transport | TCP (binary framed protocol) + Unix Domain Socket (optional) |
| Config | TOML file via `--config`; built-in defaults for BTC/USDT, ETH/USDT, SOL/USDT |
| Metrics | Prometheus endpoint at `metrics_addr` (default `:9000`) |
| Admin API | HTTP REST with optional Bearer token auth |
| Sharding | `shards = 0` = auto (one per CPU core) |
| Graceful shutdown | SIGTERM/Ctrl-C drains in-flight commands within `drain_timeout_secs` |

## Running

```bash
# Built-in defaults (BTC/USDT, ETH/USDT, SOL/USDT on 0.0.0.0:9001)
cargo run --release -p me-server

# Custom config
cargo run --release -p me-server -- --config config.toml

# Override address on the fly
cargo run --release -p me-server -- --tcp-addr 127.0.0.1:9001 --shards 8
```

## Minimal config.toml

```toml
[server]
tcp_addr     = "0.0.0.0:9001"
metrics_addr = "0.0.0.0:9000"
shards       = 0                # 0 = auto-detect CPU cores

[server.admin]
api_key = "secret"              # leave empty to disable auth

[[symbols]]
id          = 1
base        = "BTC"
quote       = "USDT"
price_scale = 2   # prices in cents: $50000.00 → 5000000
qty_scale   = 8   # quantities: 1 BTC → 100000000

[[symbols]]
id          = 2
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
GET  /health           — liveness probe
GET  /metrics          — Prometheus text format
POST /admin/halt       — halt trading for a symbol (requires Bearer token)
POST /admin/resume     — resume trading for a symbol
```

## Ports

- `9001` — matching engine TCP (binary protocol, see me-io README)
- `9000` — HTTP metrics + admin
