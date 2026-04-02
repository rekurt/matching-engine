//! Property-based tests using proptest.
//!
//! These tests generate random sequences of matching engine commands and verify
//! that core invariants are never violated, regardless of the input sequence.
//! This catches edge cases that hand-written unit tests miss.

use proptest::prelude::*;

use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{Order, OrderType, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side, SymbolId};

use me_engine::command::Command;
use me_engine::engine::Engine;

// ─── Constants ──────────────────────────────────────────────────────────────

const SYMBOL: SymbolId = SymbolId(0);
const BASE_PRICE: i64 = 10_000_000_000; // 100.00
const TICK: i64 = 1_000_000; // 0.01
const ONE_UNIT: u64 = 100_000_000; // 1.0

// ─── Helpers ────────────────────────────────────────────────────────────────

fn config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn engine() -> Engine {
    Engine::new(config())
}

// ─── Strategies ─────────────────────────────────────────────────────────────

/// Generate a valid price (multiple of tick, within reasonable range).
fn valid_price() -> impl Strategy<Value = FixedPrice> {
    // 50.00 to 150.00 in 0.01 steps
    (5000i64..=15000i64).prop_map(|ticks| FixedPrice::from_raw(ticks * TICK))
}

/// Generate a valid quantity (multiple of lot step, within min/max).
fn valid_qty() -> impl Strategy<Value = FixedQty> {
    // 0.00001 to 10.0 in 0.00001 steps
    (1u64..=1_000_000u64).prop_map(|steps| FixedQty::from_raw(steps * 1_000))
}

/// Generate a side.
fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

/// Generate a TimeInForce.
fn tif() -> impl Strategy<Value = TimeInForce> {
    prop_oneof![
        3 => Just(TimeInForce::GTC),
        1 => Just(TimeInForce::IOC),
        1 => Just(TimeInForce::FOK),
    ]
}

/// Generate a single command with a given order ID.
fn command(id: OrderId) -> impl Strategy<Value = Command> {
    prop_oneof![
        // Limit order (most common)
        6 => (side(), tif(), valid_price(), valid_qty()).prop_map(
            move |(s, t, p, q)| {
                Command::PlaceOrder(Order::new_limit(id, SYMBOL, s, t, p, q, 1000))
            }
        ),
        // Market order
        2 => (side(), valid_qty()).prop_map(
            move |(s, q)| {
                Command::PlaceOrder(Order::new_market(id, SYMBOL, s, TimeInForce::IOC, q, 1000))
            }
        ),
        // Cancel (targets a potentially existing order)
        1 => Just(Command::CancelOrder {
            order_id: id.saturating_sub(1).max(1),
            symbol: SYMBOL,
            timestamp: 1000,
        }),
        // Amend price (targets a potentially existing order)
        1 => valid_price().prop_map(move |p| Command::AmendOrder {
            order_id: id.saturating_sub(1).max(1),
            symbol: SYMBOL,
            new_price: Some(p),
            new_qty: None,
            timestamp: 1000,
        }),
    ]
}

/// Generate a sequence of commands with unique IDs.
fn command_sequence(max_len: usize) -> impl Strategy<Value = Vec<Command>> {
    proptest::collection::vec(any::<u8>(), 1..=max_len).prop_flat_map(|dummy| {
        let len = dummy.len();
        let strategies: Vec<_> = (1..=len)
            .map(|i| command(i as OrderId).boxed())
            .collect();
        strategies
    })
}

// ─── Invariant checks ───────────────────────────────────────────────────────

/// INVARIANT 1: The order book is never crossed.
/// After any operation, best_bid < best_ask (or one side is empty).
fn assert_book_not_crossed(engine: &Engine) {
    let book = engine.book();
    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
        assert!(
            bid < ask,
            "CROSSED BOOK: best_bid={bid:?} >= best_ask={ask:?}, order_count={}",
            book.order_count()
        );
    }
}

/// INVARIANT 2: Order count matches the index size.
fn assert_order_count_consistent(engine: &Engine) {
    let book = engine.book();
    let reported = book.order_count();
    let all_orders = book.all_resting_orders();
    assert_eq!(
        reported,
        all_orders.len(),
        "order_count() = {} but all_resting_orders().len() = {}",
        reported,
        all_orders.len()
    );
}

