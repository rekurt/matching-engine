use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::invariant::InvariantKind;
use me_core::order::{Order, OrderStatus, OrderType, TimeInForce, ORDER_FLAG_POST_ONLY};
use me_core::symbol::SymbolConfig;
use me_core::trade::Trade;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side};

use crate::book::OrderBook;
use crate::price_level::PriceLevelIter;

/// Emit an `InternalError` rejection for an order that cannot be processed
/// due to a book-invariant violation. Used by hot-path matching helpers
/// when they need to abort after calling `record_invariant_violation`.
#[cold]
#[inline(never)]
fn emit_internal_error(events: &mut EventBuffer, order_id: OrderId, timestamp: u64) {
    events.push(MatchEvent::OrderRejected {
        order_id,
        reason: RejectReason::InternalError,
        timestamp,
    });
}

// ─── Validation ──────────────────────────────────────────────────────────────

/// Validate an incoming order against symbol config.
/// Returns a RejectReason if invalid.
pub fn validate_order(order: &Order, config: &SymbolConfig) -> Option<RejectReason> {
    // Quantity validations
    if order.qty.is_zero() || order.remaining_qty.is_zero() {
        return Some(RejectReason::InvalidQuantity);
    }
    if order.qty < config.min_qty || order.qty > config.max_qty {
        return Some(RejectReason::InvalidQuantity);
    }
    if !order.qty.is_multiple_of(config.qty_step) {
        return Some(RejectReason::InvalidLotSize);
    }

    // Price validations (only for limit and stop-limit orders)
    match order.order_type {
        OrderType::Limit | OrderType::StopLimit => {
            if !order.price.is_positive() {
                return Some(RejectReason::InvalidPrice);
            }
            if config.price_step.raw() > 0 && order.price.raw() % config.price_step.raw() != 0 {
                return Some(RejectReason::InvalidTickSize);
            }
        }
        _ => {}
    }

    // Stop price validation for stop orders
    if matches!(
        order.order_type,
        OrderType::StopLimit | OrderType::StopMarket
    ) && !order.stop_price.is_positive()
    {
        return Some(RejectReason::InvalidPrice);
    }

    // GTD orders must carry a non-zero expiry.
    if matches!(order.tif, TimeInForce::GTD) && order.expire_at == 0 {
        return Some(RejectReason::InvalidExpiry);
    }

    None
}

// ─── Core matching entry point ───────────────────────────────────────────────

/// Process an incoming order against the book.
/// This is the main entry point for all order types.
pub fn process_order(book: &mut OrderBook, mut order: Order, events: &mut EventBuffer) {
    // Validate
    if let Some(reason) = validate_order(&order, &book.config) {
        events.push(MatchEvent::OrderRejected {
            order_id: order.id,
            reason,
            timestamp: order.timestamp,
        });
        return;
    }

    // Check duplicate
    if book.contains_order(order.id) {
        events.push(MatchEvent::OrderRejected {
            order_id: order.id,
            reason: RejectReason::DuplicateOrderId,
            timestamp: order.timestamp,
        });
        return;
    }

    // Dispatch by order type
    match order.order_type {
        OrderType::Limit => {
            // POST_ONLY: reject before accepting if the order would immediately
            // cross the spread (i.e., it would act as a taker, not a maker).
            if order.flags & ORDER_FLAG_POST_ONLY != 0 {
                let would_cross = match order.side {
                    Side::Buy => book.best_ask().is_some_and(|ask| ask <= order.price),
                    Side::Sell => book.best_bid().is_some_and(|bid| bid >= order.price),
                };
                if would_cross {
                    events.push(MatchEvent::OrderRejected {
                        order_id: order.id,
                        reason: RejectReason::WouldTakeMarket,
                        timestamp: order.timestamp,
                    });
                    return;
                }
            }
            events.push(MatchEvent::OrderAccepted {
                order_id: order.id,
                timestamp: order.timestamp,
            });
            match_limit(book, &mut order, events);
        }
        OrderType::Market => {
            events.push(MatchEvent::OrderAccepted {
                order_id: order.id,
                timestamp: order.timestamp,
            });
            match_market(book, &mut order, events);
        }
        OrderType::StopLimit | OrderType::StopMarket => {
            // Stop orders go to stop book — handled by engine layer
            // This shouldn't be called directly; engine routes stops.
            // But we accept them here for completeness.
            events.push(MatchEvent::OrderAccepted {
                order_id: order.id,
                timestamp: order.timestamp,
            });
        }
    }
}

