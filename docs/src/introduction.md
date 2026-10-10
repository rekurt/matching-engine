# Introduction

matching-engine processes spot orders with price-time priority. The Rust workspace separates core types, matching, shard orchestration, persistence and network delivery. TCP and Unix domain socket clients use binary protocol v2.

Supported orders include limit, market, stop-limit, stop-market, trailing-stop and iceberg. Time-in-force choices are GTC, IOC, FOK and GTD. Risk policy is configured per symbol; session ownership scopes cancel, amend and status queries.

Start with the [quick start](quickstart.md). Integrators should read the [protocol](protocol.md) and [recovery contract](deployment.md) before implementing reconnects.

The service provides matching, not account balances or settlement. There is no replication, consensus or automatic failover. Journals and durable request history grow with activity. Network latency depends on storage sync, TLS, workload and hardware; benchmark the complete path before setting an SLA.
