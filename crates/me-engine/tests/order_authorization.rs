//! C-1: per-session order ownership on cancel/amend/query.
//!
//! A client may only cancel or amend an order its own session placed. An
//! attempt against another session's order is rejected with `OrderNotFound`
//! (without revealing the order exists), and the victim's order stays resting.
//! A privileged (`SessionId::NONE`) caller — WAL replay, admin, direct
//! `Engine::process` — bypasses the check.

use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_core::SessionId;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn sess(b: u8) -> SessionId {
    SessionId::from_bytes([b; 16])
}

fn place_owned(id: OrderId, price: f64, qty: f64, session: SessionId) -> Command {
    let mut o = Order::new_limit(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    );
    o.client_session_id = session;
    Command::PlaceOrder(o)
}

fn cancel(id: OrderId, ts: u64) -> Command {
    Command::CancelOrder {
        order_id: id,
        symbol: SymbolId(0),
        timestamp: ts,
    }
}

fn is_rejected_not_found(evs: &[MatchEvent], id: OrderId) -> bool {
    matches!(
        evs,
        [MatchEvent::OrderRejected { order_id, reason: RejectReason::OrderNotFound, .. }]
            if *order_id == id
    )
}

#[test]
fn cancel_by_other_session_is_rejected_and_order_survives() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let alice = sess(1);
    let bob = sess(2);

    e.process_with_session(place_owned(1, 100.0, 1.0, alice), alice);

    // Bob (different session) tries to cancel Alice's order.
    let evs = e.process_with_session(cancel(1, 2), bob).to_vec();
    assert!(
        is_rejected_not_found(&evs, 1),
        "cross-session cancel must be OrderNotFound, got {evs:?}"
    );

    // The order must still be resting — Alice can cancel it herself.
    let evs2 = e.process_with_session(cancel(1, 3), alice).to_vec();
    assert!(
        evs2.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })),
        "owner cancel must succeed, got {evs2:?}"
    );
}

#[test]
fn amend_by_other_session_is_rejected_and_order_unchanged() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let alice = sess(1);
    let bob = sess(2);

    e.process_with_session(place_owned(7, 100.0, 1.0, alice), alice);

    let amend_bob = Command::AmendOrder {
        order_id: 7,
        symbol: SymbolId(0),
        new_price: Some(FixedPrice::from_f64(105.0)),
        new_qty: None,
        timestamp: 2,
    };
    let evs = e.process_with_session(amend_bob, bob).to_vec();
    assert!(
        is_rejected_not_found(&evs, 7),
        "cross-session amend must be OrderNotFound, got {evs:?}"
    );

    // Owner amend still works.
    let amend_alice = Command::AmendOrder {
        order_id: 7,
        symbol: SymbolId(0),
        new_price: Some(FixedPrice::from_f64(105.0)),
        new_qty: None,
        timestamp: 3,
    };
    let evs2 = e.process_with_session(amend_alice, alice).to_vec();
    assert!(
        evs2.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderAmended { order_id: 7, .. })),
        "owner amend must succeed, got {evs2:?}"
    );
}

#[test]
fn privileged_none_session_can_cancel_any_order() {
    // WAL replay / admin path uses `process` (== NONE session) and must remain
    // able to cancel regardless of the order's owning session.
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let alice = sess(1);
    e.process_with_session(place_owned(9, 100.0, 1.0, alice), alice);

    let evs = e.process(cancel(9, 2)).to_vec();
    assert!(
        evs.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 9, .. })),
        "privileged NONE cancel must succeed, got {evs:?}"
    );
}

#[test]
fn unowned_order_is_manageable_by_any_session() {
    // An order placed with NONE owner (legacy / internal) stays manageable by
    // any session — this preserves operability without re-opening the hole,
    // since an attacker can never match a real victim session this way.
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let bob = sess(2);
    e.process(place_owned(11, 100.0, 1.0, SessionId::NONE));

    let evs = e.process_with_session(cancel(11, 2), bob).to_vec();
    assert!(
        evs.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 11, .. })),
        "NONE-owned order must be cancelable by any session, got {evs:?}"
    );
}