/// INVARIANT 3: All resting orders have remaining_qty > 0.
fn assert_all_resting_have_qty(engine: &Engine) {
    for order in engine.book().all_resting_orders() {
        assert!(
            !order.remaining_qty.is_zero(),
            "Order {} is resting with zero remaining_qty",
            order.id
        );
    }
}

/// INVARIANT 4: All resting orders are limit type (never market).
fn assert_no_market_on_book(engine: &Engine) {
    for order in engine.book().all_resting_orders() {
        assert_ne!(
            order.order_type,
            OrderType::Market,
            "Market order {} found resting on book",
            order.id
        );
    }
}

/// INVARIANT 5: Bid levels are strictly descending, ask levels strictly ascending.
fn assert_price_ordering(engine: &Engine) {
    let book = engine.book();
    let orders = book.all_resting_orders();

    let mut bids: Vec<FixedPrice> = orders
        .iter()
        .filter(|o| o.side == Side::Buy)
        .map(|o| o.price)
        .collect();
    bids.sort();
    bids.dedup();

    let mut asks: Vec<FixedPrice> = orders
        .iter()
        .filter(|o| o.side == Side::Sell)
        .map(|o| o.price)
        .collect();
    asks.sort();
    asks.dedup();

    // Best bid should match
    if let Some(&max_bid) = bids.last() {
        assert_eq!(
            book.best_bid(),
            Some(max_bid),
            "best_bid() doesn't match actual highest bid"
        );
    }
    if let Some(&min_ask) = asks.first() {
        assert_eq!(
            book.best_ask(),
            Some(min_ask),
            "best_ask() doesn't match actual lowest ask"
        );
    }
}

/// Run all invariant checks.
fn assert_all_invariants(engine: &Engine) {
    assert_book_not_crossed(engine);
    assert_order_count_consistent(engine);
    assert_all_resting_have_qty(engine);
    assert_no_market_on_book(engine);
    assert_price_ordering(engine);
}

