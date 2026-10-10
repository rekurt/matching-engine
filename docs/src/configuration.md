# Configuration

Pass `--config config.toml`. Without it, the binary uses built-in defaults. CLI overrides are `--tcp-addr`, `--metrics-addr`, `--shards` and `--uds-path`. Unknown TOML fields and invalid combinations are rejected before trading starts.

## Server

| Field | Default | Meaning |
| --- | --- | --- |
| `server.tcp_addr` | `127.0.0.1:9090` | Trading listener |
| `server.metrics_addr` | `127.0.0.1:9091` | HTTP health, metrics and admin |
| `server.shards` | `0` | Auto CPU count; pin a count for recovery compatibility |
| `server.uds_path` | absent | Optional Unix domain socket |
| `server.allow_plaintext_remote` | `false` | Explicit opt-in for non-loopback plaintext TCP |
| `server.clock` | `coarse` | `system`, `coarse` or `rdtsc` |
| `server.pin_threads` | `false` | Linux CPU affinity |
| `server.shutdown.drain_timeout_secs` | `30` | Connection drain deadline |
| `server.heartbeat.interval_ms` | `5000` | Advertised heartbeat interval |
| `server.heartbeat.timeout_ms` | `15000` | Connection read timeout |
| `server.session.replay_buffer_size` | `1000` | Per-session replay frame capacity |
| `server.session.ttl_secs` | `60` | Idle lifetime for unused sessions |

Configure `server.tls.cert_path` and `key_path` together; `client_ca_path` enables required client certificates. TLS needs the `tls` build feature. The HTTP listener has no native TLS.

`server.admin.api_key` is empty by default, disabling halt/resume routes. Configured keys require at least 16 printable, non-space ASCII characters. Status and metrics remain public; protect the HTTP listener with a private network or proxy.

## Symbols and risk

Each `[[symbols]]` entry defines an ID, base/quote names, price/quantity scales and decimal-string limits. See [trading pairs](trading-pairs.md). The optional `[symbols.risk]` table enables these controls:

| Field | Meaning |
| --- | --- |
| `max_order_qty` | Maximum order quantity, including hidden quantity |
| `max_order_notional` | Conservative quote notional cap |
| `price_band_bps` | Deviation from reference price; zero disables |
| `reference_price` | Initial reference price as a decimal string |
| `max_orders_per_second` | Token-bucket limit; zero disables |
| `max_open_orders` | Resting-order limit; zero disables |
| `trading_enabled` | Admission switch |
| `self_trade_prevention` | Reject same-session self-matches |

Without a risk table, these checks do not run. Market orders subject to notional/reference checks fail closed if no reference price is available. Seed `reference_price` or establish a traded reference. Position limits are not supported without an external account ledger.

`invariant_policy` is `halt_symbol` by default. `strict` panics on internal invariant violations; `continue` logs and continues and should be reserved for controlled recovery.

## Storage

| Field | Default | Meaning |
| --- | --- | --- |
| `wal.enabled` | `true` | Enable durable recovery |
| `wal.mode` | `pre` | `pre` or `post`; both persist decisions/outcomes before reply |
| `wal.path` | `./data/wal` | Journal directory |
| `wal.sync_interval` | `100` | Legacy writer interval; the server syncs each decision explicitly, so this does not batch server replies |
| `wal.snapshot_dir` | derived | Defaults to `{wal.path}/snapshots` |
| `wal.snapshot_interval` | `100000` | Commands per shard checkpoint; zero disables |

Snapshot creation does not truncate decision history. Symbol/risk settings and shard topology are part of the recovery contract. See [deployment and recovery](deployment.md) for backups and upgrades.
