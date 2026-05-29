use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId};

use me_book::book::OrderBook;
use me_book::matching;
use me_book::stop_book::StopBook;
use me_clock::Clock;
use me_persist::snapshot::BookSnapshot;

use crate::command::Command;

/// Maximum number of cascading stop trigger iterations to prevent infinite loops.
const MAX_STOP_CASCADE: usize = 100;

/// Trading state for circuit breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradingState {
    /// Normal trading.
    Active,
    /// Trading halted — only cancels allowed.
    Halted,
}

/// Single-pair matching engine.
///
/// FUNDAMENTAL INVARIANT: all operations within one Engine are strictly sequential.
/// One Engine = one OrderBook = one thread. No locks, no shared mutable state.
pub struct Engine {
    book: OrderBook,
    stop_book: StopBook,
    events: EventBuffer,
    trading_state: TradingState,
    clock: Arc<dyn Clock>,
    /// When true, reject incoming orders that would match against a resting
    /// order from the same client session (self-trade prevention, cancel-newest).
    self_trade_prevention: bool,
    /// Min-heap of `(expire_at, order_id)` for GTD orders, soonest expiry first.
    /// Entries are lazy: a popped id is only cancelled if it is still resting
    /// with a matching expiry, so stale entries (filled/cancelled/reused) are
    /// harmlessly skipped.
    gtd_expiry: BinaryHeap<Reverse<(u64, OrderId)>>,
    /// Active iceberg orders, keyed by order id. The value is the template
    /// (qty = total, display_qty = visible slice size) with `cumulative_qty`
    /// tracking the total filled across all slices. When a visible slice fully
    /// fills and a hidden reserve remains, the next slice is re-inserted at the
    /// back of the queue.
    icebergs: FxHashMap<OrderId, Order>,
}

impl Engine {
    pub fn new(config: SymbolConfig) -> Self {
        Self::with_clock(config, Arc::new(me_clock::SystemClock))
    }