// ─── Limit order matching ────────────────────────────────────────────────────

fn match_limit(book: &mut OrderBook, order: &mut Order, events: &mut EventBuffer) {
    // FOK: check if we can fill entirely before doing anything
    if order.tif == TimeInForce::FOK {
        let available = match order.side {
            Side::Buy => book.available_ask_qty(Some(order.price)),
            Side::Sell => book.available_bid_qty(Some(order.price)),
        };
        if available < order.remaining_qty {
            order.status = OrderStatus::Rejected;
            events.push(MatchEvent::OrderRejected {
                order_id: order.id,
                reason: RejectReason::InsufficientLiquidity,
                timestamp: order.timestamp,
            });
            return;
        }
    }

    // Match against opposite side
    match order.side {
        Side::Buy => match_buy_limit(book, order, events),
        Side::Sell => match_sell_limit(book, order, events),
    }

    // Post-matching handling
    if !order.remaining_qty.is_zero() {
        match order.tif {
            TimeInForce::IOC => {
                // Cancel remaining
                order.status = OrderStatus::Cancelled;
                events.push(MatchEvent::OrderCancelled {
                    order_id: order.id,
                    remaining_qty: order.remaining_qty,
                    timestamp: order.timestamp,
                });
            }
            TimeInForce::FOK => {
                // FOK was pre-checked against available liquidity and should
                // have fully filled. Reaching here means the available-qty
                // calculation disagreed with actual matching — i.e.
                // `level.total_qty` drifted from the sum of resting orders.
                // This is an internal invariant violation, NOT a user error.
                // Never panic on the hot path in production: record the
                // violation (policy decides halt vs. continue) and cancel the
                // unfilled remainder so a FOK order never rests on the book.
                book.record_invariant_violation(
                    InvariantKind::FokPartialFillAfterPrecheck,
                    "match_limit::fok_remainder",
                );
                order.status = OrderStatus::Cancelled;
                events.push(MatchEvent::OrderCancelled {
                    order_id: order.id,
                    remaining_qty: order.remaining_qty,
                    timestamp: order.timestamp,
                });
            }
            TimeInForce::GTC | TimeInForce::GTD => {
                // Rest on book. GTD additionally carries an expiry that the
                // engine layer enforces (lazy expiry sweep) — the book itself
                // treats GTD identically to GTC.
                book.insert_order(*order);
            }
        }
    }
}

fn match_buy_limit(book: &mut OrderBook, taker: &mut Order, events: &mut EventBuffer) {
    let limit_price = taker.price;

    loop {
        // Get best ask
        let best_ask_price = match book.best_ask() {
            Some(p) if p <= limit_price => p,
            _ => break, // No more crossing levels
        };

        match_taker_against_ask_level(book, taker, best_ask_price, events);

        if taker.remaining_qty.is_zero() {
            break;
        }
    }

    book.refresh_best(Side::Sell);
}

fn match_sell_limit(book: &mut OrderBook, taker: &mut Order, events: &mut EventBuffer) {
    let limit_price = taker.price;

    loop {
        let best_bid_price = match book.best_bid() {
            Some(p) if p >= limit_price => p,
            _ => break,
        };

        match_taker_against_bid_level(book, taker, best_bid_price, events);

        if taker.remaining_qty.is_zero() {
            break;
        }
    }

    book.refresh_best(Side::Buy);
}

// ─── Market order matching ───────────────────────────────────────────────────

fn match_market(book: &mut OrderBook, order: &mut Order, events: &mut EventBuffer) {
    // FOK: check entire opposite side
    if order.tif == TimeInForce::FOK {
        let available = match order.side {
            Side::Buy => book.available_ask_qty(None),
            Side::Sell => book.available_bid_qty(None),
        };
        if available < order.remaining_qty {
            order.status = OrderStatus::Rejected;
            events.push(MatchEvent::OrderRejected {
                order_id: order.id,
                reason: RejectReason::InsufficientLiquidity,
                timestamp: order.timestamp,
            });
            return;
        }
    }

    match order.side {
        Side::Buy => match_market_buy(book, order, events),
        Side::Sell => match_market_sell(book, order, events),
    }

    // Market orders NEVER rest on book. Cancel any remainder.
    if !order.remaining_qty.is_zero() {
        order.status = OrderStatus::Cancelled;
        events.push(MatchEvent::OrderCancelled {
            order_id: order.id,
            remaining_qty: order.remaining_qty,
            timestamp: order.timestamp,
        });
    }
}

