use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use me_core::events::{EventBuffer, MatchEvent, RejectReason};
use me_core::order::{
    Order, OrderStatus, TimeInForce, ORDER_FLAG_MAKER_OR_CANCEL, ORDER_FLAG_POST_ONLY,
};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side};

use me_book::book::OrderBook;
use me_book::matching;
use me_book::stop_book::StopBook;
use me_clock::Clock;
use me_persist::snapshot::BookSnapshot;

use crate::command::Command;

/// Maximum number of cascading stop trigger iterations to prevent infinite loops.
const MAX_STOP_CASCADE: usize = 100;
pub const MAX_PENDING_STOPS: usize = 4096;
pub const MAX_LIVE_GTD: usize = 4096;

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
    risk_config: Option<me_risk::config::RiskConfig>,
    processing_timestamp: u64,
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
            risk_config: None,
            processing_timestamp: 0,
        }
    }

    pub fn set_risk_config(&mut self, config: Option<me_risk::config::RiskConfig>) {
        self.risk_config = config;
    }

    pub fn amended_order(
        &self,
        order_id: OrderId,
        new_price: Option<FixedPrice>,
        new_qty: Option<FixedQty>,
    ) -> Result<Order, RejectReason> {
        if self.icebergs.contains_key(&order_id) || self.stop_book.contains(order_id) {
            return Err(RejectReason::AmendNotSupported);
        }
        let mut order = self
            .book
            .get_order(order_id)
            .copied()
            .ok_or(RejectReason::OrderNotFound)?;
        if self.risk_config.is_some()
            && !order.cumulative_qty.is_zero()
            && (new_price.is_some_and(|price| price > order.price)
                || new_qty.is_some_and(|qty| qty > order.qty))
        {
            return Err(RejectReason::AmendNotSupported);
        }
        if let Some(price) = new_price {
            if !self.book.config.validate_price(price) {
                return Err(RejectReason::InvalidTickSize);
            }
            order.price = price;
        }
        if let Some(qty) = new_qty {
            if !self.book.config.validate_qty(qty) {
                return Err(RejectReason::InvalidQuantity);
            }
            order.qty = qty;
        }
        order.remaining_qty =
            FixedQty::from_raw(order.qty.raw().saturating_sub(order.cumulative_qty.raw()));
        Ok(order)
    }

    fn execution_notional_blocked(&self, order: &Order) -> bool {
        let Some(config) = self.risk_config.as_ref() else {
            return false;
        };
        let Some(price) = self.book.execution_price_bound(order) else {
            return false;
        };
        let notional = (price.raw().unsigned_abs() as u128).saturating_mul(order.qty.raw() as u128);
        notional > (config.max_order_notional as u128) * (me_core::types::SCALE_U64 as u128)
    }

    /// Enable or disable self-trade prevention for this symbol.
    pub fn set_self_trade_prevention(&mut self, enabled: bool) {
        self.self_trade_prevention = enabled;
    }

    /// True when self-trade prevention is enabled and `order` would cross into a
    /// resting order from the same session. Shared by the normal place path and
    /// the stop-trigger path so both honor STP identically — a market order has
    /// no limit, so it is checked against any crossable price.
    fn self_trade_blocked(&self, order: &Order) -> bool {
        if !self.self_trade_prevention {
            return false;
        }
        let limit_price = match order.order_type {
            me_core::order::OrderType::Market => None,
            _ => Some(order.price),
        };
        self.book
            .would_self_trade(order.side, limit_price, &order.client_session_id)
    }

    /// True when `order` carries a maker-only flag (POST_ONLY / MAKER_OR_CANCEL)
    /// and would cross the spread — i.e. act as a taker. Mirrors the entry-time
    /// guard in `matching::process_order`, so an iceberg refill honors the same
    /// "never take" contract its first slice did.
    fn maker_only_would_cross(&self, order: &Order) -> bool {
        if order.flags & (ORDER_FLAG_POST_ONLY | ORDER_FLAG_MAKER_OR_CANCEL) == 0 {
            return false;
        }
        match order.side {
            Side::Buy => self.book.best_ask().is_some_and(|ask| ask <= order.price),
            Side::Sell => self.book.best_bid().is_some_and(|bid| bid >= order.price),
        }
    }

    /// Owning session of a resting order (live book first, then stop book).
    /// `None` if the order is not currently resting in either book.
    fn order_owner(&self, order_id: OrderId) -> Option<me_core::SessionId> {
        self.book
            .get_order(order_id)
            .or_else(|| self.stop_book.get(order_id))
            .map(|o| o.client_session_id)
    }

    /// Authorize a cancel/amend issued by `requesting_session` against
    /// `order_id`. Returns `true` when the mutation is permitted.
    ///
    /// Rules:
    /// - `requesting_session == NONE` is a privileged/internal caller (WAL
    ///   replay, admin, direct `process`): always allowed.
    /// - A resting order owned by `NONE` (legacy/unattributed) is manageable by
    ///   anyone — preserves operability without re-opening the cross-session
    ///   hole, since an attacker with a real session can never match a victim's
    ///   real session this way.
    /// - Otherwise the owning session must equal the requesting session.
    /// - A non-resting order returns `true` so the normal path emits the
    ///   ordinary `OrderNotFound` rejection.
    pub(crate) fn is_mutation_authorized(
        &self,
        order_id: OrderId,
        requesting_session: me_core::SessionId,
    ) -> bool {
        if requesting_session.is_none() {
            return true;
        }
        match self.order_owner(order_id) {
            Some(owner) if owner.is_none() => true,
            Some(owner) => owner == requesting_session,
            None => true,
        }
    }

    /// Process a single command and return the resulting events.
    ///
    /// This is the main entry point. Commands are processed STRICTLY SEQUENTIALLY.
    /// The caller must ensure that commands for one symbol are never processed concurrently.
    ///
    /// Equivalent to [`Engine::process_with_session`] with a privileged
    /// (`SessionId::NONE`) caller — i.e. no per-session ownership enforcement.
    pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
        self.process_with_session(cmd, me_core::SessionId::NONE)
    }

    /// Process a command on behalf of `requesting_session`, enforcing order
    /// ownership for cancel/amend. A client may only cancel or amend an order
    /// its own session placed; an attempt against another session's order is
    /// rejected with `OrderNotFound` (without revealing the order exists).
    pub fn process_with_session(
        &mut self,
        cmd: Command,
        requesting_session: me_core::SessionId,
    ) -> &[MatchEvent] {
        self.process_with_session_at(cmd, requesting_session, self.clock.now_ns())
    }

    pub fn process_with_session_at(
        &mut self,
        cmd: Command,
        requesting_session: me_core::SessionId,
        timestamp: u64,
    ) -> &[MatchEvent] {
        self.processing_timestamp = timestamp;
        if let Command::CancelOrder {
            order_id,
            timestamp,
            ..
        }
        | Command::AmendOrder {
            order_id,
            timestamp,
            ..
        } = &cmd
        {
            let (order_id, timestamp) = (*order_id, *timestamp);
            if !self.is_mutation_authorized(order_id, requesting_session) {
                self.events.clear();
                self.events.push(MatchEvent::OrderRejected {
                    order_id,
                    reason: RejectReason::OrderNotFound,
                    timestamp,
                });
                return self.events.events();
            }
        }
        self.process_inner(cmd)
    }

    fn process_inner(&mut self, cmd: Command) -> &[MatchEvent] {
        self.events.clear();
        let mut stop_scanned = 0;
        let gtd_candidate = match &cmd {
            Command::PlaceOrder(order) if order.is_gtd() => Some((order.id, order.expire_at)),
            _ => None,
        };

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

                if order.is_gtd() && self.gtd_expiry.len() >= MAX_LIVE_GTD {
                    let mut ids = rustc_hash::FxHashSet::default();
                    self.gtd_expiry.retain(|Reverse((expiry, id))| {
                        self.book
                            .get_order(*id)
                            .or_else(|| self.stop_book.get(*id))
                            .is_some_and(|live| {
                                live.is_gtd() && live.expire_at == *expiry && ids.insert(*id)
                            })
                    });
                    if self.gtd_expiry.len() >= MAX_LIVE_GTD {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::RiskCheckFailed,
                            timestamp: order.timestamp,
                        });
                        return self.events.events();
                    }
                }
                if order.is_stop() && self.stop_book.len() >= MAX_PENDING_STOPS {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id: order.id,
                        reason: RejectReason::RiskCheckFailed,
                        timestamp: order.timestamp,
                    });
                    return self.events.events();
                }

                if order.is_stop() {
                    // Stops bypass process_order, so validate them here (qty,
                    // stop price, GTD expiry, etc.) before they rest. Internal /
                    // Router paths build orders without the io decoder, so this is
                    // the only validation guard for stops.
                    if let Some(reason) = matching::validate_order(&order, &self.book.config) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason,
                            timestamp: order.timestamp,
                        });
                        return self.events.events();
                    }
                    // Same uniqueness invariant as process_order: an id already
                    // resting (live book) or pending (stop book) must be rejected,
                    // not inserted (which would orphan a stop-book vector entry).
                    if self.book.contains_order(order.id) || self.stop_book.contains(order.id) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::DuplicateOrderId,
                            timestamp: order.timestamp,
                        });
                        return self.events.events();
                    }
                    // Route stop orders to stop book
                    self.stop_book.insert(order, &mut self.events);
                } else {
                    // Reject a live order whose id is already pending in the stop
                    // book — process_order's duplicate check only sees the live
                    // book, so this is the symmetric half of the stop-side check
                    // above, keeping ids unique across both books.
                    if self.stop_book.contains(order.id) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::DuplicateOrderId,
                            timestamp: order.timestamp,
                        });
                        return self.events.events();
                    }
                    // Self-trade prevention (cancel-newest): reject before
                    // matching if the order would cross into a resting order
                    // from the same session.
                    if self.self_trade_blocked(&order) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::SelfTradePrevented,
                            timestamp: order.timestamp,
                        });
                        return self.events.events();
                    }

                    if self.execution_notional_blocked(&order)
                        || self
                            .book
                            .matching_work_exceeds(&order, matching::MAX_MATCHING_FILLS)
                    {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::RiskCheckFailed,
                            timestamp: order.timestamp,
                        });
                        return self.events.events();
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
                    self.check_stop_triggers(&mut stop_scanned);
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

                let replacement = match self.amended_order(order_id, new_price, new_qty) {
                    Ok(order) => order,
                    Err(reason) => {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id,
                            reason,
                            timestamp,
                        });
                        return self.events.events();
                    }
                };
                if !replacement.remaining_qty.is_zero() && self.self_trade_blocked(&replacement) {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::SelfTradePrevented,
                        timestamp,
                    });
                    return self.events.events();
                }

                if !replacement.remaining_qty.is_zero()
                    && self.execution_notional_blocked(&replacement)
                {
                    self.events.push(MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::RiskCheckFailed,
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
                self.check_stop_triggers(&mut stop_scanned);
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
            self.check_stop_triggers(&mut stop_scanned);
            self.replenish_iceberg_slices(&mut folded, &mut touched);
            if self.events.len() == before || settle >= MAX_STOP_CASCADE {
                break;
            }
            settle += 1;
        }
        self.reconcile_iceberg_events(&touched);
        // A pre-match guard (STP or a maker-only flag) may have blocked an
        // iceberg's replenishment, leaving it tracked but absent from the book.
        // Cancel such residuals so the order is never in an uncancellable /
        // unexpirable / resurrectable limbo.
        self.cancel_unrestable_icebergs();
        if let Some((id, expiry)) = gtd_candidate {
            let accepted = self.events.events().iter().any(|event| matches!(event, MatchEvent::OrderAccepted { order_id, .. } if *order_id == id));
            if accepted
                && expiry != 0
                && (self.book.contains_order(id) || self.stop_book.contains(id))
            {
                self.gtd_expiry.push(Reverse((expiry, id)));
            }
        }

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

        // Icebergs whose next slice cannot be rematched this command because it
        // would violate a pre-match guard the order carries — self-trade
        // prevention (would cross SAME-session liquidity) or a maker-only flag
        // (would cross at all). The initial placement check cannot see liquidity
        // that appears after the iceberg rests (e.g. a same-session stop the
        // iceberg's own fill triggered, or a taker's resting remainder). Such an
        // iceberg is skipped this pass and cancelled at end-of-command. Tracked
        // locally so the candidate loop below cannot spin on an order it will
        // never rematch this pass.
        let mut blocked: rustc_hash::FxHashSet<OrderId> = rustc_hash::FxHashSet::default();

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

            // Replenish slices whose visible portion left the book, in submission
            // order (template timestamp, id tiebreak) — NOT client-supplied id,
            // which need not be time-ordered. This gives the earlier-submitted
            // iceberg queue priority and is deterministic across replicas / WAL
            // replay, independent of HashMap iteration order.
            let mut candidates: Vec<(u64, OrderId)> = self
                .icebergs
                .iter()
                .filter(|(id, _)| !self.book.contains_order(**id) && !blocked.contains(*id))
                .map(|(id, t)| (t.timestamp, *id))
                .collect();
            if candidates.is_empty() {
                break;
            }
            candidates.sort_unstable();
            for (_, id) in candidates {
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
                                            // A refilled slice always carries prior fills (cumulative > 0), so it
                                            // rests as PartiallyFilled, not the template's stale New — clients read
                                            // status from the book. If the slice crosses, matching overwrites this
                                            // with the matched status.
                slice.status = OrderStatus::PartiallyFilled;
                // A refill must honor the same pre-match guards process_order
                // applies on entry — the placement check could not see liquidity
                // that rested after the iceberg did. Skip (→ cancelled at
                // end-of-command) rather than rematch if the slice would:
                //   - self-cross same-session liquidity (self-trade prevention), or
                //   - cross at all while carrying a maker-only flag (POST_ONLY /
                //     MAKER_OR_CANCEL), which forbids taking.
                if self.self_trade_blocked(&slice)
                    || self.maker_only_would_cross(&slice)
                    || self.execution_notional_blocked(&slice)
                    || self
                        .book
                        .matching_work_exceeds(&slice, matching::remaining_work(&self.events))
                {
                    blocked.insert(id);
                    continue;
                }
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

    /// Cancel any iceberg that ends the command still tracked but absent from
    /// the live book — i.e. a pre-match guard blocked its replenishment.
    ///
    /// Normally a tracked iceberg always has a visible slice resting (or has
    /// been removed when exhausted); the only way it can be tracked-but-unrested
    /// is a refill blocked by self-trade prevention or a maker-only flag
    /// (POST_ONLY / MAKER_OR_CANCEL). Leaving it in that shadow-only state would
    /// make it uncancellable (CancelOrder checks only the live/stop books →
    /// returns OrderNotFound), unexpirable (GTD sweep also checks only those
    /// books), and resurrectable (a later command could replenish it).
    /// Cancelling the residual frees the reserve, notifies the client
    /// (OrderCancelled), and restores the invariant that every tracked iceberg
    /// rests in the book.
    ///
    /// Runs AFTER reconcile so the fill summary (PartiallyFilled with the
    /// residual) is emitted while the id is still tracked; this then appends the
    /// terminal OrderCancelled for that residual.
    fn cancel_unrestable_icebergs(&mut self) {
        if self.icebergs.is_empty() {
            return;
        }
        let blocked: Vec<(OrderId, FixedQty)> = self
            .icebergs
            .iter()
            .filter(|(id, _)| !self.book.contains_order(**id))
            .map(|(id, t)| (*id, t.qty.saturating_sub(t.cumulative_qty)))
            .collect();
        if blocked.is_empty() {
            return;
        }
        let now = self.processing_timestamp;
        for (id, remaining) in blocked {
            self.icebergs.remove(&id);
            self.events.push(MatchEvent::OrderCancelled {
                order_id: id,
                remaining_qty: remaining,
                timestamp: now,
            });
        }
    }

    /// Check if any stop orders should be triggered based on recent trades.
    /// Handles cascading triggers (bounded).
    ///
    /// Feeds the FULL trade-price path of this command (in execution order)
    /// into the stop book, not just the final print. A single aggressor can
    /// sweep several price levels; trailing stops must ratchet on the
    /// intra-command extreme and fixed stops must see every threshold the path
    /// crossed. Using only the last price loses both — e.g. a sell sweep trading
    /// 105 then 95 would leave a sell trailing stop unratcheted and never fire.
    fn check_stop_triggers(&mut self, scanned: &mut usize) {
        if self.stop_book.is_empty() {
            return;
        }

        // `scanned` marks how far into the event buffer we have consumed trades.
        // Keep the cursor across settle passes: replaying an older price after
        // a trailing stop ratchets can falsely trigger it.
        let mut cascade_count = 0;

        loop {
            if cascade_count >= MAX_STOP_CASCADE {
                break;
            }

            let path: Vec<FixedPrice> = self.events.events()[*scanned..]
                .iter()
                .filter_map(|e| match e {
                    MatchEvent::Trade(t) => Some(t.price),
                    _ => None,
                })
                .collect();
            *scanned = self.events.len();

            if path.is_empty() {
                break;
            }

            let timestamp = self.processing_timestamp;
            let mut triggered_any = false;

            for price in path {
                let triggered = self
                    .stop_book
                    .check_triggers(price, timestamp, &mut self.events);
                if triggered.is_empty() {
                    continue;
                }
                triggered_any = true;

                // Process each triggered order
                for order in triggered {
                    // A triggered stop is the newest aggressor — honor STP just
                    // like the normal place path, cancelling it rather than
                    // self-matching a resting order from the same session.
                    if self.risk_config.as_ref().is_some_and(|config| {
                        me_risk::engine::RiskEngine::check_limits(config, &order).is_err()
                    }) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::RiskCheckFailed,
                            timestamp,
                        });
                        continue;
                    }
                    if self.execution_notional_blocked(&order) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::RiskCheckFailed,
                            timestamp,
                        });
                        continue;
                    }
                    if self.self_trade_blocked(&order) {
                        self.events.push(MatchEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::SelfTradePrevented,
                            timestamp,
                        });
                        continue;
                    }
                    matching::process_order(&mut self.book, order, &mut self.events);
                }
            }

            if !triggered_any {
                break;
            }
            cascade_count += 1;
        }
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
        self.try_apply_snapshot(snapshot)
            .expect("invalid engine snapshot");
    }

    pub fn try_apply_snapshot(&mut self, mut snapshot: BookSnapshot) -> Result<(), String> {
        if snapshot.stop_orders.len() > MAX_PENDING_STOPS
            || snapshot
                .resting_orders
                .iter()
                .chain(&snapshot.stop_orders)
                .filter(|order| order.is_gtd())
                .count()
                > MAX_LIVE_GTD
        {
            return Err("snapshot exceeds deferred order capacity".into());
        }

        if snapshot.sequence == u64::MAX || snapshot.trade_id_gen == u64::MAX {
            return Err("snapshot counters exhausted".into());
        }
        if snapshot.symbol != self.book.config.id {
            return Err("snapshot symbol mismatch".into());
        }
        let mut verified = OrderBook::new(self.book.config.clone());
        let mut ids = rustc_hash::FxHashSet::default();
        let mut resting_sequences = rustc_hash::FxHashSet::default();
        for (orders, stops) in [
            (&snapshot.resting_orders, false),
            (&snapshot.stop_orders, true),
        ] {
            for order in orders {
                if order.symbol != snapshot.symbol
                    || order.is_stop() != stops
                    || !ids.insert(order.id)
                {
                    return Err("snapshot order symbol, type, or duplicate id invalid".into());
                }
                if (!stops
                    && (order.order_type != me_core::order::OrderType::Limit
                        || !matches!(order.tif, TimeInForce::GTC | TimeInForce::GTD)))
                    || matching::validate_order(order, &self.book.config).is_some()
                    || order
                        .remaining_qty
                        .raw()
                        .checked_add(order.cumulative_qty.raw())
                        .is_none_or(|sum| sum > order.qty.raw())
                {
                    return Err("snapshot order values invalid".into());
                }
                if !stops
                    && (order.sequence == 0
                        || order.sequence > snapshot.sequence
                        || !resting_sequences.insert(order.sequence))
                {
                    return Err("snapshot resting order sequence invalid".into());
                }
                if !stops && !verified.insert_order(*order) {
                    return Err("snapshot price level capacity exceeded".into());
                }
            }
        }
        if verified
            .best_bid()
            .zip(verified.best_ask())
            .is_some_and(|(bid, ask)| bid >= ask)
        {
            return Err("snapshot book is crossed".into());
        }
        snapshot
            .resting_orders
            .sort_unstable_by_key(|order| order.sequence);
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
            if let Some(location) = self.book.get_order_location(order.id).copied() {
                if let Some(restored) = self.book.get_order_mut_by_key(location.slab_key) {
                    restored.sequence = order.sequence;
                }
            }
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
        Ok(())
    }
}
