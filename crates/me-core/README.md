# me-core

Примитивы и типы данных для движка сопоставления ордеров. Крейт не содержит бизнес-логики — только типобезопасные структуры, которые используют все остальные крейты.

## Возможности

| Тип | Описание |
|-----|----------|
| `FixedPrice` | Цена с фиксированной точкой, i64, 8 знаков после запятой |
| `FixedQty` | Количество с фиксированной точкой, u64, 8 знаков после запятой |
| `Order` | Структура ордера (88 байт, `#[repr(C)]`) |
| `OrderType` | Limit / Market / StopLimit / StopMarket |
| `TimeInForce` | GTC / IOC / FOK |
| `Side` | Buy / Sell |
| `SymbolId` | Идентификатор торговой пары (u16) |
| `SessionId` | 128-бит идентификатор сессии (`[u8; 16]`) |
| `MatchEvent` | Outbound события: Trade, OrderAccepted, OrderFilled и др. |
| `EventBuffer` | Переиспользуемый буфер событий (ёмкость по умолч. 256) |
| `OrderFlags` | POST_ONLY (0x01), REDUCE_ONLY (0x02), MAKER_OR_CANCEL (0x04) |
| `InvariantPolicy` | Strict / HaltSymbol / Continue — реакция на нарушения инвариантов |
| `RejectReason` | 13 причин отклонения (NoLiquidity..InternalError) |

## Пример создания ордера

```rust
use me_core::order::{Order, TimeInForce};
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};

// Limit buy 1.5 BTC @ 65000.00 USDT
let order = Order::new_limit(
    42,                            // order_id: u64
    SymbolId(0),                   // symbol (0 = BTC/USDT)
    Side::Buy,
    TimeInForce::GTC,
    FixedPrice::from_f64(65000.0), // price (stored as 6_500_000_000_000_000 i64)
    FixedQty::from_f64(1.5),       // qty  (stored as 150_000_000 u64)
    timestamp_ns,
);

// Работа с raw значениями (для сериализации)
assert_eq!(order.price.raw(), 6_500_000_000_000_000);
assert_eq!(order.qty.raw(), 150_000_000);

// Stop-limit: продать 0.5 BTC если цена упадёт до 60000, по 59500
let stop = Order::new_stop_limit(
    43, SymbolId(0), Side::Sell, TimeInForce::GTC,
    FixedPrice::from_f64(59500.0), // limit price
    FixedPrice::from_f64(60000.0), // stop trigger price
    FixedQty::from_f64(0.5),
    timestamp_ns,
);
```

## Масштаб (SCALE = 10^8)

Все цены и количества хранятся как целые числа, умноженные на `100_000_000`:

```
1.5 BTC  → FixedQty(150_000_000)
65000.00 → FixedPrice(6_500_000_000_000_000)
```

`FixedPrice` — `i64` (знаковое): допускает отрицательные значения для spread-арифметики.
`FixedQty` — `u64` (беззнаковое): количество всегда >= 0, вычитание использует saturating_sub.
