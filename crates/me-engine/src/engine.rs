use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::symbol::SymbolConfig;
use me_core::types::FixedPrice;

use me_book::book::OrderBook;
use me_book::matching;
use me_book::stop_book::StopBook;

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
}

impl Engine {
    pub fn new(config: SymbolConfig) -> Self {
        Self {
            book: OrderBook::new(config),
            stop_book: StopBook::new(),
            events: EventBuffer::with_capacity(256),
            trading_state: TradingState::Active,
        }
    }

    /// Process a single command and return the resulting events.
    ///
    /// This is the main entry point. Commands are processed STRICTLY SEQUENTIALLY.
    /// The caller must ensure that commands for one symbol are never processed concurrently.
    pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
        self.events.clear();

        match cmd {
            Command::PlaceOrder(order) => {
                // Circuit breaker: reject new orders when halted
                if self.trading_state == TradingState::Halted {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id: order.id,
                        reason: RejectReason::TradingHalted,
                        timestamp: order.timestamp,
                    });
                    return self.events.events();
                }

                if order.is_stop() {
                    // Route stop orders to stop book
                    self.stop_book.insert(order, &mut self.events);
                } else {
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
                // Amends rejected when halted
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

            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;

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
}