fn match_market_buy(book: &mut OrderBook, taker: &mut Order, events: &mut EventBuffer) {
    while let Some(best_ask_price) = book.best_ask() {
        match_taker_against_ask_level(book, taker, best_ask_price, events);

        if taker.remaining_qty.is_zero() {
            break;
        }
    }

    book.refresh_best(Side::Sell);
}

fn match_market_sell(book: &mut OrderBook, taker: &mut Order, events: &mut EventBuffer) {
    while let Some(best_bid_price) = book.best_bid() {
        match_taker_against_bid_level(book, taker, best_bid_price, events);

        if taker.remaining_qty.is_zero() {
            break;
        }
    }

    book.refresh_best(Side::Buy);
}

// ─── Level-by-level matching helpers ─────────────────────────────────────────

fn match_taker_against_ask_level(
    book: &mut OrderBook,
    taker: &mut Order,
    ask_price: FixedPrice,
    events: &mut EventBuffer,
) {
    // We need to iterate the ask level, filling orders front-to-back (FIFO).
    // Collect keys to fill first, then mutate.
    let mut fills: Vec<(OrderId, FixedQty, bool)> = Vec::new(); // (maker_id, fill_qty, fully_filled)

    {
        let level = match book.best_ask_level() {
            Some((p, l)) if *p == ask_price => l,
            _ => return,
        };

        let mut iter = PriceLevelIter::new(level);
        while let Some(key) = iter.next_key(&book.order_slab) {
            // Invariant: key just came from PriceLevelIter, which walks
            // the level's intrusive list of slab keys.
            let maker = match book.order_slab.get(key) {
                Some(node) => &node.order,
                None => {
                    book.record_invariant_violation(
                        InvariantKind::SlabKeyFromIteratorMissing,
                        "match_taker_against_ask_level::collect_fills",
                    );
                    emit_internal_error(events, taker.id, taker.timestamp);
                    return;
                }
            };
            let fill_qty = taker.remaining_qty.min(maker.remaining_qty);

            fills.push((maker.id, fill_qty, fill_qty >= maker.remaining_qty));

            taker.remaining_qty -= fill_qty;
            taker.cumulative_qty += fill_qty;

            if taker.remaining_qty.is_zero() {
                break;
            }
        }
    }

    // Now apply fills
    for (maker_id, fill_qty, fully_filled) in &fills {
        let fill_qty = *fill_qty;
        let maker_id = *maker_id;

        // Generate trade
        let trade_id = book.next_trade_id();
        let seq = book.next_sequence();
        let trade = Trade {
            trade_id,
            symbol: book.symbol,
            price: ask_price,
            qty: fill_qty,
            maker_order_id: maker_id,
            taker_order_id: taker.id,
            maker_side: Side::Sell,
            timestamp: taker.timestamp,
            sequence: seq,
        };
        events.push(MatchEvent::Trade(trade));

        // Update maker in slab.
        // Invariant: maker_id comes from fills[] which we just built from
        // the slab. Violation here means the slab/index were mutated
        // concurrently (impossible — single-threaded) or corrupted.
        let slab_key = match book.get_order_location(maker_id) {
            Some(loc) => loc.slab_key,
            None => {
                book.record_invariant_violation(
                    InvariantKind::OrderLocationMissing,
                    "match_taker_against_ask_level::apply_fills",
                );
                emit_internal_error(events, taker.id, taker.timestamp);
                return;
            }
        };
        let maker_order = match book.get_order_mut_by_key(slab_key) {
            Some(order) => order,
            None => {
                book.record_invariant_violation(
                    InvariantKind::SlabKeyFromLocationMissing,
                    "match_taker_against_ask_level::apply_fills",
                );
                emit_internal_error(events, taker.id, taker.timestamp);
                return;
            }
        };
        maker_order.remaining_qty -= fill_qty;
        maker_order.cumulative_qty += fill_qty;

        if *fully_filled {
            maker_order.status = OrderStatus::Filled;
            events.push(MatchEvent::OrderFilled {
                order_id: maker_id,
                filled_qty: fill_qty,
                price: ask_price,
                timestamp: taker.timestamp,
            });
        } else {
            maker_order.status = OrderStatus::PartiallyFilled;
            events.push(MatchEvent::OrderPartiallyFilled {
                order_id: maker_id,
                filled_qty: fill_qty,
                remaining_qty: maker_order.remaining_qty,
                price: ask_price,
                timestamp: taker.timestamp,
            });
        }
    }

    // Remove fully filled makers from book
    for (maker_id, _, fully_filled) in &fills {
        if *fully_filled {
            book.remove_order(*maker_id);
        }
    }

    // Update level qty for partially filled makers. Fully-filled makers are
    // removed via remove_order (which adjusts total_qty in PriceLevel::remove);
    // partial fills must be decremented here. Use checked subtraction so that
    // a divergence between the level aggregate and the sum of its orders is
    // surfaced as an invariant violation instead of silently saturating to 0.
    let mut total_qty_underflow = false;
    if let Some((_, level)) = book.best_ask_level_mut() {
        for (_, fill_qty, fully_filled) in &fills {
            if !fully_filled {
                match level.total_qty.checked_sub(*fill_qty) {
                    Some(v) => level.total_qty = v,
                    None => {
                        level.total_qty = FixedQty::ZERO;
                        total_qty_underflow = true;
                    }
                }
            }
        }
    }
    if total_qty_underflow {
        book.record_invariant_violation(
            InvariantKind::LevelTotalQtyUnderflow,
            "match_taker_against_ask_level::total_qty",
        );
    }

    // Emit taker fill events
    let total_filled: FixedQty = fills
        .iter()
        .map(|(_, q, _)| *q)
        .fold(FixedQty::ZERO, |a, b| a + b);
    if !total_filled.is_zero() {
        if taker.remaining_qty.is_zero() {
            taker.status = OrderStatus::Filled;
            events.push(MatchEvent::OrderFilled {
                order_id: taker.id,
                filled_qty: total_filled,
                price: ask_price,
                timestamp: taker.timestamp,
            });
        } else {
            taker.status = OrderStatus::PartiallyFilled;
            events.push(MatchEvent::OrderPartiallyFilled {
                order_id: taker.id,
                filled_qty: total_filled,
                remaining_qty: taker.remaining_qty,
                price: ask_price,
                timestamp: taker.timestamp,
            });
        }
    }
}

