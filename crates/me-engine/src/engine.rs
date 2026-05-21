use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, OrderId};

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

                // Track GTD expiry before the order is moved into a book.
                if order.is_gtd() && order.expire_at != 0 {
                    self.gtd_expiry
                        .push(Reverse((order.expire_at, order.id)));
                }

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
                    matching::process_order(&mut self.book, order, &mut self.events);
                    self.check_stop_triggers();
                }
            }
            Command::CancelOrder {
                order_id,
                timestamp,
                ..
            } => {
                // Cancels ALWAYS allowed, even when halted
                if self.book.contains_order(order_id) {
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

        self.events.events()
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
                    matching::cancel_order(&mut self.book, id, now, &mut buf);
                }
            } else if self.stop_book.contains(id) {
                self.stop_book.cancel(id, now, &mut buf);
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
        for order in snapshot.resting_orders {
            if order.is_gtd() && order.expire_at != 0 {
                self.gtd_expiry
                    .push(Reverse((order.expire_at, order.id)));
            }
            self.book.insert_order(order);
        }
        let mut scratch = EventBuffer::default();
        for order in snapshot.stop_orders {
            if order.is_gtd() && order.expire_at != 0 {
                self.gtd_expiry
                    .push(Reverse((order.expire_at, order.id)));
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
