# Introduction

matching-engine processes spot orders with price-time priority. The Rust workspace separates core types, matching, shard orchestration, persistence and network delivery. TCP and Unix domain socket clients use binary protocol v2.

Supported orders include limit, market, stop-limit, stop-market, trailing-stop and iceberg. Time-in-force choices are GTC, IOC, FOK and GTD. Legacy sessions own their orders and support configurable per-symbol risk limits. Financial mode uses funded accounts, mandatory account self-trade prevention and an authenticated gateway.

Start with the [quick start](quickstart.md). Integrators should read the [protocol](protocol.md) and [recovery contract](deployment.md) before implementing reconnects.

The [financial pilot](financial-pilot.md) adds PostgreSQL balances, reservations, quote fees and settlement through an internal mTLS API. It requires a new ledger and an empty financial book. Manual funding records accounting entries without transferring real assets. The legacy session API remains available separately.

There is no replication, consensus or automatic failover. Journals and durable request history grow with activity. Network latency depends on storage sync, TLS, workload and hardware; benchmark the complete path before setting an SLA.
