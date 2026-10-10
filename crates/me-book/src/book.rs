use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustc_hash::FxHashMap;

use me_core::invariant::{InvariantKind, InvariantPolicy};
use me_core::order::Order;
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId, SequenceId, Side, SymbolId, TradeId};

use crate::pool::{Slab, SlabKey};
use crate::price_level::{OrderNode, PriceLevel};

/// Tracks where an order lives in the book for O(1) cancel/amend.
#[derive(Clone, Copy, Debug)]
pub struct OrderLocation {
    pub price: FixedPrice,
    pub side: Side,
    pub slab_key: SlabKey,
}

pub struct OrderBook {
    pub symbol: SymbolId,
    pub config: SymbolConfig,
    /// Bids sorted descending: Reverse so first() = highest bid (best bid).
    bids: BTreeMap<Reverse<FixedPrice>, PriceLevel>,
    /// Asks sorted ascending: first() = lowest ask (best ask).
    asks: BTreeMap<FixedPrice, PriceLevel>,
    /// O(1) lookup: OrderId → location in the book.
    order_index: FxHashMap<OrderId, OrderLocation>,
    /// Shared slab allocator for all order nodes in this book.
    pub order_slab: Slab<OrderNode>,
    /// Cached best bid/ask for O(1) access.
    best_bid: Option<FixedPrice>,
    best_ask: Option<FixedPrice>,
    /// Monotonic sequence counter for this book.
    pub sequence: SequenceId,
    /// Trade ID generator for this book.
    pub trade_id_gen: TradeId,
    /// Running total of invariant violations observed on this book.
    /// Surfaced to Prometheus via the engine layer. Atomic so the inner
    /// handler can bump it while other fields are borrowed.
    invariant_violations_total: AtomicU64,
    /// Set to true when an invariant violation triggered a symbol halt
    /// under `InvariantPolicy::HaltSymbol`. Once halted, the engine must
    /// stop accepting commands for this symbol until operator intervention.
    invariant_halted: AtomicBool,
}

impl OrderBook {
    pub fn new(config: SymbolConfig) -> Self {
        let symbol = config.id;
        Self {
            symbol,
            config,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            order_index: FxHashMap::default(),
            order_slab: Slab::with_capacity(4096),
            best_bid: None,
            best_ask: None,
            sequence: 0,
            trade_id_gen: 0,
            invariant_violations_total: AtomicU64::new(0),
            invariant_halted: AtomicBool::new(false),
        }
    }

    /// Current cumulative count of invariant violations observed on this
    /// book since creation. Engine layer scrapes this into a Prometheus
    /// counter.
    #[inline]
    pub fn invariant_violations_total(&self) -> u64 {
        self.invariant_violations_total.load(Ordering::Relaxed)
    }

    /// Whether a prior invariant violation has halted this symbol under
    /// `InvariantPolicy::HaltSymbol`. Engine layer MUST check this before
    /// accepting commands and reject with `InternalError` if true.
    #[inline]
    pub fn is_halted_by_invariant(&self) -> bool {
        self.invariant_halted.load(Ordering::Relaxed)
    }

    /// Record a matching-engine invariant violation and apply the symbol's
    /// configured policy.
    ///
    /// Under `Strict`, panics with full diagnostic context — the caller
    /// never returns from this function and the following emit-rejection
    /// code becomes unreachable.
    ///
    /// Under `HaltSymbol`/`Continue`, logs and returns. The caller is
    /// responsible for emitting an `OrderRejected{InternalError}` event
    /// and unwinding its current operation cleanly.
    ///
    /// Marked `#[cold]` + `#[inline(never)]` because this path is
    /// exercised only on bugs — keeping it out of the hot instruction
    /// stream helps i-cache utilization.
    #[cold]
    #[inline(never)]
    pub fn record_invariant_violation(&self, kind: InvariantKind, context: &'static str) {
        self.invariant_violations_total
            .fetch_add(1, Ordering::Relaxed);
        let symbol = self.symbol;
        match self.config.invariant_policy {
            InvariantPolicy::Strict => {
                // Panic message is structured for log scrapers.
                panic!(
                    "BUG: matching invariant violated: kind={} context={} symbol={:?}; \
                     book state is corrupt, aborting",
                    kind.as_str(),
                    context,
                    symbol
                );
            }
            InvariantPolicy::HaltSymbol => {
                self.invariant_halted.store(true, Ordering::Relaxed);
                tracing::error!(
                    target: "matching::invariant",
                    kind = kind.as_str(),
                    context = context,
                    symbol = ?symbol,
                    "invariant violated; halting symbol"
                );
            }
            InvariantPolicy::Continue => {
                tracing::error!(
                    target: "matching::invariant",
                    kind = kind.as_str(),
                    context = context,
                    symbol = ?symbol,
                    "invariant violated; continuing"
                );
            }
        }
    }

