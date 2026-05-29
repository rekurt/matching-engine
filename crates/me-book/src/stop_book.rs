use std::cmp::Reverse;
use std::collections::BTreeMap;

use rustc_hash::FxHashMap;

use me_core::events::{EventBuffer, MatchEvent};
use me_core::order::Order;
use me_core::types::{FixedPrice, OrderId, Side};
use me_core::SessionId;

/// Location of a stop order for O(1) cancel.
#[derive(Clone, Copy, Debug)]
struct StopLocation {
    stop_price: FixedPrice,
    side: Side,
    index: usize, // index within the Vec at that price level
}

/// Manages stop orders that are waiting to be triggered.
///
/// Stop-buy orders trigger when market price rises to/above stop_price.
/// Stop-sell orders trigger when market price falls to/below stop_price.
pub struct StopBook {
    /// Stop-buy orders: triggered when price >= stop_price.
    /// `BTreeMap<stop_price ascending, Vec<Order>>`
    stop_buys: BTreeMap<FixedPrice, Vec<Order>>,

    /// Stop-sell orders: triggered when price <= stop_price.
    /// `BTreeMap<Reverse(stop_price), Vec<Order>>` — lowest stop_price first
    stop_sells: BTreeMap<Reverse<FixedPrice>, Vec<Order>>,

    /// Index for O(1) cancel of fixed-price stop orders.
    stop_index: FxHashMap<OrderId, StopLocation>,

    /// Trailing stops kept flat (not price-keyed): their `stop_price` ratchets
    /// toward the market on every trade, so a price-indexed structure would be
    /// re-keyed constantly. They are few relative to resting orders and must all
    /// be re-evaluated per trade anyway, so a flat scan is the simpler choice.
    trailing: FxHashMap<OrderId, Order>,
}

impl Default for StopBook {
    fn default() -> Self {
        Self::new()
    }
}

impl StopBook {
    pub fn new() -> Self {
        Self {
            stop_buys: BTreeMap::new(),
            stop_sells: BTreeMap::new(),
            stop_index: FxHashMap::default(),
            trailing: FxHashMap::default(),
        }
    }

    /// Number of pending stop orders.
    pub fn len(&self) -> usize {
        self.stop_index.len() + self.trailing.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stop_index.is_empty() && self.trailing.is_empty()
    }

    /// Insert a stop order into the stop book.
    pub fn insert(&mut self, order: Order, events: &mut EventBuffer) {
        let stop_price = order.stop_price;
        let order_id = order.id;
        let side = order.side;

        // Trailing stops live in a flat map; their stop_price ratchets per trade.
        if order.is_trailing_stop() {
            self.trailing.insert(order_id, order);
            events.push(MatchEvent::OrderAccepted {
                order_id,
                timestamp: order.timestamp,
            });
            return;
        }

        match side {
            Side::Buy => {
                let orders = self.stop_buys.entry(stop_price).or_default();
                let idx = orders.len();
                orders.push(order);
                self.stop_index.insert(
                    order_id,
                    StopLocation {
                        stop_price,
                        side,
                        index: idx,
                    },
                );
            }
            Side::Sell => {
                let orders = self.stop_sells.entry(Reverse(stop_price)).or_default();
                let idx = orders.len();
                orders.push(order);
                self.stop_index.insert(
                    order_id,
                    StopLocation {
                        stop_price,
                        side,
                        index: idx,
                    },
                );
            }
        }

        events.push(MatchEvent::OrderAccepted {
            order_id,
            timestamp: order.timestamp,
        });
    }