fn match_taker_against_bid_level(
    book: &mut OrderBook,
    taker: &mut Order,
    bid_price: FixedPrice,
    events: &mut EventBuffer,
) {
    let mut fills: Vec<(OrderId, FixedQty, bool)> = Vec::new();

    {
        let level = match book.best_bid_level() {
            Some((p, l)) if p == bid_price => l,
            _ => return,
        };

        let mut iter = PriceLevelIter::new(level);
        while let Some(key) = iter.next_key(&book.order_slab) {
            let maker = match book.order_slab.get(key) {
                Some(node) => &node.order,
                None => {
                    book.record_invariant_violation(
                        InvariantKind::SlabKeyFromIteratorMissing,
                        "match_taker_against_bid_level::collect_fills",
                    );
                    emit_internal_error(events, taker.id, taker.timestamp);
                    return;
                }
            };
            let fill_qty = taker.remaining_qty.min(maker.remaining_qty);

            fills.push((maker.id, fill_qty, fill_qty >= maker.remaining_qty));

            taker.remaining_qty -= fill_qty;
            taker.cumulative_qty += fill_qty;

            if taker.remaining_qty.is_zero() {
                break;
            }
        }
    }

    for (maker_id, fill_qty, fully_filled) in &fills {
        let fill_qty = *fill_qty;
        let maker_id = *maker_id;

        let trade_id = book.next_trade_id();
        let seq = book.next_sequence();
        let trade = Trade {
            trade_id,
            symbol: book.symbol,
            price: bid_price,
            qty: fill_qty,
            maker_order_id: maker_id,
            taker_order_id: taker.id,
            maker_side: Side::Buy,
            timestamp: taker.timestamp,
            sequence: seq,
        };
        events.push(MatchEvent::Trade(trade));

        let slab_key = match book.get_order_location(maker_id) {
            Some(loc) => loc.slab_key,
            None => {
                book.record_invariant_violation(
                    InvariantKind::OrderLocationMissing,
                    "match_taker_against_bid_level::apply_fills",
                );
                emit_internal_error(events, taker.id, taker.timestamp);
                return;
            }
        };
        let maker_order = match book.get_order_mut_by_key(slab_key) {
            Some(order) => order,
            None => {
                book.record_invariant_violation(
                    InvariantKind::SlabKeyFromLocationMissing,
                    "match_taker_against_bid_level::apply_fills",
                );
                emit_internal_error(events, taker.id, taker.timestamp);
                return;
            }
        };
        maker_order.remaining_qty -= fill_qty;
        maker_order.cumulative_qty += fill_qty;

        if *fully_filled {
            maker_order.status = OrderStatus::Filled;
            events.push(MatchEvent::OrderFilled {
                order_id: maker_id,
                filled_qty: fill_qty,
                price: bid_price,
                timestamp: taker.timestamp,
            });
        } else {
            maker_order.status = OrderStatus::PartiallyFilled;
            events.push(MatchEvent::OrderPartiallyFilled {
                order_id: maker_id,
                filled_qty: fill_qty,
                remaining_qty: maker_order.remaining_qty,
                price: bid_price,
                timestamp: taker.timestamp,
            });
        }
    }

    for (maker_id, _, fully_filled) in &fills {
        if *fully_filled {
            book.remove_order(*maker_id);
        }
    }

    let mut total_qty_underflow = false;
    if let Some((_, level)) = book.best_bid_level_mut() {
        for (_, fill_qty, fully_filled) in &fills {
            if !fully_filled {
                match level.total_qty.checked_sub(*fill_qty) {
                    Some(v) => level.total_qty = v,
                    None => {
                        level.total_qty = FixedQty::ZERO;
                        total_qty_underflow = true;
                    }
                }
            }
        }
    }
    if total_qty_underflow {
        book.record_invariant_violation(
            InvariantKind::LevelTotalQtyUnderflow,
            "match_taker_against_bid_level::total_qty",
        );
    }

    let total_filled: FixedQty = fills
        .iter()
        .map(|(_, q, _)| *q)
        .fold(FixedQty::ZERO, |a, b| a + b);
    if !total_filled.is_zero() {
        if taker.remaining_qty.is_zero() {
            taker.status = OrderStatus::Filled;
            events.push(MatchEvent::OrderFilled {
                order_id: taker.id,
                filled_qty: total_filled,
                price: bid_price,
                timestamp: taker.timestamp,
            });
        } else {
            taker.status = OrderStatus::PartiallyFilled;
            events.push(MatchEvent::OrderPartiallyFilled {
                order_id: taker.id,
                filled_qty: total_filled,
                remaining_qty: taker.remaining_qty,
                price: bid_price,
                timestamp: taker.timestamp,
            });
        }
    }
}

