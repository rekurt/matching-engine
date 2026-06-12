use me_book::book::OrderBook;
use me_book::matching::{cancel_order, process_order};
use me_book::stop_book::StopBook;
use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::order::{Order, OrderType, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side, SymbolId};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn make_config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn new_book() -> OrderBook {
    OrderBook::new(make_config())
}

fn limit(id: OrderId, side: Side, price: f64, qty: f64, tif: TimeInForce) -> Order {
    Order::new_limit(
        id,
        SymbolId(0),
        side,
        tif,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    )
}

fn market(id: OrderId, side: Side, qty: f64) -> Order {
    Order::new_market(
        id,
        SymbolId(0),
        side,
        TimeInForce::IOC,
        FixedQty::from_f64(qty),
        id * 1000,
    )
}

fn stop_limit(id: OrderId, side: Side, stop: f64, lim: f64, qty: f64) -> Order {
    Order::new_stop_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice::from_f64(lim),
        FixedPrice::from_f64(stop),
        FixedQty::from_f64(qty),
        id * 1000,
    )
}

/// Count events of a specific variant.
fn count_trades(events: &[MatchEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, MatchEvent::Trade(_)))
        .count()
}

fn has_accepted(events: &[MatchEvent], id: OrderId) -> bool {
    events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { order_id, .. } if *order_id == id))
}

fn has_rejected(events: &[MatchEvent], id: OrderId, reason: RejectReason) -> bool {
    events.iter().any(|e| {
        matches!(e, MatchEvent::OrderRejected { order_id, reason: r, .. }
            if *order_id == id && *r == reason)
    })
}

fn has_filled(events: &[MatchEvent], id: OrderId) -> bool {
    events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderFilled { order_id, .. } if *order_id == id))
}

fn has_partially_filled(events: &[MatchEvent], id: OrderId) -> bool {
    events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderPartiallyFilled { order_id, .. } if *order_id == id))
}

fn has_cancelled(events: &[MatchEvent], id: OrderId) -> bool {
    events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderCancelled { order_id, .. } if *order_id == id))
}

// ─── Level total_qty accuracy (FOK precheck depends on it) ───────────────────

/// Regression: a fully-filled maker's quantity must leave `level.total_qty`
/// even when the level survives because another order remains. Otherwise the
/// cached aggregate overcounts and `available_*_qty` (used by the FOK precheck)
/// reports phantom liquidity.
#[test]
fn test_level_total_qty_accurate_after_full_fill() {
    let mut book = new_book();
    let mut events = EventBuffer::default();
    // Two resting bids at 100: A=2, B=3 → level total 5.
    process_order(
        &mut book,
        limit(1, Side::Buy, 100.0, 2.0, TimeInForce::GTC),
        &mut events,
    );
    process_order(
        &mut book,
        limit(2, Side::Buy, 100.0, 3.0, TimeInForce::GTC),
        &mut events,
    );
    assert_eq!(
        book.available_bid_qty(Some(FixedPrice::from_f64(100.0))),
        FixedQty::from_f64(5.0)
    );

    // A sell of 2 fully fills A (FIFO front); B (3) survives at the level.
    let mut e2 = EventBuffer::default();
    process_order(
        &mut book,
        limit(3, Side::Sell, 100.0, 2.0, TimeInForce::IOC),
        &mut e2,
    );

    assert_eq!(
        book.available_bid_qty(Some(FixedPrice::from_f64(100.0))),
        FixedQty::from_f64(3.0),
        "fully-filled maker quantity must leave level.total_qty"
    );
}

