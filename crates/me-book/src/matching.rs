use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::order::{Order, OrderStatus, OrderType, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::trade::Trade;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side};

use crate::book::OrderBook;
use crate::price_level::PriceLevelIter;

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
    match order.order_type {
        OrderType::StopLimit | OrderType::StopMarket => {
            if !order.stop_price.is_positive() {
                return Some(RejectReason::InvalidPrice);
            }
        }
        _ => {}
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
                // Shouldn't reach here — FOK was checked above
                unreachable!("FOK order should have been fully filled or rejected");
            }
            TimeInForce::GTC => {
                // Rest on book
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
            let maker = &book.order_slab.get(key).unwrap().order;
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

        // Update maker in slab
        let loc = book.get_order_location(maker_id).unwrap();
        let slab_key = loc.slab_key;
        let maker_order = book.get_order_mut_by_key(slab_key).unwrap();
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

    // Update level qty for partially filled makers
    // (already done by the order node mutations above, but we need to update PriceLevel.total_qty)
    // Actually, the PriceLevel.total_qty was not decremented for partial fills.
    // We need to fix this: decrement total_qty for all fills.
    // For fully filled orders, remove_order already handles it via PriceLevel::remove.
    // For partial fills, we need to manually update the level.
    // Let's update the ask level total_qty for partial fills.
    if let Some((_, level)) = book.best_ask_level_mut() {
        for (_, fill_qty, fully_filled) in &fills {
            if !fully_filled {
                level.total_qty -= *fill_qty;
            }
        }
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
            let maker = &book.order_slab.get(key).unwrap().order;
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

        let loc = book.get_order_location(maker_id).unwrap();
        let slab_key = loc.slab_key;
        let maker_order = book.get_order_mut_by_key(slab_key).unwrap();
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

    if let Some((_, level)) = book.best_bid_level_mut() {
        for (_, fill_qty, fully_filled) in &fills {
            if !fully_filled {
                level.total_qty -= *fill_qty;
            }
        }
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

    let current_order = book.get_order(order_id).copied().unwrap();
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

    let price_changed = new_price.is_some() && new_price.unwrap() != current_order.price;
    let qty_increased = new_qty.is_some() && new_qty.unwrap() > current_order.qty;

    if price_changed || qty_increased {
        // Cancel and re-insert — loses time priority
        let mut order = book.remove_order(order_id).unwrap();
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
        // In-place modification — retains time priority
        let slab_key = loc.slab_key;
        let order = book.get_order_mut_by_key(slab_key).unwrap();

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
