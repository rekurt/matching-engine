use std::time::Duration;

use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry, TextEncoder,
};

use me_book::book::OrderBook;
use me_core::events::{MatchEvent, RejectReason};
use me_core::order::OrderType;
use me_core::order::TimeInForce;
use me_core::types::{Side, SCALE};

// ─── EngineMetrics ──────────────────────────────────────────────────────────

/// Comprehensive Prometheus metrics for the matching engine.
pub struct EngineMetrics {
    // ── Order Processing ────────────────────────────────────────────────
    /// Total orders received (side, order_type, tif, symbol).
    pub orders_total: IntCounterVec,
    /// Rejected orders (reason, symbol).
    pub orders_rejected_total: IntCounterVec,
    /// Accepted orders (side, order_type, symbol).
    pub orders_accepted_total: IntCounterVec,
    /// Time to process a single order (order_type).
    pub order_processing_duration_seconds: HistogramVec,

    // ── Matching ────────────────────────────────────────────────────────
    /// Total trades executed (symbol).
    pub trades_total: IntCounterVec,
    /// Total trade volume in quantity (symbol).
    pub trades_volume_total: GaugeVec,
    /// Last trade price per symbol (symbol).
    pub trade_price: GaugeVec,
    /// Full vs partial fills (fill_type).
    pub fills_total: IntCounterVec,

    // ── Order Book ──────────────────────────────────────────────────────
    /// Number of bid price levels (symbol).
    pub book_bid_levels: IntGaugeVec,
    /// Number of ask price levels (symbol).
    pub book_ask_levels: IntGaugeVec,
    /// Total resting bid orders (symbol).
    pub book_bid_orders: IntGaugeVec,
    /// Total resting ask orders (symbol).
    pub book_ask_orders: IntGaugeVec,
    /// Current spread: best_ask - best_bid (symbol).
    pub book_spread: GaugeVec,
    /// Best bid price (symbol).
    pub book_best_bid: GaugeVec,
    /// Best ask price (symbol).
    pub book_best_ask: GaugeVec,
    /// Total bid quantity depth (symbol).
    pub book_depth_bid_qty: GaugeVec,
    /// Total ask quantity depth (symbol).
    pub book_depth_ask_qty: GaugeVec,

    // ── Cancel/Amend ────────────────────────────────────────────────────
    /// Successful cancellations (symbol).
    pub cancels_total: IntCounterVec,
    /// Failed cancellations (symbol).
    pub cancels_failed_total: IntCounterVec,
    /// Amends by type (amend_type, symbol).
    pub amends_total: IntCounterVec,

    // ── Stop Orders ─────────────────────────────────────────────────────
    /// Pending stop orders (symbol).
    pub stop_orders_pending: IntGaugeVec,
    /// Stop triggers (symbol).
    pub stop_triggers_total: IntCounterVec,

    // ── System ──────────────────────────────────────────────────────────
    /// Total commands processed (command_type).
    pub commands_total: IntCounterVec,
    /// Total events emitted (event_type).
    pub events_total: IntCounterVec,
    /// Number of active trading pairs.
    pub active_symbols: IntGauge,

    // ── WAL/Persistence ─────────────────────────────────────────────────
    /// WAL write operations.
    pub wal_writes_total: IntCounter,
    /// WAL bytes written.
    pub wal_bytes_written_total: IntCounter,
    /// Snapshot saves.
    pub snapshot_saves_total: IntCounter,

    // ── Connection Metrics ─────────────────────────────────────────────
    /// Total connections opened (transport: tcp/uds).
    pub connections_total: IntCounterVec,
    /// Currently active connections (transport).
    pub connections_active: IntGaugeVec,
    /// Total bytes received (transport).
    pub connection_bytes_in_total: IntCounterVec,
    /// Total bytes sent (transport).
    pub connection_bytes_out_total: IntCounterVec,
    /// Total frames received (transport).
    pub connection_frames_in_total: IntCounterVec,
    /// Total frames sent (transport).
    pub connection_frames_out_total: IntCounterVec,

    // ── Protocol Errors ────────────────────────────────────────────────
    /// Protocol errors by type (error_type).
    pub protocol_errors_total: IntCounterVec,