    /// Cancel a pending stop order.
    pub fn cancel(&mut self, order_id: OrderId, timestamp: u64, events: &mut EventBuffer) -> bool {
        // Trailing stops are held in the flat map.
        if let Some(order) = self.trailing.remove(&order_id) {
            events.push(MatchEvent::OrderCancelled {
                order_id,
                remaining_qty: order.remaining_qty,
                timestamp,
            });
            return true;
        }

        let loc = match self.stop_index.remove(&order_id) {
            Some(loc) => loc,
            None => return false,
        };

        match loc.side {
            Side::Buy => {
                if let Some(orders) = self.stop_buys.get_mut(&loc.stop_price) {
                    // Find and remove the order (swap_remove for O(1))
                    if let Some(pos) = orders.iter().position(|o| o.id == order_id) {
                        let order = orders.swap_remove(pos);
                        // Update index for the swapped order (if any)
                        if pos < orders.len() {
                            let swapped_id = orders[pos].id;
                            if let Some(swapped_loc) = self.stop_index.get_mut(&swapped_id) {
                                swapped_loc.index = pos;
                            }
                        }
                        if orders.is_empty() {
                            self.stop_buys.remove(&loc.stop_price);
                        }
                        events.push(MatchEvent::OrderCancelled {
                            order_id,
                            remaining_qty: order.remaining_qty,
                            timestamp,
                        });
                        return true;
                    }
                }
            }
            Side::Sell => {
                if let Some(orders) = self.stop_sells.get_mut(&Reverse(loc.stop_price)) {
                    if let Some(pos) = orders.iter().position(|o| o.id == order_id) {
                        let order = orders.swap_remove(pos);
                        if pos < orders.len() {
                            let swapped_id = orders[pos].id;
                            if let Some(swapped_loc) = self.stop_index.get_mut(&swapped_id) {
                                swapped_loc.index = pos;
                            }
                        }
                        if orders.is_empty() {
                            self.stop_sells.remove(&Reverse(loc.stop_price));
                        }
                        events.push(MatchEvent::OrderCancelled {
                            order_id,
                            remaining_qty: order.remaining_qty,
                            timestamp,
                        });
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Check for triggered stop orders based on the last trade price.
    /// Returns a list of orders that should be converted and fed into the matching engine.
    pub fn check_triggers(
        &mut self,
        last_trade_price: FixedPrice,
        timestamp: u64,
        events: &mut EventBuffer,
    ) -> Vec<Order> {
        let mut triggered = Vec::new();

        // Trigger stop-buys: price >= stop_price (ascending order)
        let mut triggered_buy_prices = Vec::new();
        for &stop_price in self.stop_buys.keys() {
            if last_trade_price >= stop_price {
                triggered_buy_prices.push(stop_price);
            } else {
                break; // BTreeMap is sorted ascending, no more triggers
            }
        }

        for price in triggered_buy_prices {
            if let Some(orders) = self.stop_buys.remove(&price) {
                for mut order in orders {
                    self.stop_index.remove(&order.id);
                    events.push(MatchEvent::StopTriggered {
                        order_id: order.id,
                        trigger_price: last_trade_price,
                        timestamp,
                    });
                    order.convert_stop();
                    triggered.push(order);
                }
            }
        }

        // Trigger stop-sells: price <= stop_price (descending order)
        let mut triggered_sell_prices = Vec::new();
        for &Reverse(stop_price) in self.stop_sells.keys() {
            if last_trade_price <= stop_price {
                triggered_sell_prices.push(stop_price);
            } else {
                break;
            }
        }

        for price in triggered_sell_prices {
            if let Some(orders) = self.stop_sells.remove(&Reverse(price)) {
                for mut order in orders {
                    self.stop_index.remove(&order.id);
                    events.push(MatchEvent::StopTriggered {
                        order_id: order.id,
                        trigger_price: last_trade_price,
                        timestamp,
                    });
                    order.convert_stop();
                    triggered.push(order);
                }
            }
        }

        // Trailing stops: ratchet each stop_price toward the market in the
        // protective direction, then trigger if the market has reversed to it.
        //   buy  stop: stop = min(stop, price + trail); triggers when price >= stop
        //   sell stop: stop = max(stop, price - trail); triggers when price <= stop
        if !self.trailing.is_empty() {
            let p = last_trade_price.raw();
            // Ratcheting is independent per order, so HashMap iteration order is
            // fine here; the TRIGGER order is not — collect (timestamp, id) and
            // sort so simultaneous triggers feed matching deterministically
            // (time priority, id tiebreak), like the explicit order of fixed
            // stops, instead of HashMap-iteration order that differs across
            // replicas / WAL replay.
            let mut triggered_ids: Vec<(u64, OrderId)> = Vec::new();
            for (id, order) in self.trailing.iter_mut() {
                let off = order.trail_offset.raw();
                let cur = order.stop_price.raw();
                let new_stop = match order.side {
                    Side::Buy => cur.min(p.saturating_add(off)),
                    Side::Sell => cur.max(p.saturating_sub(off)),
                };
                order.stop_price = FixedPrice::from_raw(new_stop);
                let hit = match order.side {
                    Side::Buy => p >= new_stop,
                    Side::Sell => p <= new_stop,
                };
                if hit {
                    triggered_ids.push((order.timestamp, *id));
                }
            }
            triggered_ids.sort_unstable();
            for (_, id) in triggered_ids {
                if let Some(mut order) = self.trailing.remove(&id) {
                    events.push(MatchEvent::StopTriggered {
                        order_id: id,
                        trigger_price: last_trade_price,
                        timestamp,
                    });
                    order.convert_stop();
                    triggered.push(order);
                }
            }
        }

        triggered
    }

    /// Check if a stop order exists.
    pub fn contains(&self, order_id: OrderId) -> bool {
        self.stop_index.contains_key(&order_id) || self.trailing.contains_key(&order_id)
    }

    /// Borrow a pending stop order by id (fixed-price or trailing), for callers
    /// that must inspect it (e.g. validate a GTD expiry) before acting.
    pub fn get(&self, order_id: OrderId) -> Option<&Order> {
        if let Some(order) = self.trailing.get(&order_id) {
            return Some(order);
        }
        let loc = self.stop_index.get(&order_id)?;
        let level = match loc.side {
            Side::Buy => self.stop_buys.get(&loc.stop_price)?,
            Side::Sell => self.stop_sells.get(&Reverse(loc.stop_price))?,
        };
        level.get(loc.index)
    }

    /// Get all pending stop orders (for snapshots).
    pub fn all_stop_orders(&self) -> Vec<Order> {
        let mut orders = Vec::new();
        for level_orders in self.stop_buys.values() {
            orders.extend_from_slice(level_orders);
        }
        for level_orders in self.stop_sells.values() {
            orders.extend_from_slice(level_orders);
        }
        orders.extend(self.trailing.values().copied());
        orders
    }

    /// Collect all pending stop orders belonging to a given client session.
    pub fn scan_by_session_id(&self, session_id: &SessionId) -> Vec<Order> {
        self.all_stop_orders()
            .into_iter()
            .filter(|order| order.client_session_id == *session_id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::events::EventBuffer;
    use me_core::order::{Order, OrderType, TimeInForce};
    use me_core::types::{FixedPrice, FixedQty, SymbolId};

    fn make_stop_limit_buy(id: OrderId, stop_price: f64, limit_price: f64, qty: f64) -> Order {
        Order::new_stop_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(limit_price),
            FixedPrice::from_f64(stop_price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
    }

    fn make_stop_market_sell(id: OrderId, stop_price: f64, qty: f64) -> Order {
        Order::new_stop_market(
            id,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(stop_price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
    }

    #[test]
    fn test_insert_and_trigger_stop_buy() {
        let mut book = StopBook::new();
        let mut events = EventBuffer::default();

        let order = make_stop_limit_buy(1, 100.0, 101.0, 1.0);
        book.insert(order, &mut events);
        assert_eq!(book.len(), 1);

        // Price hasn't reached stop — no triggers
        let triggered = book.check_triggers(FixedPrice::from_f64(99.0), 1000, &mut events);
        assert!(triggered.is_empty());
        assert_eq!(book.len(), 1);

        // Price reaches stop
        let triggered = book.check_triggers(FixedPrice::from_f64(100.0), 2000, &mut events);
        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0].id, 1);
        assert_eq!(triggered[0].order_type, OrderType::Limit);
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn test_insert_and_trigger_stop_sell() {
        let mut book = StopBook::new();
        let mut events = EventBuffer::default();

        let order = make_stop_market_sell(2, 95.0, 1.0);
        book.insert(order, &mut events);

        // Price above stop — no trigger
        let triggered = book.check_triggers(FixedPrice::from_f64(96.0), 1000, &mut events);
        assert!(triggered.is_empty());

        // Price drops to stop
        let triggered = book.check_triggers(FixedPrice::from_f64(95.0), 2000, &mut events);
        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0].order_type, OrderType::Market);
    }

    #[test]
    fn test_cancel_stop_order() {
        let mut book = StopBook::new();
        let mut events = EventBuffer::default();

        book.insert(make_stop_limit_buy(1, 100.0, 101.0, 1.0), &mut events);
        book.insert(make_stop_limit_buy(2, 100.0, 102.0, 2.0), &mut events);

        assert!(book.cancel(1, 5000, &mut events));
        assert_eq!(book.len(), 1);
        assert!(!book.contains(1));
        assert!(book.contains(2));

        // Cancel non-existent
        assert!(!book.cancel(99, 5000, &mut events));
    }

    #[test]
    fn test_multiple_triggers_same_price() {
        let mut book = StopBook::new();
        let mut events = EventBuffer::default();

        book.insert(make_stop_limit_buy(1, 100.0, 101.0, 1.0), &mut events);
        book.insert(make_stop_limit_buy(2, 100.0, 102.0, 2.0), &mut events);

        let triggered = book.check_triggers(FixedPrice::from_f64(100.0), 1000, &mut events);
        assert_eq!(triggered.len(), 2);
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn test_trigger_at_exact_price() {
        let mut book = StopBook::new();
        let mut events = EventBuffer::default();

        book.insert(make_stop_limit_buy(1, 100.0, 101.0, 1.0), &mut events);

        // Just below — no trigger
        let triggered = book.check_triggers(
            FixedPrice::from_raw(FixedPrice::from_f64(100.0).raw() - 1),
            1000,
            &mut events,
        );
        assert!(triggered.is_empty());

        // Exact price — triggers
        let triggered = book.check_triggers(FixedPrice::from_f64(100.0), 2000, &mut events);
        assert_eq!(triggered.len(), 1);
    }

    fn trailing_sell(id: OrderId, init_stop: f64, trail: f64, qty: f64) -> Order {
        Order::new_stop_market(
            id,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(init_stop),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_trail_offset(FixedPrice::from_f64(trail))
    }

    fn trailing_buy(id: OrderId, init_stop: f64, trail: f64, qty: f64) -> Order {
        Order::new_stop_market(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(init_stop),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_trail_offset(FixedPrice::from_f64(trail))
    }

    #[test]
    fn test_trailing_sell_ratchets_up_then_triggers() {
        let mut book = StopBook::new();
        let mut ev = EventBuffer::default();
        book.insert(trailing_sell(1, 95.0, 5.0, 1.0), &mut ev);
        assert_eq!(book.len(), 1);

        // Market rises — stop ratchets up, no trigger.
        assert!(book
            .check_triggers(FixedPrice::from_f64(100.0), 1, &mut ev)
            .is_empty());
        assert!(book
            .check_triggers(FixedPrice::from_f64(110.0), 2, &mut ev)
            .is_empty());
        // Small pullback above the ratcheted stop (105) — still no trigger.
        assert!(book
            .check_triggers(FixedPrice::from_f64(106.0), 3, &mut ev)
            .is_empty());
        // Falls to the ratcheted stop (105) — triggers, converts to market.
        let t = book.check_triggers(FixedPrice::from_f64(105.0), 4, &mut ev);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].order_type, OrderType::Market);
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn test_trailing_buy_ratchets_down_then_triggers() {
        let mut book = StopBook::new();
        let mut ev = EventBuffer::default();
        book.insert(trailing_buy(1, 105.0, 5.0, 1.0), &mut ev);

        // Market falls — stop ratchets down (to 100), no trigger.
        assert!(book
            .check_triggers(FixedPrice::from_f64(100.0), 1, &mut ev)
            .is_empty());
        assert!(book
            .check_triggers(FixedPrice::from_f64(95.0), 2, &mut ev)
            .is_empty());
        // Rebounds to the ratcheted stop (100) — triggers.
        let t = book.check_triggers(FixedPrice::from_f64(100.0), 3, &mut ev);
        assert_eq!(t.len(), 1);
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn test_trailing_stop_cancel_and_contains() {
        let mut book = StopBook::new();
        let mut ev = EventBuffer::default();
        book.insert(trailing_sell(7, 95.0, 5.0, 1.0), &mut ev);
        assert!(book.contains(7));
        assert!(book.cancel(7, 9, &mut ev));
        assert!(!book.contains(7));
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn test_scan_by_session_id_filters_pending_stops() {
        let mut book = StopBook::new();
        let mut events = EventBuffer::default();

        let session_a = SessionId::from_bytes([11; 16]);
        let session_b = SessionId::from_bytes([22; 16]);

        let mut order_a = make_stop_limit_buy(1, 100.0, 101.0, 1.0);
        order_a.client_session_id = session_a;
        let mut order_b = make_stop_market_sell(2, 95.0, 2.0);
        order_b.client_session_id = session_b;

        book.insert(order_a, &mut events);
        book.insert(order_b, &mut events);

        let session_a_orders = book.scan_by_session_id(&session_a);
        assert_eq!(session_a_orders.len(), 1);
        assert_eq!(session_a_orders[0].id, 1);
    }
}