    /// Create an engine with a custom clock (for production use CoarseClock/RdtscClock).
    pub fn with_clock(config: SymbolConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            book: OrderBook::new(config),
            stop_book: StopBook::new(),
            events: EventBuffer::with_capacity(256),
            trading_state: TradingState::Active,
            clock,
            self_trade_prevention: false,
            gtd_expiry: BinaryHeap::new(),
            icebergs: FxHashMap::default(),
        }
    }

    /// Enable or disable self-trade prevention for this symbol.
    pub fn set_self_trade_prevention(&mut self, enabled: bool) {
        self.self_trade_prevention = enabled;
    }

    /// Process a single command and return the resulting events.
    ///
    /// This is the main entry point. Commands are processed STRICTLY SEQUENTIALLY.
    /// The caller must ensure that commands for one symbol are never processed concurrently.
    pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
        self.events.clear();

        match cmd {
            Command::PlaceOrder(order) => {
                // Circuit breaker: reject ALL new orders when halted (including stops).
                if self.trading_state == TradingState::Halted {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id: order.id,
                        reason: RejectReason::TradingHalted,
                        timestamp: order.timestamp,
                    });
                    return self.events.events();
                }

                // Snapshot the event count so that, after processing, we can
                // tell whether this order was rejected — the GTD expiry enqueue
                // is deferred until acceptance (see the end of this arm).
                let events_start = self.events.len();

                if order.is_stop() {
                    // Route stop orders to stop book
                    self.stop_book.insert(order, &mut self.events);
                } else {
                    // Self-trade prevention (cancel-newest): reject before
                    // matching if the order would cross into a resting order
                    // from the same session.
                    if self.self_trade_prevention {
                        let limit_price = match order.order_type {
                            me_core::order::OrderType::Market => None,
                            _ => Some(order.price),
                        };
                        if self.book.would_self_trade(
                            order.side,
                            limit_price,
                            &order.client_session_id,
                        ) {
                            self.events.push(MatchEvent::OrderRejected {
                                order_id: order.id,
                                reason: RejectReason::SelfTradePrevented,
                                timestamp: order.timestamp,
                            });
                            return self.events.events();
                        }
                    }

                    // Iceberg: only the display slice is visible on the book; the
                    // hidden reserve is replenished as slices fill. Applies to
                    // resting tifs only (GTC/GTD); IOC/FOK ignore display_qty.
                    if order.is_iceberg()
                        && matches!(order.tif, TimeInForce::GTC | TimeInForce::GTD)
                    {
                        let mark = self.events.len();
                        let mut slice = order;
                        slice.remaining_qty = order.display_qty; // visible slice
                        matching::process_order(&mut self.book, slice, &mut self.events);
                        // Register the iceberg ONLY if the slice was accepted —
                        // a validation rejection must not leave a tracked
                        // template that replenish would later rest on the book.
                        let rejected = self.events.events()[mark..].iter().any(|e| {
                            matches!(e, MatchEvent::OrderRejected { order_id, .. } if *order_id == order.id)
                        });
                        if !rejected {
                            self.icebergs.insert(order.id, order);
                        }
                    } else {
                        matching::process_order(&mut self.book, order, &mut self.events);
                    }
                    self.check_stop_triggers();
                }

                // Enqueue GTD expiry only after acceptance: a rejected
                // submission (bad lot/tick, duplicate, self-trade) must not
                // leave a stale heap entry, which would otherwise accumulate
                // unbounded under repeated bad input with a far-future expiry.
                if order.is_gtd() && order.expire_at != 0 {
                    let rejected = self.events.events()[events_start..].iter().any(|e| {
                        matches!(e, MatchEvent::OrderRejected { order_id, .. } if *order_id == order.id)
                    });
                    if !rejected {
                        self.gtd_expiry.push(Reverse((order.expire_at, order.id)));
                    }
                }
            }
            Command::CancelOrder {
                order_id,
                timestamp,
                ..
            } => {
                // Cancels ALWAYS allowed, even when halted
                if self.book.contains_order(order_id) {
                    // Drop any iceberg reserve so it is not replenished after cancel.
                    self.icebergs.remove(&order_id);
                    matching::cancel_order(&mut self.book, order_id, timestamp, &mut self.events);
                } else if self.stop_book.contains(order_id) {
                    self.stop_book.cancel(order_id, timestamp, &mut self.events);
                } else {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::OrderNotFound,
                        timestamp,
                    });
                }
            }
            Command::AmendOrder {
                order_id,
                new_price,
                new_qty,
                timestamp,
                ..
            } => {
                // Amends rejected when halted — conservative policy; cancels-only during halt.
                if self.trading_state == TradingState::Halted {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::TradingHalted,
                        timestamp,
                    });
                    return self.events.events();
                }

                // Amending an iceberg can't be applied consistently: amend only
                // touches the visible slice on the book, leaving the engine's
                // hidden template (qty/display/cumulative) stale, which would
                // corrupt later replenishment. Reject; the client cancels and
                // re-places to change an iceberg.
                if self.icebergs.contains_key(&order_id) {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::AmendNotSupported,
                        timestamp,
                    });
                    return self.events.events();
                }

                matching::amend_order(
                    &mut self.book,
                    order_id,
                    new_price,
                    new_qty,
                    timestamp,
                    &mut self.events,
                );
                self.check_stop_triggers();
            }
        }

        // Replenish any iceberg whose visible slice fully filled this command,
        // then settle the replenish<->stop interaction: a replenished slice can
        // trade at a price that triggers a stop, and a triggered stop's order can
        // in turn consume an iceberg slice that needs replenishing. The
        // per-command stop check ran before replenishment, so iterate here until
        // no new events are produced (bounded by MAX_STOP_CASCADE).
        // `folded` (events already folded into iceberg cumulatives) and `touched`
        // persist across the settle loop so per-slice fills are counted exactly
        // once; reconciliation that collapses per-slice events runs ONCE at the
        // end, after the interaction has settled.
        let mut folded = 0usize;
        let mut touched: rustc_hash::FxHashSet<OrderId> = rustc_hash::FxHashSet::default();
        self.replenish_iceberg_slices(&mut folded, &mut touched);
        let mut settle = 0;
        loop {
            let before = self.events.len();
            self.check_stop_triggers();
            self.replenish_iceberg_slices(&mut folded, &mut touched);
            if self.events.len() == before || settle >= MAX_STOP_CASCADE {
                break;
            }
            settle += 1;
        }
        self.reconcile_iceberg_events(&touched);

        self.events.events()
    }

    /// Fold this command's incremental fills into each tracked iceberg's
    /// `cumulative_qty`, then, for any iceberg whose visible slice has left the
    /// book with a hidden reserve remaining, build the next slice and route it
    /// through matching (`rematch_limit`) — NOT a bare insert — so a crossing
    /// slice executes against remaining opposite liquidity instead of resting
    /// into a crossed book. Repeats until every iceberg is resting or exhausted.
    ///
    /// `folded` (events already folded) and `touched` are caller-owned so this
    /// can run repeatedly — interleaved with stop-trigger checks — while counting
    /// each per-slice fill exactly once. The caller collapses the per-slice status
    /// events via `reconcile_iceberg_events` once the interaction has settled.
    fn replenish_iceberg_slices(
        &mut self,
        folded: &mut usize,
        touched: &mut rustc_hash::FxHashSet<OrderId>,
    ) {
        if self.icebergs.is_empty() {
            return;
        }

        loop {
            // Fold only events appended since the last pass (avoid double count).
            let mut fills: Vec<(OrderId, FixedQty)> = Vec::new();
            {
                let evs = self.events.events();
                for ev in &evs[*folded..] {
                    match ev {
                        MatchEvent::OrderFilled {
                            order_id,
                            filled_qty,
                            ..
                        }
                        | MatchEvent::OrderPartiallyFilled {
                            order_id,
                            filled_qty,
                            ..
                        } if self.icebergs.contains_key(order_id) => {
                            fills.push((*order_id, *filled_qty));
                        }
                        _ => {}
                    }
                }
                *folded = evs.len();
            }
            for (id, f) in fills {
                if let Some(t) = self.icebergs.get_mut(&id) {
                    t.cumulative_qty += f;
                }
                touched.insert(id);
            }
            // Drop fully-exhausted icebergs (final slice consumed).
            self.icebergs.retain(|_, t| t.cumulative_qty < t.qty);

            // Replenish slices whose visible portion left the book.
            let mut candidates: Vec<OrderId> = self
                .icebergs
                .iter()
                .filter(|(id, _)| !self.book.contains_order(**id))
                .map(|(id, _)| *id)
                .collect();
            if candidates.is_empty() {
                break;
            }
            // Canonical order (by id) so simultaneous replenishment assigns queue
            // priority deterministically across replicas and WAL replay, not by
            // HashMap iteration order.
            candidates.sort_unstable();
            for id in candidates {
                let template = match self.icebergs.get(&id) {
                    Some(t) => *t,
                    None => continue,
                };
                let hidden = template.qty.saturating_sub(template.cumulative_qty);
                if hidden.is_zero() {
                    self.icebergs.remove(&id);
                    continue;
                }
                let next = template.display_qty.min(hidden);
                let mut slice = template;
                slice.remaining_qty = next; // qty=total, cumulative=so-far preserved
                touched.insert(id);
                matching::rematch_limit(&mut self.book, &mut slice, &mut self.events);
            }
        }
    }

    /// Collapse per-slice `OrderFilled`/`OrderPartiallyFilled` events for each
    /// touched iceberg id into one summary event reflecting the final state.
    fn reconcile_iceberg_events(&mut self, ids: &rustc_hash::FxHashSet<OrderId>) {
        if ids.is_empty() {
            return;
        }
        // Snapshot the TRUE residual per iceberg first, to avoid borrowing the
        // iceberg map and the event vec simultaneously. The residual is
        // `qty - cumulative_qty` (visible slice + hidden reserve) — what the
        // order owner is still working, NOT the visible slice on the book. An id
        // no longer tracked has been fully exhausted (residual = None → Filled).
        let true_remaining: rustc_hash::FxHashMap<OrderId, Option<FixedQty>> = ids
            .iter()
            .map(|id| {
                let rem = self
                    .icebergs
                    .get(id)
                    .map(|t| t.qty.saturating_sub(t.cumulative_qty));
                (*id, rem)
            })
            .collect();

        let evs = self.events.vec_mut();
        // Aggregate (sum filled this command, last price, last timestamp) per id
        // while removing the per-slice status events.
        let mut agg: rustc_hash::FxHashMap<OrderId, (FixedQty, FixedPrice, u64)> =
            rustc_hash::FxHashMap::default();
        evs.retain(|e| match e {
            MatchEvent::OrderFilled {
                order_id,
                filled_qty,
                price,
                timestamp,
            }
            | MatchEvent::OrderPartiallyFilled {
                order_id,
                filled_qty,
                price,
                timestamp,
                ..
            } if ids.contains(order_id) => {
                let entry = agg
                    .entry(*order_id)
                    .or_insert((FixedQty::ZERO, *price, *timestamp));
                entry.0 += *filled_qty;
                entry.1 = *price;
                entry.2 = *timestamp;
                false
            }
            _ => true,
        });

        // Emit summaries in canonical id order so the output event sequence is
        // deterministic across replicas and WAL replay (HashMap iteration is not).
        let mut summaries: Vec<(OrderId, (FixedQty, FixedPrice, u64))> = agg.into_iter().collect();
        summaries.sort_unstable_by_key(|(id, _)| *id);
        for (id, (sum, price, ts)) in summaries {
            if sum.is_zero() {
                continue;
            }
            match true_remaining.get(&id).copied().flatten() {
                Some(remaining) => evs.push(MatchEvent::OrderPartiallyFilled {
                    order_id: id,
                    filled_qty: sum,
                    remaining_qty: remaining,
                    price,
                    timestamp: ts,
                }),
                None => evs.push(MatchEvent::OrderFilled {
                    order_id: id,
                    filled_qty: sum,
                    price,
                    timestamp: ts,
                }),
            }
        }
    }

    /// Check if any stop orders should be triggered based on recent trades.
    /// Handles cascading triggers (bounded).
    fn check_stop_triggers(&mut self) {
        if self.stop_book.is_empty() {
            return;
        }

        // Find the last trade price from our events
        let last_trade_price = self.find_last_trade_price();
        let last_trade_price = match last_trade_price {
            Some(p) => p,
            None => return,
        };

        let mut cascade_count = 0;
        let mut trigger_price = last_trade_price;

        loop {
            if cascade_count >= MAX_STOP_CASCADE {
                break;
            }

            let timestamp = self.clock.now_ns();

            let triggered =
                self.stop_book
                    .check_triggers(trigger_price, timestamp, &mut self.events);

            if triggered.is_empty() {
                break;
            }

            cascade_count += 1;

            // Process each triggered order
            for order in triggered {
                matching::process_order(&mut self.book, order, &mut self.events);
            }

            // Check if new trades were generated that could trigger more stops
            let new_price = self.find_last_trade_price();
            match new_price {
                Some(p) if p != trigger_price => {
                    trigger_price = p;
                }
                _ => break,
            }
        }
    }

    /// Find the last trade price from the current events buffer.
    fn find_last_trade_price(&self) -> Option<FixedPrice> {
        self.events.events().iter().rev().find_map(|e| match e {
            MatchEvent::Trade(t) => Some(t.price),
            _ => None,
        })
    }

    /// Cancel any GTD orders whose expiry has passed (lazy sweep).
    ///
    /// Returns the resulting events (typically `OrderCancelled`). These are NOT
    /// part of any single command's reply — the caller (router) audits and
    /// meters them and updates risk counters, but does not return them to the
    /// client that happened to trigger the sweep.
    pub fn expire_gtd_now(&mut self) -> Vec<MatchEvent> {
        let now = self.clock.now_ns();
        let mut buf = EventBuffer::default();
        while let Some(&Reverse((exp, id))) = self.gtd_expiry.peek() {
            if exp > now {
                break;
            }
            self.gtd_expiry.pop();
            // Live book: cancel only if still resting with the same expiry,
            // guarding against an id that was reused by a newer order.
            if let Some(order) = self.book.get_order(id).copied() {
                if order.is_gtd() && order.expire_at == exp {
                    // Drop iceberg reserve so an expired iceberg is not replenished.
                    self.icebergs.remove(&id);
                    matching::cancel_order(&mut self.book, id, now, &mut buf);
                }
            } else if let Some((is_gtd, exp_at)) =
                self.stop_book.get(id).map(|o| (o.is_gtd(), o.expire_at))
            {
                // Same guard as the live-book branch: only cancel a stop that is
                // still the GTD order this heap entry was created for. Otherwise a
                // stale entry would cancel a reused id (a non-GTD or differently-
                // dated stop).
                if is_gtd && exp_at == exp {
                    self.stop_book.cancel(id, now, &mut buf);
                }
            }
        }
        buf.drain()
    }

    /// Halt trading — only cancel orders will be processed.
    pub fn halt_trading(&mut self) {
        self.trading_state = TradingState::Halted;
    }

    /// Resume normal trading.
    pub fn resume_trading(&mut self) {
        self.trading_state = TradingState::Active;
    }

    /// Check if trading is active.
    pub fn is_trading_active(&self) -> bool {
        self.trading_state == TradingState::Active
    }

    /// Get current trading state.
    pub fn trading_state(&self) -> TradingState {
        self.trading_state
    }

    /// Access the order book (for queries/snapshots).
    pub fn book(&self) -> &OrderBook {
        &self.book
    }

    /// Mutable access to the order book (for recovery/restore).
    pub fn book_mut(&mut self) -> &mut OrderBook {
        &mut self.book
    }

    /// Access the stop book (for snapshots).
    pub fn stop_book(&self) -> &StopBook {
        &self.stop_book
    }

    /// Mutable access to the stop book (for recovery/restore).
    pub fn stop_book_mut(&mut self) -> &mut StopBook {
        &mut self.stop_book
    }

    /// Number of pending GTD expiry entries (for metrics/observability). May
    /// include stale entries for orders that have since filled or cancelled;
    /// those are skipped harmlessly by the sweep.
    pub fn gtd_pending(&self) -> usize {
        self.gtd_expiry.len()
    }

    /// Get the symbol config.
    pub fn config(&self) -> &SymbolConfig {
        &self.book.config
    }

    // ─── Snapshot / Restore ──────────────────────────────────────────────
    //
    // These are the single source of truth for snapshotting engine state.
    // Callers (recovery, tests, admin API) MUST use them rather than
    // assembling a `BookSnapshot` by hand — that's how fields like
    // `stop_orders` have been silently forgotten in the past.

    /// Capture the full durable state of this engine into a `BookSnapshot`.
    ///
    /// Snapshot captures (at call time):
    /// - symbol and sequence counter
    /// - trade-id generator
    /// - all resting orders on both sides of the order book
    /// - all pending stop orders (both stop-buy and stop-sell)
    ///
    /// Transient/recoverable state is NOT captured:
    /// - `trading_state` (halt/resume is an operator action, not durable)
    /// - `events` buffer (per-command scratch space)
    /// - invariant halt flag (re-derived from next violation)
    ///
    /// The `wal_sequence` watermark is set to 0 here; the shard worker (which
    /// owns the per-shard WAL writer) overwrites it with the live WAL append
    /// sequence before persisting, so recovery can replay only post-snapshot
    /// records.
    pub fn snapshot(&self) -> BookSnapshot {
        BookSnapshot {
            symbol: self.book.config.id,
            sequence: self.book.sequence,
            trade_id_gen: self.book.trade_id_gen,
            wal_sequence: 0,
            resting_orders: self.book.all_resting_orders(),
            stop_orders: self.stop_book.all_stop_orders(),
        }
    }

    /// Rebuild an engine from a previously captured `BookSnapshot`.
    ///
    /// The engine starts in `Active` trading state; callers that need to
    /// restore halt state must do so explicitly via `halt_trading()`.
    ///
    /// Resting orders are reinserted into the book WITHOUT re-running the
    /// matcher — we assume the snapshot was self-consistent (no crossed
    /// book) at the moment it was captured.
    pub fn restore_from_snapshot(config: SymbolConfig, snapshot: BookSnapshot) -> Self {
        debug_assert_eq!(
            config.id, snapshot.symbol,
            "restore_from_snapshot: config.id does not match snapshot.symbol"
        );
        let mut engine = Self::new(config);
        engine.apply_snapshot(snapshot);
        engine
    }

    /// Apply snapshot state to an existing engine, overwriting its book
    /// and stop book. Used during WAL replay where the engine is created
    /// first and then rewound to a snapshot.
    pub fn apply_snapshot(&mut self, snapshot: BookSnapshot) {
        // Reset both books to empty, then re-insert.
        let cfg = self.book.config.clone();
        self.book = OrderBook::new(cfg);
        self.stop_book = StopBook::new();

        self.gtd_expiry.clear();
        self.icebergs.clear();
        for order in snapshot.resting_orders {
            if order.is_gtd() && order.expire_at != 0 {
                self.gtd_expiry.push(Reverse((order.expire_at, order.id)));
            }
            // Rebuild iceberg tracking: a resting slice with display_qty>0 and a
            // hidden reserve remaining is an active iceberg. The restored order
            // carries qty/display/cumulative, so it is its own template.
            if order.display_qty.raw() > 0 && order.cumulative_qty < order.qty {
                self.icebergs.insert(order.id, order);
            }
            self.book.insert_order(order);
        }
        let mut scratch = EventBuffer::default();
        for order in snapshot.stop_orders {
            if order.is_gtd() && order.expire_at != 0 {
                self.gtd_expiry.push(Reverse((order.expire_at, order.id)));
            }
            self.stop_book.insert(order, &mut scratch);
        }
        // Discard the synthetic OrderAccepted events produced by
        // stop_book.insert — they belong to the original insertion, not
        // to the restore.
        scratch.clear();

        self.book.sequence = snapshot.sequence;
        self.book.trade_id_gen = snapshot.trade_id_gen;
    }
}