/// End-to-end: after a full fill leaves only 3 at the level, a FOK sell for 4
/// must be rejected (InsufficientLiquidity) — the precheck must see the true
/// residual, not the stale cached total — and must not partially fill.
#[test]
fn test_fok_rejected_against_true_residual_not_stale_total() {
    let mut book = new_book();
    let mut events = EventBuffer::default();
    process_order(
        &mut book,
        limit(1, Side::Buy, 100.0, 2.0, TimeInForce::GTC),
        &mut events,
    );
    process_order(
        &mut book,
        limit(2, Side::Buy, 100.0, 3.0, TimeInForce::GTC),
        &mut events,
    );
    let mut e2 = EventBuffer::default();
    process_order(
        &mut book,
        limit(3, Side::Sell, 100.0, 2.0, TimeInForce::IOC),
        &mut e2,
    );

    let mut e3 = EventBuffer::default();
    process_order(
        &mut book,
        limit(4, Side::Sell, 100.0, 4.0, TimeInForce::FOK),
        &mut e3,
    );
    assert!(
        has_rejected(e3.events(), 4, RejectReason::InsufficientLiquidity),
        "FOK for 4 with only 3 truly available must be rejected"
    );
    assert_eq!(count_trades(e3.events()), 0, "rejected FOK must not trade");
}

// ─── FOK: full rejection on insufficient liquidity ───────────────────────────

