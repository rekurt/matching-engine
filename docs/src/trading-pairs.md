# Trading pairs

Each `[[symbols]]` entry registers one symbol. IDs must be unique. Prices and quantities on the wire always use scale `10^8`; `price_scale` and `qty_scale` do not change the wire scale.

```toml
[[symbols]]
id = 0
base = "BTC"
quote = "USDT"
price_scale = 2
qty_scale = 8
min_qty = "0.00001"
max_qty = "10000.0"
qty_step = "0.00001"
price_step = "0.01"
invariant_policy = "halt_symbol"

[symbols.risk]
max_order_qty = "10000.0"
max_order_notional = "100000000.0"
price_band_bps = 1000
reference_price = "50000.0"
max_orders_per_second = 1000
max_open_orders = 100000
trading_enabled = true
self_trade_prevention = false
```

Limits and steps are decimal strings, parsed without binary floating-point rounding. Quantity must be positive, within min/max bounds and a multiple of `qty_step`. Limit prices must be positive and a multiple of `price_step`.

For example, `50000.50` is raw price `5000050000000` and `0.5` is raw quantity `50000000`, regardless of symbol display scales. Use checked integer/decimal conversion in clients.

Adding or changing symbols, risk settings or shard count in an existing durable deployment can make recovery incompatible. Validate against copied storage and follow the [upgrade procedure](deployment.md).
