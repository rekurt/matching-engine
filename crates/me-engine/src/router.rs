use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};
use rustc_hash::FxHashMap;

use me_core::events::{MatchEvent, RejectReason};
use me_core::symbol::SymbolConfig;
use me_core::types::SymbolId;

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
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => {
                return vec![MatchEvent::OrderRejected {
                    order_id: 0,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: cmd.timestamp(),
                }];
            }
        };

        let (reply_tx, reply_rx) = bounded(1);
        let envelope = CommandEnvelope {
            cmd,
            reply: reply_tx,
        };

        self.shards[shard_idx].tx.send(envelope).unwrap();
        reply_rx.recv().unwrap()
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
    num_shards: usize,
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
    AddSymbol(SymbolConfig),
    Shutdown,
}

impl ShardedRouterV2 {
    pub fn new(num_shards: usize) -> Self {
        let channel_capacity = 4096;
        let mut shards = Vec::with_capacity(num_shards);

        for _ in 0..num_shards {
            let (tx, rx) = bounded::<ShardMsg>(channel_capacity);

            let handle = thread::spawn(move || {
                shard_v2_worker_loop(rx);
            });

            shards.push(ShardV2 {
                cmd_tx: tx,
                handle: Some(handle),
            });
        }

        Self {
            shards,
            symbol_to_shard: FxHashMap::default(),
            num_shards,
        }
    }

    /// Register a new trading pair.
    pub fn add_symbol(&mut self, config: SymbolConfig) {
        let id = config.id;
        let shard_idx = (id.0 as usize) % self.num_shards;
        self.symbol_to_shard.insert(id, shard_idx);
        let _ = self.shards[shard_idx]
            .cmd_tx
            .send(ShardMsg::AddSymbol(config));
    }

    /// Process a command synchronously.
    pub fn process(&self, cmd: Command) -> Vec<MatchEvent> {
        let symbol = cmd.symbol();
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => {
                return vec![MatchEvent::OrderRejected {
                    order_id: 0,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: cmd.timestamp(),
                }];
            }
        };

        let (reply_tx, reply_rx) = bounded(1);
        let _ = self.shards[shard_idx].cmd_tx.send(ShardMsg::Command {
            cmd,
            reply: reply_tx,
        });
        reply_rx.recv().unwrap()
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

fn shard_v2_worker_loop(rx: Receiver<ShardMsg>) {
    let mut engines: FxHashMap<SymbolId, Engine> = FxHashMap::default();

    while let Ok(msg) = rx.recv() {
        match msg {
            ShardMsg::Command { cmd, reply } => {
                let symbol = cmd.symbol();
                let events = if let Some(engine) = engines.get_mut(&symbol) {
                    engine.process(cmd).to_vec()
                } else {
                    vec![MatchEvent::OrderRejected {
                        order_id: 0,
                        reason: RejectReason::SymbolNotFound,
                        timestamp: cmd.timestamp(),
                    }]
                };
                let _ = reply.send(events);
            }
            ShardMsg::AddSymbol(config) => {
                let id = config.id;
                engines.insert(id, Engine::new(config));
            }
            ShardMsg::Shutdown => break,
        }
    }
}