    #[inline]
    pub fn best_bid(&self) -> Option<FixedPrice> {
        self.best_bid
    }

    #[inline]
    pub fn best_ask(&self) -> Option<FixedPrice> {
        self.best_ask
    }

    #[inline]
    pub fn next_sequence(&mut self) -> SequenceId {
        self.sequence += 1;
        self.sequence
    }

    #[inline]
    pub fn next_trade_id(&mut self) -> TradeId {
        self.trade_id_gen += 1;
        self.trade_id_gen
    }

    /// Check if an order ID already exists in the book.
    #[inline]
    pub fn contains_order(&self, order_id: OrderId) -> bool {
        self.order_index.contains_key(&order_id)
    }

    /// Get order location for cancel/amend.
    #[inline]
    pub fn get_order_location(&self, order_id: OrderId) -> Option<&OrderLocation> {
        self.order_index.get(&order_id)
    }

    /// Get a reference to a resting order by ID.
    pub fn get_order(&self, order_id: OrderId) -> Option<&Order> {
        let loc = self.order_index.get(&order_id)?;
        self.order_slab.get(loc.slab_key).map(|node| &node.order)
    }

    /// Collect all resting orders belonging to a given client session.
    ///
    /// Used by the OpenOrders snapshot handler (protocol v2 GetOpenOrders).
    /// O(N) scan over all resting orders — intended for occasional resync,
    /// NOT a hot-path operation.
    pub fn scan_by_session_id(&self, session_id: &me_core::SessionId) -> Vec<Order> {
        self.order_index
            .values()
            .filter_map(|loc| {
                let order = &self.order_slab.get(loc.slab_key)?.order;
                if order.client_session_id == *session_id {
                    Some(*order)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get a mutable reference to a resting order by its slab key.
    #[inline]
    pub fn get_order_mut_by_key(&mut self, key: SlabKey) -> Option<&mut Order> {
        self.order_slab.get_mut(key).map(|node| &mut node.order)
    }

    /// Total number of resting orders in the book.
    pub fn order_count(&self) -> usize {
        self.order_index.len()
    }

    /// Number of bid price levels.
    pub fn bid_level_count(&self) -> usize {
        self.bids.len()
    }

    /// Number of ask price levels.
    pub fn ask_level_count(&self) -> usize {
        self.asks.len()
    }

    /// Conservative execution-price bound, including hidden quantity at resting levels.
    pub fn execution_price_bound(&self, order: &Order) -> Option<FixedPrice> {
        use me_core::order::OrderType;
        match (order.side, order.order_type) {
            (Side::Buy, OrderType::Market) => self.asks.keys().next_back().copied(),
            (Side::Sell, OrderType::Market) => self.best_bid(),
            (Side::Buy, OrderType::Limit) => Some(order.price),
            (Side::Sell, OrderType::Limit) => Some(
                self.best_bid()
                    .map_or(order.price, |bid| bid.max(order.price)),
            ),
            _ => None,
        }
    }

    pub fn can_rest(&self, order: &Order) -> bool {
        let level = match order.side {
            Side::Buy => self.bids.get(&Reverse(order.price)),
            Side::Sell => self.asks.get(&order.price),
        };
        let Some(level) = level else {
            return true;
        };
        let replaced = self
            .get_order(order.id)
            .filter(|old| old.side == order.side && old.price == order.price);
        let existing = level
            .total_qty
            .raw()
            .saturating_sub(replaced.map_or(0, |old| old.remaining_qty.raw()));
        existing.checked_add(order.remaining_qty.raw()).is_some()
            && (replaced.is_some() || level.order_count < u32::MAX)
    }

    /// Insert at its limit price; return false on duplicate id or level capacity overflow.
    pub fn insert_order(&mut self, mut order: Order) -> bool {
        if self.contains_order(order.id) || !self.can_rest(&order) {
            return false;
        }
        let price = order.price;
        let side = order.side;
        order.sequence = self.next_sequence();

        let slab_key = match side {
            Side::Buy => {
                let level = self
                    .bids
                    .entry(Reverse(price))
                    .or_insert_with(|| PriceLevel::new(price));
                level.push_back(order, &mut self.order_slab)
            }
            Side::Sell => {
                let level = self
                    .asks
                    .entry(price)
                    .or_insert_with(|| PriceLevel::new(price));
                level.push_back(order, &mut self.order_slab)
            }
        };

        self.order_index.insert(
            order.id,
            OrderLocation {
                price,
                side,
                slab_key,
            },
        );

        self.update_best_after_insert(side, price);
        true
    }

    /// Remove an order from the book by its ID.
    /// Returns the removed Order, or None if not found.
    pub fn remove_order(&mut self, order_id: OrderId) -> Option<Order> {
        let loc = self.order_index.remove(&order_id)?;
        let order = match loc.side {
            Side::Buy => {
                let level = self.bids.get_mut(&Reverse(loc.price))?;
                let order = level.remove(loc.slab_key, &mut self.order_slab);
                if level.is_empty() {
                    self.bids.remove(&Reverse(loc.price));
                }
                order
            }
            Side::Sell => {
                let level = self.asks.get_mut(&loc.price)?;
                let order = level.remove(loc.slab_key, &mut self.order_slab);
                if level.is_empty() {
                    self.asks.remove(&loc.price);
                }
                order
            }
        };

        self.refresh_best(loc.side);
        Some(order)
    }

    /// Get the first (best) ask price level.
    pub fn best_ask_level(&self) -> Option<(&FixedPrice, &PriceLevel)> {
        self.asks.iter().next()
    }

    /// Get mutable access to the best ask level.
    pub fn best_ask_level_mut(&mut self) -> Option<(&FixedPrice, &mut PriceLevel)> {
        self.asks.iter_mut().next()
    }

    /// Get the first (best) bid price level.
    pub fn best_bid_level(&self) -> Option<(FixedPrice, &PriceLevel)> {
        self.bids.iter().next().map(|(Reverse(p), l)| (*p, l))
    }

    /// Get mutable access to the best bid level.
    pub fn best_bid_level_mut(&mut self) -> Option<(FixedPrice, &mut PriceLevel)> {
        self.bids.iter_mut().next().map(|(Reverse(p), l)| (*p, l))
    }

    /// Remove the best ask price level entirely (when it's been fully consumed).
    pub fn remove_best_ask(&mut self) -> Option<FixedPrice> {
        let price = *self.asks.keys().next()?;
        self.asks.remove(&price);
        self.refresh_best(Side::Sell);
        Some(price)
    }

    /// Remove the best bid price level entirely.
    pub fn remove_best_bid(&mut self) -> Option<FixedPrice> {
        let Reverse(price) = *self.bids.keys().next()?;
        self.bids.remove(&Reverse(price));
        self.refresh_best(Side::Buy);
        Some(price)
    }

    /// Remove an order from the order_index only (used when the matching engine
    /// has already removed the node from the price level).
    pub fn remove_from_index(&mut self, order_id: OrderId) {
        self.order_index.remove(&order_id);
    }

    /// Check if the ask side is empty.
    #[inline]
    pub fn asks_empty(&self) -> bool {
        self.asks.is_empty()
    }

    /// Check if the bid side is empty.
    #[inline]
    pub fn bids_empty(&self) -> bool {
        self.bids.is_empty()
    }

    /// Sum of all available quantity on the ask side at prices <= max_price.
    /// If max_price is None, sum entire ask side. Saturates at FixedQty::MAX.
    pub fn available_ask_qty(&self, max_price: Option<FixedPrice>) -> FixedQty {
        let mut total = FixedQty::ZERO;
        for (price, level) in &self.asks {
            if let Some(max) = max_price {
                if *price > max {
                    break;
                }
            }
            total = total.saturating_add(level.total_qty);
        }
        total
    }

    /// Sum of all available quantity on the bid side at prices >= min_price.
    /// If min_price is None, sum entire bid side. Saturates at FixedQty::MAX.
    pub fn available_bid_qty(&self, min_price: Option<FixedPrice>) -> FixedQty {
        let mut total = FixedQty::ZERO;
        for (Reverse(price), level) in &self.bids {
            if let Some(min) = min_price {
                if *price < min {
                    break;
                }
            }
            total = total.saturating_add(level.total_qty);
        }
        total
    }

    pub fn matching_work_exceeds(&self, order: &Order, budget: usize) -> bool {
        use me_core::order::OrderType;
        let limit = (order.order_type == OrderType::Limit).then_some(order.price);
        let mut remaining = order.remaining_qty.raw();
        let mut work = 0u128;
        let mut visit = |level: &PriceLevel, remaining: &mut u64| {
            let mut replenished = std::collections::VecDeque::new();
            let mut iter = crate::price_level::PriceLevelIter::new(level);
            while *remaining > 0 {
                let Some(key) = iter.next_key(&self.order_slab) else {
                    break;
                };
                let Some(node) = self.order_slab.get(key) else {
                    return true;
                };
                let maker = &node.order;
                let fill = (*remaining).min(maker.remaining_qty.raw());
                if fill == 0 {
                    continue;
                }
                *remaining -= fill;
                work += 1;
                if work > budget as u128 {
                    return true;
                }
                if maker.is_iceberg() && fill == maker.remaining_qty.raw() {
                    let hidden = maker.open_qty().raw().saturating_sub(fill);
                    if hidden > 0 {
                        replenished
                            .push_back((hidden, maker.display_qty.min(maker.remaining_qty).raw()));
                    }
                }
            }
            while *remaining > 0 {
                let Some((open, slice)) = replenished.pop_front() else {
                    break;
                };
                if slice == 0 {
                    return true;
                }
                let fill = (*remaining).min(open).min(slice);
                *remaining -= fill;
                work += 1;
                if work > budget as u128 {
                    return true;
                }
                if open > fill {
                    replenished.push_back((open - fill, slice));
                }
            }
            false
        };
        match order.side {
            Side::Buy => {
                for (price, level) in &self.asks {
                    if remaining == 0 || limit.is_some_and(|bound| *price > bound) {
                        break;
                    }
                    if visit(level, &mut remaining) {
                        return true;
                    }
                }
            }
            Side::Sell => {
                for (Reverse(price), level) in &self.bids {
                    if remaining == 0 || limit.is_some_and(|bound| *price < bound) {
                        break;
                    }
                    if visit(level, &mut remaining) {
                        return true;
                    }
                }
            }
        }
        let filled = order.remaining_qty.raw() - remaining;
        if order.is_iceberg() && filled > 0 {
            work += ((filled - 1) / order.display_qty.raw()) as u128;
        }
        work > budget as u128
    }

    pub fn executable_qty(&self, taker_side: Side, limit: Option<FixedPrice>) -> FixedQty {
        let mut total = FixedQty::ZERO;
        let mut add_level = |level: &PriceLevel| {
            let mut iter = crate::price_level::PriceLevelIter::new(level);
            while let Some(key) = iter.next_key(&self.order_slab) {
                if let Some(node) = self.order_slab.get(key) {
                    total = total.saturating_add(if node.order.is_iceberg() {
                        node.order.open_qty()
                    } else {
                        node.order.remaining_qty
                    });
                }
            }
        };
        match taker_side {
            Side::Buy => {
                for (price, level) in &self.asks {
                    if limit.is_some_and(|bound| *price > bound) {
                        break;
                    }
                    add_level(level);
                }
            }
            Side::Sell => {
                for (Reverse(price), level) in &self.bids {
                    if limit.is_some_and(|bound| *price < bound) {
                        break;
                    }
                    add_level(level);
                }
            }
        }
        total
    }

    /// Returns true if an incoming order from `session` on `taker_side` would
    /// match against a resting order belonging to the same session, considering
    /// only price levels it could actually cross. `limit_price` bounds the scan
    /// for limit orders; `None` (market order) scans the entire opposite side.
    ///
    /// Used for self-trade prevention. Only the crossing region is scanned, so
    /// this is cheap for the common non-self-trade case.
    pub fn would_self_trade(
        &self,
        taker_side: Side,
        limit_price: Option<FixedPrice>,
        session: &me_core::SessionId,
    ) -> bool {
        match taker_side {
            // Buy taker crosses asks at price <= limit.
            Side::Buy => {
                for (price, level) in &self.asks {
                    if let Some(limit) = limit_price {
                        if *price > limit {
                            break;
                        }
                    }
                    if self.level_has_session(level, session) {
                        return true;
                    }
                }
                false
            }
            // Sell taker crosses bids at price >= limit.
            Side::Sell => {
                for (Reverse(price), level) in &self.bids {
                    if let Some(limit) = limit_price {
                        if *price < limit {
                            break;
                        }
                    }
                    if self.level_has_session(level, session) {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn level_has_session(&self, level: &PriceLevel, session: &me_core::SessionId) -> bool {
        let mut iter = crate::price_level::PriceLevelIter::new(level);
        while let Some(key) = iter.next_key(&self.order_slab) {
            if let Some(node) = self.order_slab.get(key) {
                if node.order.client_session_id == *session {
                    return true;
                }
            }
        }
        false
    }

    fn update_best_after_insert(&mut self, side: Side, price: FixedPrice) {
        match side {
            Side::Buy => {
                if self.best_bid.is_none_or(|b| price > b) {
                    self.best_bid = Some(price);
                }
            }
            Side::Sell => {
                if self.best_ask.is_none_or(|a| price < a) {
                    self.best_ask = Some(price);
                }
            }
        }
    }

    pub fn refresh_best(&mut self, side: Side) {
        match side {
            Side::Buy => {
                self.best_bid = self.bids.keys().next().map(|Reverse(p)| *p);
            }
            Side::Sell => {
                self.best_ask = self.asks.keys().next().copied();
            }
        }
    }

    /// Refresh both best bid and best ask.
    pub fn refresh_best_both(&mut self) {
        self.refresh_best(Side::Buy);
        self.refresh_best(Side::Sell);
    }

    /// Get mutable access to a specific bid level by price.
    pub fn bids_level_mut(&mut self, price: FixedPrice) -> Option<&mut PriceLevel> {
        self.bids.get_mut(&Reverse(price))
    }

    /// Get mutable access to a specific ask level by price.
    pub fn asks_level_mut(&mut self, price: FixedPrice) -> Option<&mut PriceLevel> {
        self.asks.get_mut(&price)
    }

    /// Collect all resting orders (for snapshots).
    pub fn all_resting_orders(&self) -> Vec<Order> {
        self.order_slab.iter().map(|(_, node)| node.order).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::order::TimeInForce;

    fn make_config() -> SymbolConfig {
        SymbolConfig::default_btc_usdt()
    }

    fn make_limit(id: OrderId, side: Side, price: f64, qty: f64) -> Order {
        Order::new_limit(
            id,
            SymbolId(0),
            side,
            TimeInForce::GTC,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
    }

    #[test]
    fn test_insert_and_best_prices() {
        let mut book = OrderBook::new(make_config());

        book.insert_order(make_limit(1, Side::Buy, 100.0, 1.0));
        assert_eq!(book.best_bid(), Some(FixedPrice::from_f64(100.0)));
        assert_eq!(book.best_ask(), None);

        book.insert_order(make_limit(2, Side::Sell, 101.0, 1.0));
        assert_eq!(book.best_ask(), Some(FixedPrice::from_f64(101.0)));

        book.insert_order(make_limit(3, Side::Buy, 99.0, 1.0));
        assert_eq!(book.best_bid(), Some(FixedPrice::from_f64(100.0)));

        book.insert_order(make_limit(4, Side::Buy, 100.50, 1.0));
        assert_eq!(book.best_bid(), Some(FixedPrice::from_f64(100.50)));
    }

    #[test]
    fn test_remove_and_best_update() {
        let mut book = OrderBook::new(make_config());

        book.insert_order(make_limit(1, Side::Buy, 100.0, 1.0));
        book.insert_order(make_limit(2, Side::Buy, 99.0, 1.0));

        book.remove_order(1);
        assert_eq!(book.best_bid(), Some(FixedPrice::from_f64(99.0)));

        book.remove_order(2);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn test_order_count() {
        let mut book = OrderBook::new(make_config());
        assert_eq!(book.order_count(), 0);

        book.insert_order(make_limit(1, Side::Buy, 100.0, 1.0));
        book.insert_order(make_limit(2, Side::Sell, 101.0, 1.0));
        assert_eq!(book.order_count(), 2);

        book.remove_order(1);
        assert_eq!(book.order_count(), 1);
    }

    #[test]
    fn test_contains_order() {
        let mut book = OrderBook::new(make_config());
        book.insert_order(make_limit(1, Side::Buy, 100.0, 1.0));
        assert!(book.contains_order(1));
        assert!(!book.contains_order(2));
    }

    #[test]
    fn test_available_qty() {
        let mut book = OrderBook::new(make_config());
        book.insert_order(make_limit(1, Side::Sell, 100.0, 1.0));
        book.insert_order(make_limit(2, Side::Sell, 101.0, 2.0));
        book.insert_order(make_limit(3, Side::Sell, 102.0, 3.0));

        // All asks
        assert_eq!(book.available_ask_qty(None), FixedQty::from_f64(6.0));
        // Asks at price <= 101
        assert_eq!(
            book.available_ask_qty(Some(FixedPrice::from_f64(101.0))),
            FixedQty::from_f64(3.0)
        );
    }

    #[test]
    fn test_empty_book() {
        let book = OrderBook::new(make_config());
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
        assert!(book.asks_empty());
        assert!(book.bids_empty());
        assert_eq!(book.order_count(), 0);
    }

    #[test]
    fn test_multiple_orders_same_price() {
        let mut book = OrderBook::new(make_config());
        book.insert_order(make_limit(1, Side::Buy, 100.0, 1.0));
        book.insert_order(make_limit(2, Side::Buy, 100.0, 2.0));
        assert_eq!(book.bid_level_count(), 1);
        assert_eq!(book.order_count(), 2);
    }

    fn make_config_with_policy(policy: InvariantPolicy) -> SymbolConfig {
        let mut cfg = make_config();
        cfg.invariant_policy = policy;
        cfg
    }

    #[test]
    #[should_panic(expected = "BUG: matching invariant violated")]
    fn invariant_strict_policy_panics() {
        let book = OrderBook::new(make_config_with_policy(InvariantPolicy::Strict));
        book.record_invariant_violation(InvariantKind::SlabKeyFromIteratorMissing, "test_strict");
    }

    #[test]
    fn invariant_halt_symbol_policy_sets_flag_and_bumps_counter() {
        let book = OrderBook::new(make_config_with_policy(InvariantPolicy::HaltSymbol));
        assert!(!book.is_halted_by_invariant());
        assert_eq!(book.invariant_violations_total(), 0);

        book.record_invariant_violation(InvariantKind::OrderLocationMissing, "test_halt");

        assert!(book.is_halted_by_invariant());
        assert_eq!(book.invariant_violations_total(), 1);

        // Subsequent violations keep counting but stay halted.
        book.record_invariant_violation(InvariantKind::SlabKeyFromLocationMissing, "test_halt");
        assert!(book.is_halted_by_invariant());
        assert_eq!(book.invariant_violations_total(), 2);
    }

    #[test]
    fn invariant_continue_policy_counts_but_does_not_halt() {
        let book = OrderBook::new(make_config_with_policy(InvariantPolicy::Continue));
        book.record_invariant_violation(InvariantKind::SlabKeyFromIteratorMissing, "test_continue");
        assert!(!book.is_halted_by_invariant());
        assert_eq!(book.invariant_violations_total(), 1);
    }
}