// ─── Cancel ──────────────────────────────────────────────────────────────────

/// Cancel a resting order by ID.
pub fn cancel_order(
    book: &mut OrderBook,
    order_id: OrderId,
    timestamp: u64,
    events: &mut EventBuffer,
) {
    match book.remove_order(order_id) {
        Some(order) => {
            events.push(MatchEvent::OrderCancelled {
                order_id,
                remaining_qty: order.remaining_qty,
                timestamp,
            });
        }
        None => {
            events.push(MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::OrderNotFound,
                timestamp,
            });
        }
    }
}

// ─── Amend ───────────────────────────────────────────────────────────────────

/// Amend a resting order.
/// - If only qty decreases (no price change): modify in-place, retain time priority.
/// - If price changes or qty increases: cancel and re-insert (loses time priority).
pub fn amend_order(
    book: &mut OrderBook,
    order_id: OrderId,
    new_price: Option<FixedPrice>,
    new_qty: Option<FixedQty>,
    timestamp: u64,
    events: &mut EventBuffer,
) {
    let loc = match book.get_order_location(order_id) {
        Some(loc) => *loc,
        None => {
            events.push(MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::OrderNotFound,
                timestamp,
            });
            return;
        }
    };

    // Invariant: location was just verified to exist, so get_order must
    // succeed. Diverging here means order_index and order_slab are
    // out-of-sync — caught via the invariant policy.
    let current_order = match book.get_order(order_id).copied() {
        Some(o) => o,
        None => {
            book.record_invariant_violation(
                InvariantKind::OrderLocationMissing,
                "amend_order::current_order",
            );
            emit_internal_error(events, order_id, timestamp);
            return;
        }
    };
    let final_price = new_price.unwrap_or(current_order.price);
    let final_qty = new_qty.unwrap_or(current_order.qty);

    // Validate new values
    if let Some(p) = new_price {
        if !book.config.validate_price(p) {
            events.push(MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::InvalidTickSize,
                timestamp,
            });
            return;
        }
    }
    if let Some(q) = new_qty {
        if !book.config.validate_qty(q) {
            events.push(MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::InvalidQuantity,
                timestamp,
            });
            return;
        }
    }

    let price_changed = matches!(new_price, Some(p) if p != current_order.price);
    let qty_increased = matches!(new_qty, Some(q) if q > current_order.qty);

    if price_changed || qty_increased {
        // Cancel and re-insert — loses time priority.
        // Invariant: we just verified the order exists via get_order_location.
        let mut order = match book.remove_order(order_id) {
            Some(o) => o,
            None => {
                book.record_invariant_violation(
                    InvariantKind::OrderRemovalAfterVerification,
                    "amend_order::reinsert_path",
                );
                emit_internal_error(events, order_id, timestamp);
                return;
            }
        };
        order.price = final_price;
        order.qty = final_qty;
        // Adjust remaining_qty proportionally
        let already_filled = order.cumulative_qty;
        if final_qty.raw() > already_filled.raw() {
            order.remaining_qty = FixedQty::from_raw(final_qty.raw() - already_filled.raw());
        } else {
            order.remaining_qty = FixedQty::ZERO;
        }
        order.timestamp = timestamp;

        events.push(MatchEvent::OrderAmended {
            order_id,
            new_price: final_price,
            new_qty: final_qty,
            timestamp,
        });

        if !order.remaining_qty.is_zero() {
            // Re-insert. If new price crosses, it will match.
            process_order(book, order, events);
        }
    } else {
        // In-place modification — retains time priority.
        // Invariant: loc came from get_order_location above; its slab_key
        // must still be valid because this function is single-threaded.
        let slab_key = loc.slab_key;
        let order = match book.get_order_mut_by_key(slab_key) {
            Some(o) => o,
            None => {
                book.record_invariant_violation(
                    InvariantKind::SlabKeyFromLocationMissing,
                    "amend_order::in_place",
                );
                emit_internal_error(events, order_id, timestamp);
                return;
            }
        };

        let old_remaining = order.remaining_qty;
        let qty_delta = if final_qty < current_order.qty {
            // Reducing qty
            let reduction = FixedQty::from_raw(current_order.qty.raw() - final_qty.raw());
            if reduction >= old_remaining {
                // Entire remaining cancelled
                order.remaining_qty = FixedQty::ZERO;
            } else {
                order.remaining_qty = FixedQty::from_raw(old_remaining.raw() - reduction.raw());
            }
            order.qty = final_qty;
            reduction
        } else {
            FixedQty::ZERO
        };

        events.push(MatchEvent::OrderAmended {
            order_id,
            new_price: final_price,
            new_qty: final_qty,
            timestamp,
        });

        // If remaining is zero after reduction, remove from book
        if order.remaining_qty.is_zero() {
            book.remove_order(order_id);
            events.push(MatchEvent::OrderCancelled {
                order_id,
                remaining_qty: FixedQty::ZERO,
                timestamp,
            });
        } else {
            // Update price level total_qty
            // We need to adjust the level's total_qty by the reduction
            if !qty_delta.is_zero() {
                match loc.side {
                    Side::Buy => {
                        if let Some(level) = book.bids_level_mut(loc.price) {
                            level.total_qty -= qty_delta;
                        }
                    }
                    Side::Sell => {
                        if let Some(level) = book.asks_level_mut(loc.price) {
                            level.total_qty -= qty_delta;
                        }
                    }
                }
            }
        }
    }
}
