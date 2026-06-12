//! H-2: amending an order's price/qty must NOT re-emit `OrderAccepted`.
//!
//! The reinsert path previously called `process_order`, which unconditionally
//! emitted `OrderAccepted` for the (already-accepted) order — producing
//! `OrderAmended` followed by a spurious `OrderAccepted` and breaking client
//! state machines. A maker-only order amended into a crossing price must also
//! be rejected WITHOUT vanishing from the book.

use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{Order, TimeInForce, ORDER_FLAG_POST_ONLY};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn limit(id: OrderId, side: Side, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn amend_price(id: OrderId, price: f64, ts: u64) -> Command {
    Command::AmendOrder {
        order_id: id,
        symbol: SymbolId(0),
        new_price: Some(FixedPrice::from_f64(price)),
        new_qty: None,
        timestamp: ts,
    }
}

#[test]
fn amend_price_change_does_not_re_emit_order_accepted() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    e.process(limit(1, Side::Buy, 100.0, 1.0));

    let evs = e.process(amend_price(1, 101.0, 2)).to_vec();

    assert!(
        evs.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderAmended { order_id: 1, .. })),
        "expected OrderAmended, got {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })),
        "amend must NOT re-emit OrderAccepted, got {evs:?}"
    );
}

#[test]
fn post_only_amend_into_cross_is_rejected_and_order_survives() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());

    // Resting ask at 105.
    e.process(limit(2, Side::Sell, 105.0, 1.0));

    // POST_ONLY buy at 100 (rests, does not cross).
    let mut po = Order::new_limit(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        3000,
    );
    po.flags = ORDER_FLAG_POST_ONLY;
    e.process(Command::PlaceOrder(po));

    // Amend the POST_ONLY buy to 106 — it would now cross the ask at 105.
    let evs = e.process(amend_price(3, 106.0, 4)).to_vec();
    assert!(
        evs.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                order_id: 3,
                reason: RejectReason::WouldTakeMarket,
                ..
            }
        )),
        "crossing post-only amend must be rejected WouldTakeMarket, got {evs:?}"
    );

    // The original order must still be resting at its old price — a subsequent
    // cancel succeeds (it did not silently vanish).
    let cancel = Command::CancelOrder {
        order_id: 3,
        symbol: SymbolId(0),
        timestamp: 5,
    };
    let evs2 = e.process(cancel).to_vec();
    assert!(
        evs2.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 3, .. })),
        "post-only order must survive the rejected amend, got {evs2:?}"
    );
}
