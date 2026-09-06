# me-risk — Pre-Trade Risk Management

Pre-trade risk checks performed before orders reach the matching engine. All checks are O(1).

| Feature | Details |
|---------|---------|
| Rate limiting | Token-bucket per symbol (`max_orders_per_second`) |
| Fat finger protection | `max_order_qty` — maximum single order quantity |
| Notional limit | `max_order_notional` — maximum order value in quote currency |
| Position limit | Unsupported without an account ledger; nonzero `max_position` is rejected |
| Price band | `price_band_bps` — max % deviation from reference price (basis points) |
| Open orders cap | `max_open_orders` — maximum open orders per symbol |
| Circuit breaker | `trading_enabled` per symbol — admin-controlled halt |

## Quick example

```rust
use me_risk::engine::RiskEngine;
use me_risk::config::RiskConfig;
use me_core::types::{FixedQty, SymbolId};

let mut risk = RiskEngine::new();

let config = RiskConfig {
    max_orders_per_second: 10,
    max_order_qty: FixedQty::from_raw(1_000_000_000), // 10 BTC
    price_band_bps: 500, // ±5%
    ..RiskConfig::default_for(SymbolId(1))
};
risk.add_symbol(config);

// Before forwarding to matching engine:
match risk.check_order(&order) {
    Ok(()) => { /* forward to engine */ }
    Err(reason) => { /* reject with reason */ }
}
```

## Risk parameters

```toml
[[symbols]]
id = 1
base = "BTC"
quote = "USDT"
price_scale = 2
qty_scale = 8

[symbols.risk]
max_orders_per_second = 100
max_order_qty = "10.0"          # 10 BTC
max_order_notional = "1000000"  # $1M
price_band_bps = 1000           # ±10% from reference price
max_open_orders = 10000
```

## Rate limiter internals

Token-bucket algorithm: starts full at capacity, refills continuously at `max_orders_per_second` tokens/sec.
`try_acquire()` returns `false` (rate limit exceeded) when the bucket is empty.