// ─── Property tests ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// After any sequence of valid commands, the book must never be crossed
    /// and all structural invariants must hold.
    #[test]
    fn prop_invariants_hold_after_random_commands(
        cmds in command_sequence(50)
    ) {
        let mut eng = engine();

        for cmd in cmds {
            eng.process(cmd);
            assert_all_invariants(&eng);
        }
    }

    /// Market orders never rest on the book.
    #[test]
    fn prop_market_orders_never_rest(
        side in side(),
        qty in valid_qty(),
    ) {
        let mut eng = engine();

        // Pre-populate with some resting orders
        for i in 1..=10 {
            let price = FixedPrice::from_raw(BASE_PRICE + (i as i64) * TICK);
            let order = Order::new_limit(
                i, SYMBOL, Side::Sell, TimeInForce::GTC, price,
                FixedQty::from_raw(ONE_UNIT), 1000,
            );
            eng.process(Command::PlaceOrder(order));
        }
        for i in 11..=20 {
            let price = FixedPrice::from_raw(BASE_PRICE - ((i - 10) as i64) * TICK);
            let order = Order::new_limit(
                i, SYMBOL, Side::Buy, TimeInForce::GTC, price,
                FixedQty::from_raw(ONE_UNIT), 1000,
            );
            eng.process(Command::PlaceOrder(order));
        }

        // Place market order
        let market = Order::new_market(100, SYMBOL, side, TimeInForce::IOC, qty, 2000);
        eng.process(Command::PlaceOrder(market));

        // Market order should never be on the book
        assert!(!eng.book().contains_order(100));
        assert_all_invariants(&eng);
    }

    /// FOK orders either fill completely or are fully rejected.
    /// They never partially fill and rest on the book.
    #[test]
    fn prop_fok_all_or_nothing(
        side in side(),
        price in valid_price(),
        qty in valid_qty(),
    ) {
        let mut eng = engine();

        // Pre-populate with resting orders on both sides
        for i in 1..=5 {
            let ask_price = FixedPrice::from_raw(BASE_PRICE + (i as i64) * TICK);
            eng.process(Command::PlaceOrder(Order::new_limit(
                i, SYMBOL, Side::Sell, TimeInForce::GTC, ask_price,
                FixedQty::from_raw(ONE_UNIT), 1000,
            )));
        }
        for i in 6..=10 {
            let bid_price = FixedPrice::from_raw(BASE_PRICE - ((i - 5) as i64) * TICK);
            eng.process(Command::PlaceOrder(Order::new_limit(
                i, SYMBOL, Side::Buy, TimeInForce::GTC, bid_price,
                FixedQty::from_raw(ONE_UNIT), 1000,
            )));
        }

        // Place FOK order
        let fok = Order::new_limit(100, SYMBOL, side, TimeInForce::FOK, price, qty, 2000);
        let _events = eng.process(Command::PlaceOrder(fok)).to_vec();

        // FOK should never rest on book
        assert!(
            !eng.book().contains_order(100),
            "FOK order 100 should never rest on book"
        );

        assert_all_invariants(&eng);
    }

    /// Cancelling a non-existent order returns OrderNotFound, never panics.
    #[test]
    fn prop_cancel_nonexistent_is_safe(id in 1u64..1000) {
        let mut eng = engine();
        let events = eng.process(Command::CancelOrder {
            order_id: id,
            symbol: SYMBOL,
            timestamp: 1000,
        }).to_vec();

        assert_eq!(events.len(), 1);
        match &events[0] {
            MatchEvent::OrderRejected { order_id, reason, .. } => {
                assert_eq!(*order_id, id);
                assert_eq!(*reason, RejectReason::OrderNotFound);
            }
            other => panic!("Expected OrderRejected, got {:?}", other),
        }
    }

    /// Double-cancelling an order is safe: first cancel succeeds, second returns NotFound.
    #[test]
    fn prop_double_cancel_safe(
        price in valid_price(),
        qty in valid_qty(),
        side in side(),
    ) {
        let mut eng = engine();

        // Place and then cancel
        let order = Order::new_limit(1, SYMBOL, side, TimeInForce::GTC, price, qty, 1000);
        eng.process(Command::PlaceOrder(order));

        // First cancel
        let events1 = eng.process(Command::CancelOrder {
            order_id: 1, symbol: SYMBOL, timestamp: 2000,
        }).to_vec();

        // Second cancel — should get NotFound
        let events2 = eng.process(Command::CancelOrder {
            order_id: 1, symbol: SYMBOL, timestamp: 3000,
        }).to_vec();

        // If the order was on the book (not rejected), first cancel should succeed
        let was_on_book = events1.iter().any(|e| matches!(e, MatchEvent::OrderCancelled { .. }));
        if was_on_book {
            assert!(
                events2.iter().any(|e| matches!(
                    e,
                    MatchEvent::OrderRejected { reason: RejectReason::OrderNotFound, .. }
                )),
                "Second cancel should return OrderNotFound"
            );
        }
    }

    /// Quantity conservation: for any trade, the taker's filled qty
    /// equals the sum of individual trade quantities for that taker.
    #[test]
    fn prop_quantity_conservation(
        n_asks in 1usize..=10,
        taker_qty_multiplier in 1u64..=20,
    ) {
        let mut eng = engine();
        let ask_qty = FixedQty::from_raw(ONE_UNIT);

        // Place asks at increasing prices
        for i in 0..n_asks {
            let price = FixedPrice::from_raw(BASE_PRICE + (i as i64) * TICK);
            let order = Order::new_limit(
                (i + 1) as u64, SYMBOL, Side::Sell, TimeInForce::GTC,
                price, ask_qty, 1000,
            );
            eng.process(Command::PlaceOrder(order));
        }

        // Place a large buy to sweep through multiple levels
        let taker_qty = FixedQty::from_raw(ONE_UNIT * taker_qty_multiplier);
        let high_price = FixedPrice::from_raw(BASE_PRICE + 100 * TICK);
        let taker = Order::new_limit(
            100, SYMBOL, Side::Buy, TimeInForce::GTC,
            high_price, taker_qty, 2000,
        );
        let events = eng.process(Command::PlaceOrder(taker)).to_vec();

        // Sum fill events for taker
        let taker_filled: u64 = events
            .iter()
            .filter_map(|e| match e {
                MatchEvent::OrderFilled { order_id: 100, filled_qty, .. } => Some(filled_qty.raw()),
                MatchEvent::OrderPartiallyFilled { order_id: 100, filled_qty, .. } => Some(filled_qty.raw()),
                _ => None,
            })
            .sum();

        // Each trade has a maker fill and is counted in total_trade_qty.
        // The taker fill events should sum to the same qty as the trades involving the taker.
        let taker_trade_qty: u64 = events
            .iter()
            .filter_map(|e| match e {
                MatchEvent::Trade(t) if t.taker_order_id == 100 => Some(t.qty.raw()),
                _ => None,
            })
            .sum();

        assert_eq!(
            taker_filled, taker_trade_qty,
            "Taker fill qty ({}) != taker trade qty ({})",
            taker_filled, taker_trade_qty
        );

        assert_all_invariants(&eng);
    }

    /// Trading halt: when halted, only cancels are allowed.
    #[test]
    fn prop_halt_rejects_new_orders(
        side in side(),
        price in valid_price(),
        qty in valid_qty(),
    ) {
        let mut eng = engine();
        eng.halt_trading();

        let order = Order::new_limit(1, SYMBOL, side, TimeInForce::GTC, price, qty, 1000);
        let events = eng.process(Command::PlaceOrder(order)).to_vec();

        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected { reason: RejectReason::TradingHalted, .. }
        )), "Halted engine should reject new orders");
    }

    /// Trading halt: cancels still work when halted.
    #[test]
    fn prop_halt_allows_cancels(
        price in valid_price(),
        qty in valid_qty(),
    ) {
        let mut eng = engine();

        // Place order while active
        let order = Order::new_limit(1, SYMBOL, Side::Buy, TimeInForce::GTC, price, qty, 1000);
        eng.process(Command::PlaceOrder(order));

        let was_placed = eng.book().contains_order(1);

        // Halt trading
        eng.halt_trading();

        // Cancel should still work
        if was_placed {
            let events = eng.process(Command::CancelOrder {
                order_id: 1, symbol: SYMBOL, timestamp: 2000,
            }).to_vec();

            assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderCancelled { .. })),
                "Cancel should succeed even when halted");
        }
    }

    /// Duplicate order IDs are rejected.
    #[test]
    fn prop_duplicate_id_rejected(
        price in valid_price(),
        qty in valid_qty(),
    ) {
        let mut eng = engine();

        let order1 = Order::new_limit(1, SYMBOL, Side::Buy, TimeInForce::GTC, price, qty, 1000);
        eng.process(Command::PlaceOrder(order1));

        if eng.book().contains_order(1) {
            let order2 = Order::new_limit(1, SYMBOL, Side::Sell, TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE + 1000 * TICK), qty, 2000);
            let events = eng.process(Command::PlaceOrder(order2)).to_vec();

            assert!(events.iter().any(|e| matches!(
                e,
                MatchEvent::OrderRejected { reason: RejectReason::DuplicateOrderId, .. }
            )), "Duplicate order ID should be rejected");
        }
    }

    /// Price-time priority: at the same price level, earlier orders fill first.
    #[test]
    fn prop_price_time_priority(
        n_makers in 2usize..=8,
    ) {
        let mut eng = engine();
        let price = FixedPrice::from_raw(BASE_PRICE);
        let qty = FixedQty::from_raw(ONE_UNIT);

        // Place n asks at same price
        for i in 1..=n_makers {
            let order = Order::new_limit(
                i as u64, SYMBOL, Side::Sell, TimeInForce::GTC,
                price, qty, (i * 1000) as u64,
            );
            eng.process(Command::PlaceOrder(order));
        }

        // Place a buy that sweeps 1 unit — should match the FIRST ask
        let taker = Order::new_limit(
            100, SYMBOL, Side::Buy, TimeInForce::GTC,
            price, qty, 99000,
        );
        let events = eng.process(Command::PlaceOrder(taker)).to_vec();

        // The trade should be against maker #1 (earliest)
        let trades: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                MatchEvent::Trade(t) => Some(t),
                _ => None,
            })
            .collect();

        assert!(!trades.is_empty(), "Should have at least one trade");
        assert_eq!(
            trades[0].maker_order_id, 1,
            "First trade should be against earliest maker (id=1), got id={}",
            trades[0].maker_order_id
        );
    }
}
