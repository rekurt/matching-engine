use std::cmp::Reverse;
use std::collections::BTreeMap;

use rustc_hash::FxHashMap;

use me_core::events::{EventBuffer, MatchEvent};
use me_core::order::Order;
use me_core::types::{FixedPrice, OrderId, Side};

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
    /// BTreeMap<stop_price ascending, Vec<Order>>
    stop_buys: BTreeMap<FixedPrice, Vec<Order>>,

    /// Stop-sell orders: triggered when price <= stop_price.
    /// BTreeMap<Reverse(stop_price) so lowest stop_price is first, Vec<Order>>
    stop_sells: BTreeMap<Reverse<FixedPrice>, Vec<Order>>,

    /// Index for O(1) cancel of stop orders.
    stop_index: FxHashMap<OrderId, StopLocation>,
}

impl StopBook {
    pub fn new() -> Self {
        Self {
            stop_buys: BTreeMap::new(),
            stop_sells: BTreeMap::new(),
            stop_index: FxHashMap::default(),
        }
    }

    /// Number of pending stop orders.
    pub fn len(&self) -> usize {
        self.stop_index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stop_index.is_empty()
    }

    /// Insert a stop order into the stop book.
    pub fn insert(&mut self, order: Order, events: &mut EventBuffer) {
        let stop_price = order.stop_price;
        let order_id = order.id;
        let side = order.side;

        match side {
            Side::Buy => {
                let orders = self.stop_buys.entry(stop_price).or_insert_with(Vec::new);
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
                let orders = self
                    .stop_sells
                    .entry(Reverse(stop_price))
                    .or_insert_with(Vec::new);
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
        for (&stop_price, _) in &self.stop_buys {
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
        for (&Reverse(stop_price), _) in &self.stop_sells {
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

        triggered
    }

    /// Check if a stop order exists.
    pub fn contains(&self, order_id: OrderId) -> bool {
        self.stop_index.contains_key(&order_id)
    }

    /// Get all pending stop orders (for snapshots).
    pub fn all_stop_orders(&self) -> Vec<Order> {
        let mut orders = Vec::new();
        for (_, level_orders) in &self.stop_buys {
            orders.extend_from_slice(level_orders);
        }
        for (_, level_orders) in &self.stop_sells {
            orders.extend_from_slice(level_orders);
        }
        orders
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
}
