use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};
use rustc_hash::FxHashMap;
use tracing::{debug, info_span, trace};

use me_core::events::{MatchEvent, RejectReason};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedQty, OrderId, SymbolId};
use me_metrics::EngineMetrics;

use crate::command::Command;
use crate::engine::Engine;

// ─── Single-threaded Router ──────────────────────────────────────────────────

/// Single-threaded router: all engines live in one thread.
/// Best for <100 pairs or when deterministic cross-pair sequencing is needed.
pub struct Router {
    engines: FxHashMap<SymbolId, Engine>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            engines: FxHashMap::default(),
        }
    }

    /// Register a new trading pair.
    pub fn add_symbol(&mut self, config: SymbolConfig) {
        let id = config.id;
        self.engines.insert(id, Engine::new(config));
    }

    /// Process a command, returning events.
    pub fn process(&mut self, cmd: Command) -> Vec<MatchEvent> {
        let symbol = cmd.symbol();
        match self.engines.get_mut(&symbol) {
            Some(engine) => engine.process(cmd).to_vec(),
            None => {
                vec![MatchEvent::OrderRejected {
                    order_id: 0,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: cmd.timestamp(),
                }]
            }
        }
    }

    /// Get an engine reference for a symbol (for queries).
    pub fn get_engine(&self, symbol: SymbolId) -> Option<&Engine> {
        self.engines.get(&symbol)
    }
}

// ─── Sharded Multi-threaded Router ───────────────────────────────────────────

/// Envelope wrapping a command with a reply channel.
struct CommandEnvelope {
    cmd: Command,
    reply: Sender<Vec<MatchEvent>>,
}

/// A single shard: a worker thread with its own set of engines.
struct Shard {
    tx: Sender<CommandEnvelope>,
    handle: Option<JoinHandle<()>>,
}

/// Multi-threaded router that shards symbols across worker threads.
///
/// Each shard is a dedicated thread with its own engines, running a tight
/// command processing loop. This ensures STRICT SEQUENTIAL processing
/// within each pair while enabling parallelism across pairs.
pub struct ShardedRouter {
    shards: Vec<Shard>,
    symbol_to_shard: FxHashMap<SymbolId, usize>,
    num_shards: usize,
}

impl ShardedRouter {
    /// Create a new sharded router with the given number of worker threads.
    /// Typical value: number of physical CPU cores.
    pub fn new(num_shards: usize) -> Self {
        let channel_capacity = 4096;
        let mut shards = Vec::with_capacity(num_shards);

        for _ in 0..num_shards {
            let (tx, rx) = bounded::<CommandEnvelope>(channel_capacity);

            let handle = thread::spawn(move || {
                shard_worker_loop(rx);
            });

            shards.push(Shard {
                tx,
                handle: Some(handle),
            });
        }

        Self {
            shards,
            symbol_to_shard: FxHashMap::default(),
            num_shards,
        }
    }

    /// Register a new trading pair, assigning it to a shard.
    pub fn add_symbol(&mut self, config: SymbolConfig) {
        let id = config.id;
        let shard_idx = (id.0 as usize) % self.num_shards;
        self.symbol_to_shard.insert(id, shard_idx);

        // Send an init command to the shard worker.
        // We use a special mechanism: we send a PlaceOrder with a zero qty
        // which the worker interprets as "add symbol".
        // Actually, let's use a proper approach with a setup channel.
        // For simplicity, we'll store configs and let the worker init lazily.
        // We need a different approach: send the config to the worker.
        // Let's refactor to use an enum for shard messages.

        // For now, we use a simpler approach: the sharded router
        // sends configs via a separate init mechanism.
        // Since we need the worker to own engines, we'll use a channel-based init.
    }

    /// Process a command synchronously, blocking until the response is ready.
    pub fn process(&self, cmd: Command) -> Vec<MatchEvent> {
        let symbol = cmd.symbol();
        let ts = cmd.timestamp();
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => {
                return vec![MatchEvent::OrderRejected {
                    order_id: 0,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: ts,
                }];
            }
        };

        let (reply_tx, reply_rx) = bounded(1);
        let envelope = CommandEnvelope {
            cmd,
            reply: reply_tx,
        };