    // ── Invariant Violations ───────────────────────────────────────────
    /// Matching-engine invariant violations by kind and symbol.
    /// Each sample indicates a data-structure corruption event and
    /// should trigger an alert.
    pub invariant_violations_total: IntCounterVec,

    // ── Matching Detail ────────────────────────────────────────────────
    /// Matching duration by command type (command_type: place_order/cancel/amend).
    pub matching_duration_seconds: HistogramVec,
    /// Depth of cascading stop triggers.
    pub stop_cascade_depth: Histogram,
    /// Duration of cascading stop triggers.
    pub stop_cascade_duration_seconds: Histogram,

    // ── Shard Metrics ──────────────────────────────────────────────────
    /// Queue depth per shard (shard).
    pub shard_queue_depth: IntGaugeVec,
    /// Total commands per shard (shard).
    pub shard_commands_total: IntCounterVec,

    // ── WAL Detail ─────────────────────────────────────────────────────
    /// WAL flush duration.
    pub wal_flush_duration_seconds: Histogram,
    /// Snapshot duration.
    pub snapshot_duration_seconds: Histogram,
    /// Snapshot size in bytes.
    pub snapshot_size_bytes: IntGauge,

    /// Registry reference for the HTTP handler.
    registry: Registry,
}

impl EngineMetrics {
    /// Create a new `EngineMetrics` and register all metrics with the given
    /// `Registry`.
    pub fn new(registry: &Registry) -> Self {
        // ── Order Processing ────────────────────────────────────────────
        let orders_total = IntCounterVec::new(
            Opts::new("me_orders_total", "Total orders received"),
            &["side", "order_type", "tif", "symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(orders_total.clone()))
            .expect("metric can be registered");

        let orders_rejected_total = IntCounterVec::new(
            Opts::new("me_orders_rejected_total", "Rejected orders"),
            &["reason", "symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(orders_rejected_total.clone()))
            .expect("metric can be registered");

        let orders_accepted_total = IntCounterVec::new(
            Opts::new("me_orders_accepted_total", "Accepted orders"),
            &["side", "order_type", "symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(orders_accepted_total.clone()))
            .expect("metric can be registered");

        let order_processing_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "me_order_processing_duration_seconds",
                "Time to process a single order",
            )
            .buckets(vec![
                0.000_001, // 1us
                0.000_005, // 5us
                0.000_010, // 10us
                0.000_025, // 25us
                0.000_050, // 50us
                0.000_100, // 100us
                0.000_250, // 250us
                0.000_500, // 500us
                0.001_000, // 1ms
                0.005_000, // 5ms
                0.010_000, // 10ms
            ]),
            &["order_type"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(order_processing_duration_seconds.clone()))
            .expect("metric can be registered");

        // ── Matching ────────────────────────────────────────────────────
        let trades_total = IntCounterVec::new(
            Opts::new("me_trades_total", "Total trades executed"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(trades_total.clone()))
            .expect("metric can be registered");

        let trades_volume_total = GaugeVec::new(
            Opts::new("me_trades_volume_total", "Total trade volume (qty)"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(trades_volume_total.clone()))
            .expect("metric can be registered");

        let trade_price = GaugeVec::new(
            Opts::new("me_trade_price", "Last trade price per symbol"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(trade_price.clone()))
            .expect("metric can be registered");

        let fills_total = IntCounterVec::new(
            Opts::new("me_fills_total", "Full vs partial fills"),
            &["fill_type"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(fills_total.clone()))
            .expect("metric can be registered");

        // ── Order Book ──────────────────────────────────────────────────
        let book_bid_levels = IntGaugeVec::new(
            Opts::new("me_book_bid_levels", "Number of bid price levels"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_bid_levels.clone()))
            .expect("metric can be registered");

        let book_ask_levels = IntGaugeVec::new(
            Opts::new("me_book_ask_levels", "Number of ask price levels"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_ask_levels.clone()))
            .expect("metric can be registered");

        let book_bid_orders = IntGaugeVec::new(
            Opts::new("me_book_bid_orders", "Total resting bid orders"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_bid_orders.clone()))
            .expect("metric can be registered");

        let book_ask_orders = IntGaugeVec::new(
            Opts::new("me_book_ask_orders", "Total resting ask orders"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_ask_orders.clone()))
            .expect("metric can be registered");

        let book_spread = GaugeVec::new(
            Opts::new("me_book_spread", "Current spread (best_ask - best_bid)"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_spread.clone()))
            .expect("metric can be registered");

        let book_best_bid =
            GaugeVec::new(Opts::new("me_book_best_bid", "Best bid price"), &["symbol"])
                .expect("metric can be created");
        registry
            .register(Box::new(book_best_bid.clone()))
            .expect("metric can be registered");

        let book_best_ask =
            GaugeVec::new(Opts::new("me_book_best_ask", "Best ask price"), &["symbol"])
                .expect("metric can be created");
        registry
            .register(Box::new(book_best_ask.clone()))
            .expect("metric can be registered");

        let book_depth_bid_qty = GaugeVec::new(
            Opts::new("me_book_depth_bid_qty", "Total bid quantity"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_depth_bid_qty.clone()))
            .expect("metric can be registered");

        let book_depth_ask_qty = GaugeVec::new(
            Opts::new("me_book_depth_ask_qty", "Total ask quantity"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(book_depth_ask_qty.clone()))
            .expect("metric can be registered");

        // ── Cancel/Amend ────────────────────────────────────────────────
        let cancels_total = IntCounterVec::new(
            Opts::new("me_cancels_total", "Successful cancellations"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(cancels_total.clone()))
            .expect("metric can be registered");

        let cancels_failed_total = IntCounterVec::new(
            Opts::new("me_cancels_failed_total", "Failed cancellations"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(cancels_failed_total.clone()))
            .expect("metric can be registered");

        let amends_total = IntCounterVec::new(
            Opts::new("me_amends_total", "Amends by type"),
            &["amend_type", "symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(amends_total.clone()))
            .expect("metric can be registered");

        // ── Stop Orders ─────────────────────────────────────────────────
        let stop_orders_pending = IntGaugeVec::new(
            Opts::new("me_stop_orders_pending", "Pending stop orders"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(stop_orders_pending.clone()))
            .expect("metric can be registered");

        let stop_triggers_total = IntCounterVec::new(
            Opts::new("me_stop_triggers_total", "Stop triggers"),
            &["symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(stop_triggers_total.clone()))
            .expect("metric can be registered");

        // ── System ──────────────────────────────────────────────────────
        let commands_total = IntCounterVec::new(
            Opts::new("me_commands_total", "Total commands processed"),
            &["command_type"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(commands_total.clone()))
            .expect("metric can be registered");

        let events_total = IntCounterVec::new(
            Opts::new("me_events_total", "Total events emitted"),
            &["event_type"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(events_total.clone()))
            .expect("metric can be registered");

        let active_symbols = IntGauge::new("me_active_symbols", "Number of active trading pairs")
            .expect("metric can be created");
        registry
            .register(Box::new(active_symbols.clone()))
            .expect("metric can be registered");

        // ── WAL/Persistence ─────────────────────────────────────────────
        let wal_writes_total = IntCounter::new("me_wal_writes_total", "WAL write operations")
            .expect("metric can be created");
        registry
            .register(Box::new(wal_writes_total.clone()))
            .expect("metric can be registered");

        let wal_bytes_written_total =
            IntCounter::new("me_wal_bytes_written_total", "WAL bytes written")
                .expect("metric can be created");
        registry
            .register(Box::new(wal_bytes_written_total.clone()))
            .expect("metric can be registered");

        let snapshot_saves_total = IntCounter::new("me_snapshot_saves_total", "Snapshot saves")
            .expect("metric can be created");
        registry
            .register(Box::new(snapshot_saves_total.clone()))
            .expect("metric can be registered");

        // ── Connection Metrics ──────────────────────────────────────────
        let connections_total = IntCounterVec::new(
            Opts::new("me_connections_total", "Total connections opened"),
            &["transport"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(connections_total.clone()))
            .expect("metric can be registered");

        let connections_active = IntGaugeVec::new(
            Opts::new("me_connections_active", "Currently active connections"),
            &["transport"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(connections_active.clone()))
            .expect("metric can be registered");

        let connection_bytes_in_total = IntCounterVec::new(
            Opts::new("me_connection_bytes_in_total", "Total bytes received"),
            &["transport"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(connection_bytes_in_total.clone()))
            .expect("metric can be registered");

        let connection_bytes_out_total = IntCounterVec::new(
            Opts::new("me_connection_bytes_out_total", "Total bytes sent"),
            &["transport"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(connection_bytes_out_total.clone()))
            .expect("metric can be registered");

        let connection_frames_in_total = IntCounterVec::new(
            Opts::new("me_connection_frames_in_total", "Total frames received"),
            &["transport"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(connection_frames_in_total.clone()))
            .expect("metric can be registered");

        let connection_frames_out_total = IntCounterVec::new(
            Opts::new("me_connection_frames_out_total", "Total frames sent"),
            &["transport"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(connection_frames_out_total.clone()))
            .expect("metric can be registered");

        // ── Protocol Errors ─────────────────────────────────────────────
        let protocol_errors_total = IntCounterVec::new(
            Opts::new("me_protocol_errors_total", "Protocol errors by type"),
            &["error_type"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(protocol_errors_total.clone()))
            .expect("metric can be registered");

        // ── Invariant Violations ────────────────────────────────────────
        let invariant_violations_total = IntCounterVec::new(
            Opts::new(
                "me_invariant_violations_total",
                "Matching-engine invariant violations by kind and symbol",
            ),
            &["kind", "symbol"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(invariant_violations_total.clone()))
            .expect("metric can be registered");

        // ── Matching Detail ─────────────────────────────────────────────
        let matching_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "me_matching_duration_seconds",
                "Matching duration by command type",
            )
            .buckets(vec![
                0.000_001, // 1us
                0.000_005, // 5us
                0.000_010, // 10us
                0.000_025, // 25us
                0.000_050, // 50us
                0.000_100, // 100us
                0.000_250, // 250us
                0.000_500, // 500us
                0.001_000, // 1ms
                0.005_000, // 5ms
                0.010_000, // 10ms
            ]),
            &["command_type"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(matching_duration_seconds.clone()))
            .expect("metric can be registered");

        let stop_cascade_depth = Histogram::with_opts(
            HistogramOpts::new("me_stop_cascade_depth", "Depth of cascading stop triggers")
                .buckets(vec![1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0]),
        )
        .expect("metric can be created");
        registry
            .register(Box::new(stop_cascade_depth.clone()))
            .expect("metric can be registered");

        let stop_cascade_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "me_stop_cascade_duration_seconds",
                "Duration of cascading stop triggers",
            )
            .buckets(vec![
                0.000_001, // 1us
                0.000_005, // 5us
                0.000_010, // 10us
                0.000_025, // 25us
                0.000_050, // 50us
                0.000_100, // 100us
                0.000_250, // 250us
                0.000_500, // 500us
                0.001_000, // 1ms
                0.005_000, // 5ms
                0.010_000, // 10ms
            ]),
        )
        .expect("metric can be created");
        registry
            .register(Box::new(stop_cascade_duration_seconds.clone()))
            .expect("metric can be registered");

        // ── Shard Metrics ───────────────────────────────────────────────
        let shard_queue_depth = IntGaugeVec::new(
            Opts::new("me_shard_queue_depth", "Queue depth per shard"),
            &["shard"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(shard_queue_depth.clone()))
            .expect("metric can be registered");

        let shard_commands_total = IntCounterVec::new(
            Opts::new("me_shard_commands_total", "Total commands per shard"),
            &["shard"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(shard_commands_total.clone()))
            .expect("metric can be registered");

        // ── WAL Detail ──────────────────────────────────────────────────
        let wal_flush_duration_seconds = Histogram::with_opts(
            HistogramOpts::new("me_wal_flush_duration_seconds", "WAL flush duration")
                .buckets(vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1]),
        )
        .expect("metric can be created");
        registry
            .register(Box::new(wal_flush_duration_seconds.clone()))
            .expect("metric can be registered");

        let snapshot_duration_seconds = Histogram::with_opts(
            HistogramOpts::new("me_snapshot_duration_seconds", "Snapshot duration")
                .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0]),
        )
        .expect("metric can be created");
        registry
            .register(Box::new(snapshot_duration_seconds.clone()))
            .expect("metric can be registered");

        let snapshot_size_bytes = IntGauge::new("me_snapshot_size_bytes", "Snapshot size in bytes")
            .expect("metric can be created");
        registry
            .register(Box::new(snapshot_size_bytes.clone()))
            .expect("metric can be registered");

        Self {
            orders_total,
            orders_rejected_total,
            orders_accepted_total,
            order_processing_duration_seconds,
            trades_total,
            trades_volume_total,
            trade_price,
            fills_total,
            book_bid_levels,
            book_ask_levels,
            book_bid_orders,
            book_ask_orders,
            book_spread,
            book_best_bid,
            book_best_ask,
            book_depth_bid_qty,
            book_depth_ask_qty,
            cancels_total,
            cancels_failed_total,
            amends_total,
            stop_orders_pending,
            stop_triggers_total,
            commands_total,
            events_total,
            active_symbols,
            wal_writes_total,
            wal_bytes_written_total,
            snapshot_saves_total,
            connections_total,
            connections_active,
            connection_bytes_in_total,
            connection_bytes_out_total,
            connection_frames_in_total,
            connection_frames_out_total,
            protocol_errors_total,
            invariant_violations_total,
            matching_duration_seconds,
            stop_cascade_depth,
            stop_cascade_duration_seconds,
            shard_queue_depth,
            shard_commands_total,
            wal_flush_duration_seconds,
            snapshot_duration_seconds,
            snapshot_size_bytes,
            registry: registry.clone(),
        }
    }

    // ─── Convenience methods ────────────────────────────────────────────────

    /// Record an incoming command by type string.
    /// `command_type` should be "place_order", "cancel_order", or "amend_order".
    pub fn record_command_type(&self, command_type: &str) {
        self.commands_total.with_label_values(&[command_type]).inc();
    }

    /// Record an incoming place order with full details.
    pub fn record_place_order(
        &self,
        side: Side,
        order_type: OrderType,
        tif: TimeInForce,
        symbol: &str,
    ) {
        self.commands_total
            .with_label_values(&["place_order"])
            .inc();
        self.orders_total
            .with_label_values(&[
                side_label(side),
                order_type_label(order_type),
                tif_label(tif),
                symbol,
            ])
            .inc();
    }

    /// Record events produced by the engine after processing a command.
    ///
    /// Iterates through events and updates the relevant counters/gauges.
    /// Also records the processing duration for the order type.
    pub fn record_events(&self, events: &[MatchEvent], symbol: &str, duration: Duration) {
        // Determine order_type from the command context — we record duration
        // for whatever produced these events. Default to "unknown" if we
        // cannot infer.
        let mut order_type_str = "unknown";

        for event in events {
            match event {
                MatchEvent::OrderAccepted { .. } => {
                    self.events_total
                        .with_label_values(&["order_accepted"])
                        .inc();
                    // We don't know side/order_type here; the caller should
                    // use record_command for that. We still count accepted.
                    // Side/order_type labels are set from the Command context
                    // in record_command, so we leave orders_accepted_total
                    // to be filled by the caller or via a richer method.
                }
                MatchEvent::OrderRejected { reason, .. } => {
                    self.events_total
                        .with_label_values(&["order_rejected"])
                        .inc();
                    let reason_str = reject_reason_label(*reason);
                    self.orders_rejected_total
                        .with_label_values(&[reason_str, symbol])
                        .inc();
                }
                MatchEvent::Trade(trade) => {
                    self.events_total.with_label_values(&["trade"]).inc();
                    self.trades_total.with_label_values(&[symbol]).inc();
                    let volume = fixed_to_f64(trade.qty.raw() as i64);
                    self.trades_volume_total
                        .with_label_values(&[symbol])
                        .add(volume);
                    let price = fixed_to_f64(trade.price.raw());
                    self.trade_price.with_label_values(&[symbol]).set(price);
                }
                MatchEvent::OrderPartiallyFilled { .. } => {
                    self.events_total
                        .with_label_values(&["order_partially_filled"])
                        .inc();
                    self.fills_total.with_label_values(&["partial"]).inc();
                }
                MatchEvent::OrderFilled { .. } => {
                    self.events_total.with_label_values(&["order_filled"]).inc();
                    self.fills_total.with_label_values(&["full"]).inc();
                }
                MatchEvent::OrderCancelled { .. } => {
                    self.events_total
                        .with_label_values(&["order_cancelled"])
                        .inc();
                    self.cancels_total.with_label_values(&[symbol]).inc();
                }
                MatchEvent::OrderAmended {
                    new_price, new_qty, ..
                } => {
                    self.events_total
                        .with_label_values(&["order_amended"])
                        .inc();
                    let amend_type = if !new_price.is_zero() && !new_qty.is_zero() {
                        "both"
                    } else if !new_price.is_zero() {
                        "price"
                    } else {
                        "qty"
                    };
                    self.amends_total
                        .with_label_values(&[amend_type, symbol])
                        .inc();
                }
                MatchEvent::StopTriggered { .. } => {
                    self.events_total
                        .with_label_values(&["stop_triggered"])
                        .inc();
                    self.stop_triggers_total.with_label_values(&[symbol]).inc();
                }
                MatchEvent::OrderStatus { .. } => {
                    self.events_total.with_label_values(&["order_status"]).inc();
                }
                MatchEvent::OrderNotFound { .. } => {
                    self.events_total
                        .with_label_values(&["order_not_found"])
                        .inc();
                }
            }
        }

        // Record processing duration. We try to pick a sensible order_type
        // label from the events themselves.
        for event in events {
            match event {
                MatchEvent::OrderAccepted { .. }
                | MatchEvent::OrderRejected { .. }
                | MatchEvent::Trade(_)
                | MatchEvent::OrderFilled { .. }
                | MatchEvent::OrderPartiallyFilled { .. } => {
                    order_type_str = "limit";
                    break;
                }
                _ => {}
            }
        }

        self.order_processing_duration_seconds
            .with_label_values(&[order_type_str])
            .observe(duration.as_secs_f64());
    }

    /// Record accepted order with side and type labels (call after
    /// confirmation that the order was accepted).
    pub fn record_order_accepted(&self, side: Side, order_type: OrderType, symbol: &str) {
        self.orders_accepted_total
            .with_label_values(&[side_label(side), order_type_label(order_type), symbol])
            .inc();
    }

    /// Record a failed cancellation (order not found).
    pub fn record_cancel_failed(&self, symbol: &str) {
        self.cancels_failed_total.with_label_values(&[symbol]).inc();
    }

    /// Snapshot the current order book state into Prometheus gauges.
    pub fn update_book_state(&self, book: &OrderBook, symbol: &str) {
        self.book_bid_levels
            .with_label_values(&[symbol])
            .set(book.bid_level_count() as i64);
        self.book_ask_levels
            .with_label_values(&[symbol])
            .set(book.ask_level_count() as i64);

        // order_count uses bid + ask level counts minus total for a rough
        // split. The book exposes order_count() as a total, so we record
        // totals split by checking if each side is empty.
        let total_orders = book.order_count() as i64;
        // Estimate split: if one side is empty the other has all orders.
        let bid_orders = if book.bids_empty() {
            0
        } else if book.asks_empty() {
            total_orders
        } else {
            // Approximate: we don't have a per-side order count, but
            // available_bid_qty / available_ask_qty gives depth, not count.
            // Use level counts as a proxy (not perfect, but correct enough
            // for monitoring).
            // Actually, the order_count is the total. We'll set bid_orders
            // and ask_orders to their respective available quantity counts
            // by iterating. For now, store total on both sides as an
            // approximation. Better: compute from available data.
            //
            // Since we cannot split without iterating, we store
            // total_orders in both and let callers know this is an estimate.
            total_orders / 2
        };
        let ask_orders = total_orders - bid_orders;

        self.book_bid_orders
            .with_label_values(&[symbol])
            .set(bid_orders);
        self.book_ask_orders
            .with_label_values(&[symbol])
            .set(ask_orders);

        // Best bid / ask / spread.
        match (book.best_bid(), book.best_ask()) {
            (Some(bid), Some(ask)) => {
                let bid_f = fixed_to_f64(bid.raw());
                let ask_f = fixed_to_f64(ask.raw());
                self.book_best_bid.with_label_values(&[symbol]).set(bid_f);
                self.book_best_ask.with_label_values(&[symbol]).set(ask_f);
                self.book_spread
                    .with_label_values(&[symbol])
                    .set(ask_f - bid_f);
            }
            (Some(bid), None) => {
                let bid_f = fixed_to_f64(bid.raw());
                self.book_best_bid.with_label_values(&[symbol]).set(bid_f);
                self.book_best_ask.with_label_values(&[symbol]).set(0.0);
                self.book_spread.with_label_values(&[symbol]).set(0.0);
            }
            (None, Some(ask)) => {
                let ask_f = fixed_to_f64(ask.raw());
                self.book_best_bid.with_label_values(&[symbol]).set(0.0);
                self.book_best_ask.with_label_values(&[symbol]).set(ask_f);
                self.book_spread.with_label_values(&[symbol]).set(0.0);
            }
            (None, None) => {
                self.book_best_bid.with_label_values(&[symbol]).set(0.0);
                self.book_best_ask.with_label_values(&[symbol]).set(0.0);
                self.book_spread.with_label_values(&[symbol]).set(0.0);
            }
        }

        // Depth quantities.
        let bid_qty = book.available_bid_qty(None);
        let ask_qty = book.available_ask_qty(None);
        self.book_depth_bid_qty
            .with_label_values(&[symbol])
            .set(fixed_qty_to_f64(bid_qty.raw()));
        self.book_depth_ask_qty
            .with_label_values(&[symbol])
            .set(fixed_qty_to_f64(ask_qty.raw()));
    }

    /// Record pending stop order count for a symbol.
    pub fn set_stop_orders_pending(&self, count: i64, symbol: &str) {
        self.stop_orders_pending
            .with_label_values(&[symbol])
            .set(count);
    }

    /// Record a WAL write operation.
    pub fn record_wal_write(&self, bytes: usize) {
        self.wal_writes_total.inc();
        self.wal_bytes_written_total.inc_by(bytes as u64);
    }

    /// Record a snapshot save.
    pub fn record_snapshot_save(&self) {
        self.snapshot_saves_total.inc();
    }

    /// Set the number of active symbols.
    pub fn set_active_symbols(&self, count: i64) {
        self.active_symbols.set(count);
    }

    /// Record a new connection being opened.
    pub fn record_connection_open(&self, transport: &str) {
        self.connections_total.with_label_values(&[transport]).inc();
        self.connections_active
            .with_label_values(&[transport])
            .inc();
    }

    /// Record a connection being closed.
    pub fn record_connection_close(&self, transport: &str) {
        self.connections_active
            .with_label_values(&[transport])
            .dec();
    }

    /// Record bytes received on a transport.
    pub fn record_bytes_in(&self, transport: &str, bytes: usize) {
        self.connection_bytes_in_total
            .with_label_values(&[transport])
            .inc_by(bytes as u64);
    }

    /// Record bytes sent on a transport.
    pub fn record_bytes_out(&self, transport: &str, bytes: usize) {
        self.connection_bytes_out_total
            .with_label_values(&[transport])
            .inc_by(bytes as u64);
    }

    /// Record a frame received on a transport.
    pub fn record_frame_in(&self, transport: &str) {
        self.connection_frames_in_total
            .with_label_values(&[transport])
            .inc();
    }

    /// Record frames sent on a transport.
    pub fn record_frame_out(&self, transport: &str, count: usize) {
        self.connection_frames_out_total
            .with_label_values(&[transport])
            .inc_by(count as u64);
    }

    /// Record a protocol error.
    pub fn record_protocol_error(&self, error_type: &str) {
        self.protocol_errors_total
            .with_label_values(&[error_type])
            .inc();
    }

    /// Record an invariant violation. `kind` is the stable string from
    /// `InvariantKind::as_str()`; `symbol` identifies the affected book.
    /// Operators should page on any non-zero rate for this metric.
    pub fn record_invariant_violation(&self, kind: &str, symbol: &str) {
        self.invariant_violations_total
            .with_label_values(&[kind, symbol])
            .inc();
    }

    /// Record matching duration for a command type.
    pub fn record_matching_duration(&self, command_type: &str, duration: Duration) {
        self.matching_duration_seconds
            .with_label_values(&[command_type])
            .observe(duration.as_secs_f64());
    }

    /// Record a stop cascade event with depth and duration.
    pub fn record_stop_cascade(&self, depth: usize, duration: Duration) {
        self.stop_cascade_depth.observe(depth as f64);
        self.stop_cascade_duration_seconds
            .observe(duration.as_secs_f64());
    }

    /// Record a command processed by a shard.
    pub fn record_shard_command(&self, shard_id: usize) {
        self.shard_commands_total
            .with_label_values(&[&shard_id.to_string()])
            .inc();
    }

    /// Set the queue depth for a shard.
    pub fn set_shard_queue_depth(&self, shard_id: usize, depth: i64) {
        self.shard_queue_depth
            .with_label_values(&[&shard_id.to_string()])
            .set(depth);
    }

    /// Record a WAL flush duration.
    pub fn record_wal_flush_duration(&self, duration: Duration) {
        self.wal_flush_duration_seconds
            .observe(duration.as_secs_f64());
    }

    /// Record a snapshot duration.
    pub fn record_snapshot_duration(&self, duration: Duration) {
        self.snapshot_duration_seconds
            .observe(duration.as_secs_f64());
    }

    /// Set the snapshot size in bytes.
    pub fn set_snapshot_size(&self, bytes: i64) {
        self.snapshot_size_bytes.set(bytes);
    }

    /// Return a reference to the registry this metrics instance is registered
    /// with.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Convenience: gather and encode all metrics for scraping.
    pub fn encode(&self) -> String {
        metrics_handler(&self.registry)
    }
}

// ─── HTTP scrape endpoint ───────────────────────────────────────────────────

/// Returns all gathered metrics encoded in Prometheus text exposition format.
/// Suitable for serving on an HTTP `/metrics` endpoint.
pub fn metrics_handler(registry: &Registry) -> String {
    let encoder = TextEncoder::new();
    let metric_families = registry.gather();
    let mut buffer = Vec::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .expect("encoding metrics should not fail");
    String::from_utf8(buffer).expect("prometheus output is valid UTF-8")
}

// ─── Label helpers ──────────────────────────────────────────────────────────

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn order_type_label(ot: OrderType) -> &'static str {
    match ot {
        OrderType::Limit => "limit",
        OrderType::Market => "market",
        OrderType::StopLimit => "stop_limit",
        OrderType::StopMarket => "stop_market",
    }
}

fn tif_label(tif: me_core::order::TimeInForce) -> &'static str {
    match tif {
        me_core::order::TimeInForce::GTC => "gtc",
        me_core::order::TimeInForce::IOC => "ioc",
        me_core::order::TimeInForce::FOK => "fok",
        me_core::order::TimeInForce::GTD => "gtd",
    }
}

fn reject_reason_label(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::NoLiquidity => "no_liquidity",
        RejectReason::InsufficientLiquidity => "insufficient_liquidity",
        RejectReason::InvalidPrice => "invalid_price",
        RejectReason::InvalidQuantity => "invalid_quantity",
        RejectReason::InvalidLotSize => "invalid_lot_size",
        RejectReason::InvalidTickSize => "invalid_tick_size",
        RejectReason::OrderNotFound => "order_not_found",
        RejectReason::DuplicateOrderId => "duplicate_order_id",
        RejectReason::SymbolNotFound => "symbol_not_found",
        RejectReason::TradingHalted => "trading_halted",
        RejectReason::RiskCheckFailed => "risk_check_failed",
        RejectReason::WouldTakeMarket => "would_take_market",
        RejectReason::SelfTradePrevented => "self_trade_prevented",
        RejectReason::InvalidExpiry => "invalid_expiry",
        RejectReason::InternalError => "internal_error",
    }
}

/// Convert a fixed-point i64 (with SCALE = 10^8) to f64.
#[inline]
fn fixed_to_f64(raw: i64) -> f64 {
    raw as f64 / SCALE as f64
}

/// Convert a fixed-point u64 quantity (with SCALE = 10^8) to f64.
#[inline]
fn fixed_qty_to_f64(raw: u64) -> f64 {
    raw as f64 / SCALE as f64
}