/// A FOK limit buy for more than the available ask quantity must be fully rejected
/// without generating any trades or consuming any resting liquidity.
#[test]
fn test_fok_limit_insufficient_liquidity_full_reject() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    // Resting ask: 1.0 at 100.0
    process_order(
        &mut book,
        limit(1, Side::Sell, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // FOK buy for 2.0 at 100.0 — only 1.0 available, must be fully rejected
    process_order(
        &mut book,
        limit(2, Side::Buy, 100.0, 2.0, TimeInForce::FOK),
        &mut events,
    );

    let evts = events.events();
    // Engine emits Accepted before checking FOK liquidity, then Rejected
    assert!(has_accepted(evts, 2), "FOK order must be accepted first");
    assert!(
        has_rejected(evts, 2, RejectReason::InsufficientLiquidity),
        "FOK must be rejected with InsufficientLiquidity"
    );
    // No trades should have occurred
    assert_eq!(count_trades(evts), 0, "FOK reject must produce no trades");
    // Resting ask must still be in the book
    assert_eq!(
        book.order_count(),
        1,
        "resting ask must survive FOK rejection"
    );
    assert!(!book.asks_empty(), "ask side must remain non-empty");
}

/// FOK market order rejected when entire opposite side is insufficient.
#[test]
fn test_fok_market_insufficient_liquidity() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    process_order(
        &mut book,
        limit(1, Side::Sell, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    let fok_buy = Order::new_market(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::FOK,
        FixedQty::from_f64(5.0),
        2000,
    );
    process_order(&mut book, fok_buy, &mut events);

    let evts = events.events();
    assert!(has_accepted(evts, 2));
    assert!(has_rejected(evts, 2, RejectReason::InsufficientLiquidity));
    assert_eq!(count_trades(evts), 0);
    assert_eq!(book.order_count(), 1, "resting ask must survive");
}

// ─── IOC: partial fill then cancel remainder ──────────────────────────────────

/// An IOC buy should fill what is available then cancel the unfilled remainder.
#[test]
fn test_ioc_partial_fill_remainder_cancelled() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    // Resting sell: 1.0 at 100.0
    process_order(
        &mut book,
        limit(1, Side::Sell, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // IOC buy for 3.0 — only 1.0 available
    process_order(
        &mut book,
        limit(2, Side::Buy, 100.0, 3.0, TimeInForce::IOC),
        &mut events,
    );

    let evts = events.events();
    assert!(has_accepted(evts, 2), "IOC order accepted");
    assert_eq!(count_trades(evts), 1, "exactly one trade");
    assert!(has_filled(evts, 1), "maker fully filled");
    assert!(has_partially_filled(evts, 2), "taker partially filled");
    assert!(has_cancelled(evts, 2), "IOC remainder cancelled");
    // Book must be empty — maker fully consumed, taker cancelled
    assert_eq!(book.order_count(), 0, "book must be empty after IOC match");
}

/// IOC sell: partial fill, remainder cancelled.
#[test]
fn test_ioc_sell_partial_fill() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    // Resting bid: 2.0 at 99.0
    process_order(
        &mut book,
        limit(1, Side::Buy, 99.0, 2.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // IOC sell for 5.0 — only 2.0 matched
    process_order(
        &mut book,
        limit(2, Side::Sell, 99.0, 5.0, TimeInForce::IOC),
        &mut events,
    );

    let evts = events.events();
    assert!(has_accepted(evts, 2));
    assert_eq!(count_trades(evts), 1);
    assert!(has_filled(evts, 1));
    assert!(has_partially_filled(evts, 2));
    assert!(has_cancelled(evts, 2));
    assert_eq!(book.order_count(), 0);
}

// ─── Market order on empty book ───────────────────────────────────────────────

/// A market buy into an empty ask side must be accepted then immediately cancelled.
#[test]
fn test_market_buy_empty_book_cancelled() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    process_order(&mut book, market(1, Side::Buy, 1.0), &mut events);

    let evts = events.events();
    assert!(has_accepted(evts, 1), "market order accepted");
    assert_eq!(count_trades(evts), 0, "no trades on empty book");
    assert!(
        has_cancelled(evts, 1),
        "market order cancelled — no resting"
    );
    assert_eq!(book.order_count(), 0);
}

/// A market sell into an empty bid side is also cancelled with full remaining qty.
#[test]
fn test_market_sell_empty_book_cancelled() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    process_order(&mut book, market(1, Side::Sell, 2.0), &mut events);

    let evts = events.events();
    assert!(has_accepted(evts, 1));
    assert_eq!(count_trades(evts), 0);
    assert!(has_cancelled(evts, 1));

    // Confirm remaining_qty in the cancel event is 2.0
    let cancel = evts.iter().find_map(|e| {
        if let MatchEvent::OrderCancelled {
            order_id,
            remaining_qty,
            ..
        } = e
        {
            if *order_id == 1 {
                return Some(*remaining_qty);
            }
        }
        None
    });
    assert_eq!(
        cancel,
        Some(FixedQty::from_f64(2.0)),
        "full qty should be remaining"
    );
}

// ─── Sweep across multiple price levels ───────────────────────────────────────

/// A market buy should consume asks in ascending price order (best ask first).
#[test]
fn test_market_buy_sweeps_multiple_price_levels() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    // Two ask levels: 1.0 at 100.0 and 2.0 at 101.0
    process_order(
        &mut book,
        limit(1, Side::Sell, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    process_order(
        &mut book,
        limit(2, Side::Sell, 101.0, 2.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // Market buy for 3.0 — should sweep both levels
    process_order(&mut book, market(3, Side::Buy, 3.0), &mut events);

    let evts = events.events();
    assert!(has_accepted(evts, 3));
    assert_eq!(count_trades(evts), 2, "two trades — one per price level");

    // First trade should be at the lower ask (100.0)
    let trades: Vec<_> = evts
        .iter()
        .filter_map(|e| {
            if let MatchEvent::Trade(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        trades[0].price,
        FixedPrice::from_f64(100.0),
        "first fill at best ask"
    );
    assert_eq!(trades[0].qty, FixedQty::from_f64(1.0));
    assert_eq!(
        trades[1].price,
        FixedPrice::from_f64(101.0),
        "second fill at next level"
    );
    assert_eq!(trades[1].qty, FixedQty::from_f64(2.0));

    // Both makers fully filled
    assert!(has_filled(evts, 1));
    assert!(has_filled(evts, 2));
    // Taker fully filled
    assert!(has_filled(evts, 3));
    // Book must be empty
    assert_eq!(book.order_count(), 0);
}

/// A limit sell should sweep bid levels from highest to lowest.
#[test]
fn test_limit_sell_sweeps_bid_levels() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    // Two bid levels: 1.0 at 101.0 (best) and 2.0 at 100.0
    process_order(
        &mut book,
        limit(1, Side::Buy, 101.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    process_order(
        &mut book,
        limit(2, Side::Buy, 100.0, 2.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // Limit sell for 3.0 at 100.0 — crosses both bids
    process_order(
        &mut book,
        limit(3, Side::Sell, 100.0, 3.0, TimeInForce::GTC),
        &mut events,
    );

    let evts = events.events();
    assert_eq!(count_trades(evts), 2);

    let trades: Vec<_> = evts
        .iter()
        .filter_map(|e| {
            if let MatchEvent::Trade(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    // Best bid first: 101.0
    assert_eq!(trades[0].price, FixedPrice::from_f64(101.0));
    assert_eq!(trades[1].price, FixedPrice::from_f64(100.0));

    assert!(has_filled(evts, 1));
    assert!(has_filled(evts, 2));
    assert!(has_filled(evts, 3));
    assert_eq!(book.order_count(), 0);
}

// ─── Self-trade ───────────────────────────────────────────────────────────────

/// The engine does not enforce self-trade prevention.
/// Two orders from the same conceptual client fill each other normally.
#[test]
fn test_self_trade_fills_normally() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    // "Client A" places a buy GTC
    process_order(
        &mut book,
        limit(1, Side::Buy, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // Same "Client A" places a matching sell — engine has no self-trade prevention
    process_order(
        &mut book,
        limit(2, Side::Sell, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );

    let evts = events.events();
    assert!(has_accepted(evts, 2));
    assert_eq!(
        count_trades(evts),
        1,
        "self-trade fills like any other match"
    );
    assert!(has_filled(evts, 1), "resting buy fully filled");
    assert!(has_filled(evts, 2), "incoming sell fully filled");
    assert_eq!(book.order_count(), 0, "book is empty after self-trade");
}

// ─── Stop-limit: trigger behaviour ───────────────────────────────────────────

/// A stop-limit buy should NOT trigger when the trade price is below stop_price,
/// and SHOULD trigger when the price reaches or exceeds stop_price.
#[test]
fn test_stop_limit_buy_not_triggered_before_stop_price() {
    let mut stop_book = StopBook::new();
    let mut events = EventBuffer::default();

    // Stop-limit buy: triggers at 100.0, limit price 101.0
    let order = stop_limit(1, Side::Buy, 100.0, 101.0, 1.0);
    stop_book.insert(order, &mut events);
    assert_eq!(stop_book.len(), 1);

    // Trade at 99.0 — below stop_price, should not trigger
    let triggered = stop_book.check_triggers(FixedPrice::from_f64(99.0), 2000, &mut events);
    assert!(
        triggered.is_empty(),
        "stop must not trigger below stop_price"
    );
    assert_eq!(stop_book.len(), 1, "stop order must remain pending");
}

/// Stop-limit buy triggers exactly at stop_price and is converted to a limit order.
#[test]
fn test_stop_limit_buy_triggers_at_stop_price() {
    let mut stop_book = StopBook::new();
    let mut events = EventBuffer::default();

    let order = stop_limit(1, Side::Buy, 100.0, 101.0, 1.0);
    stop_book.insert(order, &mut events);
    events.clear();

    // Trade exactly at stop_price
    let triggered = stop_book.check_triggers(FixedPrice::from_f64(100.0), 3000, &mut events);
    assert_eq!(triggered.len(), 1, "stop must trigger at stop_price");
    assert_eq!(triggered[0].id, 1);
    assert_eq!(
        triggered[0].order_type,
        OrderType::Limit,
        "stop-limit converts to limit"
    );
    assert_eq!(
        triggered[0].price,
        FixedPrice::from_f64(101.0),
        "limit price preserved"
    );
    assert_eq!(stop_book.len(), 0, "stop order removed from stop book");

    // StopTriggered event emitted
    let has_triggered_event = events
        .events()
        .iter()
        .any(|e| matches!(e, MatchEvent::StopTriggered { order_id, .. } if *order_id == 1));
    assert!(has_triggered_event, "StopTriggered event must be emitted");
}

/// Stop-limit sell triggers when trade price drops to or below stop_price.
#[test]
fn test_stop_limit_sell_triggers_at_or_below_stop_price() {
    let mut stop_book = StopBook::new();
    let mut events = EventBuffer::default();

    let order = Order::new_stop_limit(
        2,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(95.0), // limit price
        FixedPrice::from_f64(97.0), // stop price
        FixedQty::from_f64(1.0),
        2000,
    );
    stop_book.insert(order, &mut events);
    events.clear();

    // Price at 98.0 — above stop_price 97.0, no trigger
    let triggered = stop_book.check_triggers(FixedPrice::from_f64(98.0), 3000, &mut events);
    assert!(
        triggered.is_empty(),
        "sell stop must not trigger above stop_price"
    );

    // Price drops to 97.0 — triggers
    let triggered = stop_book.check_triggers(FixedPrice::from_f64(97.0), 4000, &mut events);
    assert_eq!(triggered.len(), 1);
    assert_eq!(triggered[0].order_type, OrderType::Limit);
    assert_eq!(stop_book.len(), 0);
}

/// Triggered stop order, when fed into the matching engine, executes normally.
#[test]
fn test_stop_limit_triggered_and_matched() {
    let mut book = new_book();
    let mut stop_book = StopBook::new();
    let mut events = EventBuffer::default();

    // Place a resting ask at 101.0
    process_order(
        &mut book,
        limit(1, Side::Sell, 101.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // Place stop-limit buy: triggers at 100.0, limit 101.0
    let stop_order = stop_limit(2, Side::Buy, 100.0, 101.0, 1.0);
    stop_book.insert(stop_order, &mut events);
    events.clear();

    // Simulate a trade at 100.0 — triggers the stop
    let mut triggered = stop_book.check_triggers(FixedPrice::from_f64(100.0), 5000, &mut events);
    assert_eq!(triggered.len(), 1);
    events.clear();

    // Feed the triggered (now limit) order into the book — should match against ask at 101.0
    for order in triggered.drain(..) {
        process_order(&mut book, order, &mut events);
    }

    let evts = events.events();
    assert_eq!(count_trades(evts), 1, "triggered order must match");
    assert!(has_filled(evts, 1), "resting ask filled");
    assert!(has_filled(evts, 2), "triggered order filled");
    assert_eq!(book.order_count(), 0);
}

// ─── Additional edge cases ────────────────────────────────────────────────────

/// Duplicate order ID is rejected with DuplicateOrderId.
#[test]
fn test_duplicate_order_id_rejected() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    process_order(
        &mut book,
        limit(1, Side::Buy, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    // Same ID again
    process_order(
        &mut book,
        limit(1, Side::Buy, 99.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    let evts = events.events();
    assert!(has_rejected(evts, 1, RejectReason::DuplicateOrderId));
}

/// Cancel a resting order — book becomes empty.
#[test]
fn test_cancel_resting_order() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    process_order(
        &mut book,
        limit(1, Side::Buy, 100.0, 1.0, TimeInForce::GTC),
        &mut events,
    );
    events.clear();

    cancel_order(&mut book, 1, 9999, &mut events);
    assert!(has_cancelled(events.events(), 1));
    assert_eq!(book.order_count(), 0);
}

/// Cancel a non-existent order produces OrderRejected(OrderNotFound).
#[test]
fn test_cancel_nonexistent_order() {
    let mut book = new_book();
    let mut events = EventBuffer::default();

    cancel_order(&mut book, 999, 1000, &mut events);
    assert!(has_rejected(
        events.events(),
        999,
        RejectReason::OrderNotFound
    ));
}