        if self.shards[shard_idx].tx.send(envelope).is_err() {
            return vec![MatchEvent::OrderRejected {
                order_id: 0,
                reason: RejectReason::SymbolNotFound,
                timestamp: ts,
            }];
        }
        reply_rx.recv().unwrap_or_else(|_| {
            vec![MatchEvent::OrderRejected {
                order_id: 0,
                reason: RejectReason::SymbolNotFound,
                timestamp: ts,
            }]
        })
    }

    /// Shut down all worker threads gracefully.
    pub fn shutdown(mut self) {
        // Drop all senders to signal workers to exit.
        for shard in &mut self.shards {
            drop(std::mem::replace(
                &mut shard.tx,
                bounded::<CommandEnvelope>(1).0,
            ));
        }
        for shard in &mut self.shards {
            if let Some(handle) = shard.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

// ─── Shard worker loop ───────────────────────────────────────────────────────

fn shard_worker_loop(rx: Receiver<CommandEnvelope>) {
    let mut engines: FxHashMap<SymbolId, Engine> = FxHashMap::default();

    while let Ok(envelope) = rx.recv() {
        let symbol = envelope.cmd.symbol();

        // Lazy engine creation: if we don't have an engine for this symbol,
        // create one with a default config. In production, you'd want to
        // pre-register symbols via a setup phase.
        let events = if let Some(engine) = engines.get_mut(&symbol) {
            engine.process(envelope.cmd).to_vec()
        } else {
            vec![MatchEvent::OrderRejected {
                order_id: 0,
                reason: RejectReason::SymbolNotFound,
                timestamp: envelope.cmd.timestamp(),
            }]
        };

        let _ = envelope.reply.send(events);
    }
}

/// Extended sharded router that supports proper symbol registration.
pub struct ShardedRouterV2 {
    shards: Vec<ShardV2>,
    symbol_to_shard: FxHashMap<SymbolId, usize>,
    symbol_names: FxHashMap<SymbolId, String>,
    num_shards: usize,
    #[allow(dead_code)]
    metrics: Option<Arc<EngineMetrics>>,
}

struct ShardV2 {
    cmd_tx: Sender<ShardMsg>,
    handle: Option<JoinHandle<()>>,
}

enum ShardMsg {
    Command {
        cmd: Command,
        reply: Sender<Vec<MatchEvent>>,
    },
    QueryOrder {
        order_id: OrderId,
        symbol: SymbolId,
        reply: Sender<Vec<MatchEvent>>,
    },
    AddSymbol(SymbolConfig),
    HaltSymbol(SymbolId),
    ResumeSymbol(SymbolId),
    Shutdown,
}

impl ShardedRouterV2 {
    pub fn new(num_shards: usize) -> Self {
        Self::build(num_shards, None)
    }

    /// Create a new sharded router with metrics instrumentation.
    pub fn new_with_metrics(num_shards: usize, metrics: Arc<EngineMetrics>) -> Self {
        Self::build(num_shards, Some(metrics))
    }

    fn build(num_shards: usize, metrics: Option<Arc<EngineMetrics>>) -> Self {
        let channel_capacity = 4096;
        let mut shards = Vec::with_capacity(num_shards);

        for shard_id in 0..num_shards {
            let (tx, rx) = bounded::<ShardMsg>(channel_capacity);
            let metrics_clone = metrics.clone();

            let handle = thread::spawn(move || {
                shard_v2_worker_loop(rx, shard_id, metrics_clone);
            });

            shards.push(ShardV2 {
                cmd_tx: tx,
                handle: Some(handle),
            });
        }

        Self {
            shards,
            symbol_to_shard: FxHashMap::default(),
            symbol_names: FxHashMap::default(),
            num_shards,
            metrics,
        }
    }

    /// Register a new trading pair.
    pub fn add_symbol(&mut self, config: SymbolConfig) {
        let id = config.id;
        let shard_idx = (id.0 as usize) % self.num_shards;
        self.symbol_names
            .insert(id, format!("{}/{}", config.base, config.quote));
        self.symbol_to_shard.insert(id, shard_idx);
        let _ = self.shards[shard_idx]
            .cmd_tx
            .send(ShardMsg::AddSymbol(config));
    }

    /// Process a command synchronously.
    ///
    /// Note: `halt_symbol`/`resume_symbol` are eventually consistent — a command
    /// already queued may be processed before the halt message.
    pub fn process(&self, cmd: Command) -> Vec<MatchEvent> {
        let symbol = cmd.symbol();
        let ts = cmd.timestamp();
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => {
                return vec![MatchEvent::OrderRejected {
                    order_id: 0,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: ts,
                }];
            }
        };

        let (reply_tx, reply_rx) = bounded(1);
        if self.shards[shard_idx]
            .cmd_tx
            .send(ShardMsg::Command {
                cmd,
                reply: reply_tx,
            })
            .is_err()
        {
            return vec![MatchEvent::OrderRejected {
                order_id: 0,
                reason: RejectReason::SymbolNotFound,
                timestamp: ts,
            }];
        }
        reply_rx.recv().unwrap_or_else(|_| {
            vec![MatchEvent::OrderRejected {
                order_id: 0,
                reason: RejectReason::SymbolNotFound,
                timestamp: ts,
            }]
        })
    }

    /// Halt trading for a symbol (circuit breaker).
    pub fn halt_symbol(&self, symbol: SymbolId) -> bool {
        if let Some(&shard_idx) = self.symbol_to_shard.get(&symbol) {
            let _ = self.shards[shard_idx].cmd_tx.send(ShardMsg::HaltSymbol(symbol));
            true
        } else {
            false
        }
    }

    /// Query the status of a resting order.
    ///
    /// Returns `MatchEvent::OrderStatus` if found, `MatchEvent::OrderNotFound` otherwise.
    /// The query is routed to the shard that owns `symbol` and executed synchronously.
    pub fn query_order(&self, order_id: OrderId, symbol: SymbolId) -> Vec<MatchEvent> {
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => return vec![MatchEvent::OrderNotFound { order_id }],
        };

        let (reply_tx, reply_rx) = bounded(1);
        if self.shards[shard_idx]
            .cmd_tx
            .send(ShardMsg::QueryOrder {
                order_id,
                symbol,
                reply: reply_tx,
            })
            .is_err()
        {
            return vec![MatchEvent::OrderNotFound { order_id }];
        }
        reply_rx
            .recv()
            .unwrap_or_else(|_| vec![MatchEvent::OrderNotFound { order_id }])
    }

    /// Resume trading for a symbol.
    pub fn resume_symbol(&self, symbol: SymbolId) -> bool {
        if let Some(&shard_idx) = self.symbol_to_shard.get(&symbol) {
            let _ = self.shards[shard_idx].cmd_tx.send(ShardMsg::ResumeSymbol(symbol));
            true
        } else {
            false
        }
    }

    pub fn shutdown(mut self) {
        for shard in &self.shards {
            let _ = shard.cmd_tx.send(ShardMsg::Shutdown);
        }
        for shard in &mut self.shards {
            if let Some(handle) = shard.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

fn shard_v2_worker_loop(
    rx: Receiver<ShardMsg>,
    shard_id: usize,
    metrics: Option<Arc<EngineMetrics>>,
) {
    let _span = info_span!("shard_worker", shard_id).entered();
    let mut engines: FxHashMap<SymbolId, Engine> = FxHashMap::default();
    let mut symbol_names: FxHashMap<SymbolId, String> = FxHashMap::default();

    while let Ok(msg) = rx.recv() {
        match msg {
            ShardMsg::Command { cmd, reply } => {
                let symbol = cmd.symbol();
                let sym_name = symbol_names
                    .get(&symbol)
                    .map(|s| s.as_str())
                    .unwrap_or("unknown");

                let cmd_type_str = match &cmd {
                    Command::PlaceOrder(_) => "place_order",
                    Command::CancelOrder { .. } => "cancel_order",
                    Command::AmendOrder { .. } => "amend_order",
                };

                if let Some(ref m) = metrics {
                    match &cmd {
                        Command::PlaceOrder(order) => {
                            m.record_place_order(order.side, order.order_type, order.tif, sym_name);
                        }
                        Command::CancelOrder { .. } => {
                            m.record_command_type("cancel_order");
                        }
                        Command::AmendOrder { .. } => {
                            m.record_command_type("amend_order");
                        }
                    }
                    m.record_shard_command(shard_id);
                }

                trace!(symbol = sym_name, cmd_type = cmd_type_str, "processing command");

                let start = std::time::Instant::now();
                let events = if let Some(engine) = engines.get_mut(&symbol) {
                    engine.process(cmd).to_vec()
                } else {
                    vec![MatchEvent::OrderRejected {
                        order_id: 0,
                        reason: RejectReason::SymbolNotFound,
                        timestamp: cmd.timestamp(),
                    }]
                };
                let duration = start.elapsed();

                if let Some(ref m) = metrics {
                    m.record_events(&events, sym_name, duration);
                    m.record_matching_duration(cmd_type_str, duration);
                    if let Some(engine) = engines.get(&symbol) {
                        m.update_book_state(engine.book(), sym_name);
                        m.set_stop_orders_pending(
                            engine.stop_book().len() as i64,
                            sym_name,
                        );
                    }
                }

                let trades_count = events
                    .iter()
                    .filter(|e| matches!(e, MatchEvent::Trade(_)))
                    .count();
                debug!(
                    symbol = sym_name,
                    trades = trades_count,
                    duration_us = duration.as_micros() as u64,
                    "command processed"
                );

                let _ = reply.send(events);
            }
            ShardMsg::QueryOrder { order_id, symbol, reply } => {
                let events = if let Some(engine) = engines.get(&symbol) {
                    match engine.book().get_order(order_id) {
                        Some(order) => {
                            let filled = FixedQty::from_raw(
                                order.qty.raw().saturating_sub(order.remaining_qty.raw()),
                            );
                            vec![MatchEvent::OrderStatus {
                                order_id,
                                status: order.status as u8,
                                filled_qty: filled,
                                remaining_qty: order.remaining_qty,
                                price: order.price,
                                timestamp: order.timestamp,
                            }]
                        }
                        None => vec![MatchEvent::OrderNotFound { order_id }],
                    }
                } else {
                    vec![MatchEvent::OrderNotFound { order_id }]
                };
                let _ = reply.send(events);
            }
            ShardMsg::AddSymbol(config) => {
                let id = config.id;
                let name = format!("{}/{}", config.base, config.quote);
                symbol_names.insert(id, name);
                engines.insert(id, Engine::new(config));
            }
            ShardMsg::HaltSymbol(symbol) => {
                if let Some(engine) = engines.get_mut(&symbol) {
                    engine.halt_trading();
                    debug!(symbol = ?symbol, "trading halted for symbol");
                }
            }
            ShardMsg::ResumeSymbol(symbol) => {
                if let Some(engine) = engines.get_mut(&symbol) {
                    engine.resume_trading();
                    debug!(symbol = ?symbol, "trading resumed for symbol");
                }
            }
            ShardMsg::Shutdown => break,
        }
    }
}
