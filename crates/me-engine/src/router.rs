use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Upper bound on how long a caller waits for a shard to reply before treating
/// the command as failed. Bounds the blast radius of a wedged shard (e.g. a
/// stalled disk during WAL fsync) so it cannot hang every client thread.
const SHARD_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of commands a Pre-mode shard accumulates before forcing a
/// group-commit fsync. Bounds how long a reply can be deferred and how much
/// memory pending replies hold. Under load the queue rarely drains, so most
/// fsyncs amortize across a full batch; under light load the queue empties and
/// the batch commits immediately (no added latency).
const MAX_GROUP_BATCH: usize = 4096;

/// A processed command awaiting durable commit: (reply channel, order id for an
/// InternalError reply if the commit fsync fails, the success events).
type PendingReply = (Sender<Vec<MatchEvent>>, OrderId, Vec<MatchEvent>);

use crossbeam_channel::{bounded, Receiver, Sender};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, info_span, trace};

use me_audit::async_logger::AsyncAuditLogger;
use me_audit::logger::{match_event_to_entry, order_to_entry};
use me_core::events::{MatchEvent, RejectReason};
use me_core::symbol::SymbolConfig;
use me_core::types::{OrderId, SymbolId};
use me_metrics::EngineMetrics;
use me_persist::snapshot::SnapshotManager;
use me_persist::wal::{WalCommand, WalEntry, WalReader, WalWriter};
use me_risk::config::RiskConfig;
use me_risk::engine::RiskEngine;

use crate::command::Command;
use crate::engine::Engine;

/// WAL mode: when to write the write-ahead log relative to matching.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum WalMode {
    /// Write WAL before matching (full durability, slightly higher latency).
    Pre,
    /// Write WAL after matching (lower latency, may lose last commands on crash).
    Post,
    /// WAL disabled.
    #[default]
    Disabled,
}

/// WAL configuration for the router.
#[derive(Clone, Debug, Default)]
pub struct WalConfig {
    pub mode: WalMode,
    pub path: String,
    pub sync_interval: u64,
    /// Directory for periodic book snapshots. `None` disables snapshotting
    /// (recovery then replays the entire WAL from genesis).
    pub snapshot_dir: Option<String>,
    /// Take a snapshot of all symbols on the shard every N processed commands,
    /// then truncate the WAL. `0` disables periodic snapshots.
    pub snapshot_interval: u64,
}

/// Thread affinity configuration.
#[derive(Clone, Debug, Default)]
pub struct AffinityConfig {
    /// Pin each shard worker thread to a CPU core.
    pub pin_threads: bool,
    /// Set real-time scheduling priority (requires root on Linux).
    pub realtime_priority: bool,
}

// ─── Router ─────────────────────────────────────────────────────────────────

/// Sharded multi-threaded router with integrated Risk + Audit.
///
/// Each shard is a dedicated OS thread with its own engines and risk engine.
/// Strict sequential processing within each symbol, parallelism across symbols.
///
/// Hot path per command:
///   risk check (~37 ns) → match (~134 ns) → async audit send (~10 ns) = ~180 ns
pub struct Router {
    shards: Vec<Shard>,
    symbol_to_shard: FxHashMap<SymbolId, usize>,
    symbol_names: FxHashMap<SymbolId, String>,
    num_shards: usize,
    audit: Option<Arc<AsyncAuditLogger>>,
    #[allow(dead_code)]
    affinity: AffinityConfig,
    #[allow(dead_code)]
    metrics: Option<Arc<EngineMetrics>>,
}

struct Shard {
    tx: Sender<ShardMsg>,
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
    /// Query all open orders for a client session across all symbols in this shard.
    /// Used by protocol v2 GetOpenOrders → OpenOrders snapshot response.
    GetOpenOrdersBySession {
        session_id: me_core::SessionId,
        reply: Sender<Vec<me_core::order::Order>>,
    },
    AddSymbol {
        config: SymbolConfig,
        risk_config: Option<RiskConfig>,
    },
    HaltSymbol(SymbolId),
    ResumeSymbol(SymbolId),
    Shutdown,
}

impl Router {
    pub fn new(num_shards: usize) -> Self {
        Self::build(num_shards, None, None, None)
    }

    pub fn new_with_metrics(num_shards: usize, metrics: Arc<EngineMetrics>) -> Self {
        Self::build(num_shards, Some(metrics), None, None)
    }

    /// Create a router with full HFT pipeline: metrics + async audit + WAL.
    pub fn new_full(
        num_shards: usize,
        metrics: Option<Arc<EngineMetrics>>,
        audit: Option<Arc<AsyncAuditLogger>>,
    ) -> Self {
        Self::build(num_shards, metrics, audit, None)
    }

    /// Create a router with full HFT pipeline including WAL.
    pub fn new_with_wal(
        num_shards: usize,
        metrics: Option<Arc<EngineMetrics>>,
        audit: Option<Arc<AsyncAuditLogger>>,
        wal_config: WalConfig,
    ) -> Self {
        Self::build(num_shards, metrics, audit, Some(wal_config))
    }

    /// Create a router with full pipeline + thread affinity.
    pub fn new_with_affinity(
        num_shards: usize,
        metrics: Option<Arc<EngineMetrics>>,
        audit: Option<Arc<AsyncAuditLogger>>,
        wal_config: Option<WalConfig>,
        affinity: AffinityConfig,
    ) -> Self {
        Self::build_with_affinity(num_shards, metrics, audit, wal_config, affinity)
    }

    fn build(
        num_shards: usize,
        metrics: Option<Arc<EngineMetrics>>,
        audit: Option<Arc<AsyncAuditLogger>>,
        wal_config: Option<WalConfig>,
    ) -> Self {
        Self::build_with_affinity(
            num_shards,
            metrics,
            audit,
            wal_config,
            AffinityConfig::default(),
        )
    }

    fn build_with_affinity(
        num_shards: usize,
        metrics: Option<Arc<EngineMetrics>>,
        audit: Option<Arc<AsyncAuditLogger>>,
        wal_config: Option<WalConfig>,
        affinity: AffinityConfig,
    ) -> Self {
        let channel_capacity = 4096;
        let mut shards = Vec::with_capacity(num_shards);

        for shard_id in 0..num_shards {
            let (tx, rx) = bounded::<ShardMsg>(channel_capacity);
            let metrics_clone = metrics.clone();
            let audit_clone = audit.clone();
            let wal_cfg = wal_config.clone();
            let affinity_cfg = affinity.clone();

            let handle = thread::Builder::new()
                .name(format!("me-shard-{shard_id}"))
                .spawn(move || {
                    // Apply thread affinity before any work
                    if affinity_cfg.pin_threads {
                        crate::affinity::pin_to_core(shard_id);
                    }
                    if affinity_cfg.realtime_priority {
                        if let Err(e) = crate::affinity::set_realtime_priority() {
                            // Missing CAP_SYS_NICE / running as non-root is
                            // the usual cause. Log at WARN with shard_id so
                            // operators can see per-thread status and catch
                            // misconfigured HFT deployments.
                            tracing::warn!(
                                shard_id,
                                error = %e,
                                "set_realtime_priority FAILED — shard running at normal priority"
                            );
                        }
                    }

                    shard_worker_loop(rx, shard_id, metrics_clone, audit_clone, wal_cfg);
                })
                .expect("failed to spawn shard worker thread");

            shards.push(Shard {
                tx,
                handle: Some(handle),
            });
        }

        Self {
            shards,
            symbol_to_shard: FxHashMap::default(),
            symbol_names: FxHashMap::default(),
            num_shards,
            audit,
            affinity: AffinityConfig::default(),
            metrics,
        }
    }

    /// Register a new trading pair with optional risk config.
    pub fn add_symbol(&mut self, config: SymbolConfig) {
        self.add_symbol_with_risk(config, None);
    }

    /// Register a new trading pair with risk config.
    ///
    /// If the shard worker thread is dead, the symbol IS added to
    /// `symbol_to_shard` but the shard won't create an engine for it, so
    /// subsequent commands will be routed to a shard that can't handle them.
    /// This is a fatal startup-time condition — panic rather than producing
    /// a subtly broken server.
    pub fn add_symbol_with_risk(&mut self, config: SymbolConfig, risk_config: Option<RiskConfig>) {
        let id = config.id;
        let shard_idx = (id.0 as usize) % self.num_shards;
        self.symbol_names
            .insert(id, format!("{}/{}", config.base, config.quote));
        self.symbol_to_shard.insert(id, shard_idx);
        if let Err(e) = self.shards[shard_idx].tx.send(ShardMsg::AddSymbol {
            config,
            risk_config,
        }) {
            panic!(
                "Router::add_symbol failed for symbol {id:?}: shard {shard_idx} is dead ({e}). \
                 This is a startup error; aborting to avoid a subtly broken server."
            );
        }
    }

    /// Process a command synchronously.
    pub fn process(&self, cmd: Command) -> Vec<MatchEvent> {
        let symbol = cmd.symbol();
        let ts = cmd.timestamp();
        let order_id = cmd_order_id(&cmd);
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => {
                return vec![MatchEvent::OrderRejected {
                    order_id,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: ts,
                }];
            }
        };

        let (reply_tx, reply_rx) = bounded(1);
        if self.shards[shard_idx]
            .tx
            .send(ShardMsg::Command {
                cmd,
                reply: reply_tx,
            })
            .is_err()
        {
            // Shard worker dead — this is NOT SymbolNotFound. Report
            // InternalError with full context so operators can diagnose.
            tracing::error!(
                shard_idx,
                symbol = ?symbol,
                order_id,
                "SHARD DEAD: command send failed, returning InternalError"
            );
            return vec![MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::InternalError,
                timestamp: ts,
            }];
        }
        reply_rx
            .recv_timeout(SHARD_REPLY_TIMEOUT)
            .unwrap_or_else(|_| {
                // Reply not received in time: the shard crashed before replying, OR
                // it is stuck (e.g. blocked on a stalled disk during WAL fsync).
                // Bound the wait so a single wedged shard can't hang every caller
                // forever. Client sees InternalError and must reconcile (e.g.
                // GetOrderStatus / resume).
                tracing::error!(
                    shard_idx,
                    symbol = ?symbol,
                    order_id,
                    "REPLY TIMEOUT/DROPPED: shard stuck or crashed, returning InternalError"
                );
                vec![MatchEvent::OrderRejected {
                    order_id,
                    reason: RejectReason::InternalError,
                    timestamp: ts,
                }]
            })
    }

    /// Halt trading for a symbol (circuit breaker).
    ///
    /// Returns `true` if the halt was successfully dispatched to the shard.
    /// Returns `false` if the symbol is unknown OR the shard is dead — the
    /// caller MUST distinguish these cases (check logs for shard-dead errors)
    /// since a broken circuit breaker is a risk-control failure.
    pub fn halt_symbol(&self, symbol: SymbolId) -> bool {
        let Some(&shard_idx) = self.symbol_to_shard.get(&symbol) else {
            return false;
        };
        if let Err(e) = self.shards[shard_idx].tx.send(ShardMsg::HaltSymbol(symbol)) {
            tracing::error!(
                shard_idx,
                symbol = ?symbol,
                error = %e,
                "CIRCUIT BREAKER FAILED: halt_symbol could not dispatch (shard dead)"
            );
            return false;
        }
        true
    }

    /// Resume trading for a symbol.
    ///
    /// Returns `true` if the resume was dispatched; `false` on unknown
    /// symbol or dead shard (logged).
    pub fn resume_symbol(&self, symbol: SymbolId) -> bool {
        let Some(&shard_idx) = self.symbol_to_shard.get(&symbol) else {
            return false;
        };
        if let Err(e) = self.shards[shard_idx]
            .tx
            .send(ShardMsg::ResumeSymbol(symbol))
        {
            tracing::error!(
                shard_idx,
                symbol = ?symbol,
                error = %e,
                "resume_symbol dispatch failed (shard dead)"
            );
            return false;
        }
        true
    }

    /// Query order status.
    pub fn query_order(&self, order_id: OrderId, symbol: SymbolId) -> Vec<MatchEvent> {
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => return vec![MatchEvent::OrderNotFound { order_id }],
        };

        let (reply_tx, reply_rx) = bounded(1);
        if self.shards[shard_idx]
            .tx
            .send(ShardMsg::QueryOrder {
                order_id,
                symbol,
                reply: reply_tx,
            })
            .is_err()
        {
            // Shard worker dead: DO NOT mask as OrderNotFound. The order
            // may actually exist and the client needs to know their query
            // was not authoritatively answered.
            tracing::error!(
                shard_idx,
                symbol = ?symbol,
                order_id,
                "SHARD DEAD: query send failed, returning InternalError"
            );
            return vec![MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::InternalError,
                timestamp: 0,
            }];
        }
        reply_rx
            .recv_timeout(SHARD_REPLY_TIMEOUT)
            .unwrap_or_else(|_| {
                tracing::error!(
                    shard_idx,
                    symbol = ?symbol,
                    order_id,
                    "REPLY TIMEOUT/DROPPED: query_order, returning InternalError"
                );
                vec![MatchEvent::OrderRejected {
                    order_id,
                    reason: RejectReason::InternalError,
                    timestamp: 0,
                }]
            })
    }

    /// Get all open (resting) orders for a client session.
    ///
    /// Fans out `GetOpenOrdersBySession` to every shard (since the session may
    /// have orders on multiple symbols) and collects the results. The caller
    /// blocks until all shards respond.
    ///
    /// Returns `Err` if ANY shard is dead or fails to respond — an
    /// incomplete snapshot MUST NOT be returned as authoritative state
    /// during a post-gap resync, because clients would discard orders
    /// that are still resting on the dead shard.
    ///
    /// Used by protocol v2 GetOpenOrders handler for post-gap resync.
    pub fn get_open_orders_by_session(
        &self,
        session_id: me_core::SessionId,
    ) -> Result<Vec<me_core::order::Order>, String> {
        let mut all_orders = Vec::new();
        let mut replies = Vec::with_capacity(self.num_shards);

        // Fan out: send GetOpenOrdersBySession to every shard.
        for (shard_idx, shard) in self.shards.iter().enumerate() {
            let (tx, rx) = bounded(1);
            if shard
                .tx
                .send(ShardMsg::GetOpenOrdersBySession {
                    session_id,
                    reply: tx,
                })
                .is_err()
            {
                tracing::error!(shard_idx, "OpenOrders snapshot ABORTED: shard dead");
                return Err(format!(
                    "shard {shard_idx} dead: cannot build complete snapshot"
                ));
            }
            replies.push(rx);
        }

        // Collect responses (blocking; each shard replies quickly).
        for rx in replies {
            match rx.recv() {
                Ok(orders) => all_orders.extend(orders),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "OpenOrders snapshot ABORTED: shard reply dropped"
                    );
                    return Err(format!("shard reply dropped: {e}"));
                }
            }
        }
        Ok(all_orders)
    }

    /// Get the async audit logger (for external use if needed).
    pub fn audit(&self) -> Option<&Arc<AsyncAuditLogger>> {
        self.audit.as_ref()
    }

    pub fn shutdown(mut self) {
        for (idx, shard) in self.shards.iter().enumerate() {
            if let Err(e) = shard.tx.send(ShardMsg::Shutdown) {
                // Shard already dead at shutdown — useful debugging info.
                tracing::debug!(
                    shard_id = idx,
                    error = %e,
                    "shutdown: shard already dead, Shutdown send failed"
                );
            }
        }
        for (idx, shard) in self.shards.iter_mut().enumerate() {
            if let Some(handle) = shard.handle.take() {
                if let Err(panic) = handle.join() {
                    // Hot-path shard worker panic is catastrophic — NEVER silent.
                    tracing::error!(
                        shard_id = idx,
                        ?panic,
                        "SHARD WORKER PANICKED during its lifetime"
                    );
                }
            }
        }
    }
}

// ─── Shard worker loop (hot path) ───────────────────────────────────────────

fn shard_worker_loop(
    rx: Receiver<ShardMsg>,
    shard_id: usize,
    metrics: Option<Arc<EngineMetrics>>,
    audit: Option<Arc<AsyncAuditLogger>>,
    wal_config: Option<WalConfig>,
) {
    let _span = info_span!("shard_worker", shard_id).entered();
    let mut engines: FxHashMap<SymbolId, Engine> = FxHashMap::default();
    let mut symbol_names: FxHashMap<SymbolId, String> = FxHashMap::default();
    let mut risk: RiskEngine = RiskEngine::new();

    // Initialize WAL + recover prior state (snapshot + WAL replay) if configured.
    let snapshot_interval = wal_config.as_ref().map_or(0, |c| c.snapshot_interval);
    let WalInit {
        mut wal,
        replay_entries,
        snapshot_mgr,
        wal_mode,
    } = init_wal_and_recover(shard_id, wal_config);

    let wal_pre_open_failed = wal_mode == WalMode::Pre && wal.is_none();
    if wal_pre_open_failed {
        tracing::error!(
            shard_id,
            "WAL pre-mode configured but writer is unavailable; shard will reject all commands to preserve durability"
        );
    }
    let mut commands_since_snapshot: u64 = 0;

    // Group commit applies only to Pre-mode (the mode that must fsync before
    // acknowledging). Post/Disabled keep their existing immediate-reply path.
    let group_commit = wal_mode == WalMode::Pre && wal.is_some();
    let mut pending: Vec<PendingReply> = Vec::new();

    while let Ok(msg) = rx.recv() {
        // Flush any deferred group-commit batch before handling a control
        // message, so replies stay ordered and durability is committed at
        // message boundaries.
        if !matches!(msg, ShardMsg::Command { .. }) && !pending.is_empty() {
            commit_and_flush(shard_id, &mut wal, &mut engines, &mut pending);
        }
        match msg {
            ShardMsg::Command { cmd, reply } => {
                let symbol = cmd.symbol();
                let reply_order_id = cmd_order_id(&cmd);
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

                trace!(symbol = sym_name, cmd_type = cmd_type_str, "processing");

                // ── Hot path: WAL(pre) → Risk → Match → WAL(post) → Audit ──

                let start = std::time::Instant::now();

                // For post-mode WAL, we need the command after matching (which consumes it).
                // Clone only when WAL post-mode is active.
                let wal_cmd_for_post = if wal_mode == WalMode::Post {
                    Some(command_to_wal(&cmd))
                } else {
                    None
                };

                // 0. Lazy GTD expiry sweep — runs BEFORE journaling/risk-checking
                // this command. Two reasons:
                //   1. An already-expired GTD frees its open-order slot so
                //      max_open_orders does not spuriously reject this command.
                //   2. DURABILITY/DETERMINISM: each expiry is journaled as a
                //      CancelOrder *before* the triggering command, so crash
                //      recovery replays the cancellation at the same log
                //      position. Recovery never re-runs the wall-clock sweep, so
                //      without this record replay would resurrect the expired GTD
                //      and let it phantom-match a later order — diverging the
                //      recovered book from the live one.
                // The expiry cancellations are metered/audited and decrement the
                // open-order count, but are not part of this command's reply.
                let mut wal_pre_write_failed = false;
                if let Some(engine) = engines.get_mut(&symbol) {
                    let expired = engine.expire_gtd_now();
                    if !expired.is_empty() {
                        for ev in &expired {
                            if let MatchEvent::OrderCancelled {
                                order_id,
                                timestamp,
                                ..
                            } = ev
                            {
                                // Journal the expiry as a durable cancel. Pre-mode
                                // buffers it (group-committed alongside the
                                // command); Post-mode appends immediately, ahead
                                // of the command's own post-write. Either way the
                                // expiry precedes the command in the log.
                                if let Some(ref mut w) = wal {
                                    let expiry_cmd = WalCommand::CancelOrder {
                                        order_id: *order_id,
                                        symbol,
                                        timestamp: *timestamp,
                                    };
                                    let res = match wal_mode {
                                        WalMode::Pre => w.append_nosync(&expiry_cmd),
                                        WalMode::Post => w.append(&expiry_cmd),
                                        // Unreachable: a present writer implies a
                                        // non-Disabled mode. Treat as no-op.
                                        WalMode::Disabled => Ok(0),
                                    };
                                    if let Err(e) = res {
                                        tracing::error!(
                                            shard_id, order_id, error = %e,
                                            "WAL GTD-EXPIRY WRITE FAILED — recovery would diverge"
                                        );
                                        match wal_mode {
                                            // Pre: reject the triggering command,
                                            // same durability contract as a failed
                                            // command pre-write below.
                                            WalMode::Pre => wal_pre_write_failed = true,
                                            // Post: the book is already mutated but
                                            // not journaled — halt the symbol so no
                                            // further un-journaled state accrues.
                                            WalMode::Post => engine.halt_trading(),
                                            WalMode::Disabled => {}
                                        }
                                    }
                                }
                                risk.on_order_removed(symbol);
                            }
                        }
                        if let Some(ref a) = audit {
                            for ev in &expired {
                                if let Some(entry) = match_event_to_entry(ev, symbol) {
                                    a.log_entry(entry);
                                }
                            }
                        }
                        if let Some(ref m) = metrics {
                            m.record_events(&expired, sym_name, std::time::Duration::ZERO);
                        }
                    }
                }

                // 0.5. WAL pre-trade write of THIS command — AFTER the expiry
                // cancels above, so any expiries precede it in the log.
                //
                // DURABILITY CONTRACT: Pre-mode guarantees that every accepted
                // command is journaled BEFORE execution. If the write fails we
                // MUST reject the order — otherwise a crash immediately after
                // matching would lose the trade, violating the contract.
                if wal_mode == WalMode::Pre && !wal_pre_write_failed {
                    if let Some(ref mut w) = wal {
                        let wal_cmd = command_to_wal(&cmd);
                        // Buffered append only; the batch is fsynced once in
                        // commit_and_flush before any reply is released.
                        if let Err(e) = w.append_nosync(&wal_cmd) {
                            tracing::error!(
                                shard_id, error = %e,
                                "WAL PRE-WRITE FAILED — rejecting order (durability violated)"
                            );
                            wal_pre_write_failed = true;
                        }
                    } else {
                        tracing::error!(
                            shard_id,
                            "WAL PRE-WRITE SKIPPED — writer unavailable, rejecting order (durability violated)"
                        );
                        wal_pre_write_failed = true;
                    }
                }

                // 1. Pre-trade risk check (PlaceOrder only)
                let risk_rejected = if wal_pre_write_failed {
                    false
                } else {
                    match &cmd {
                        Command::PlaceOrder(order) => {
                            if let Some(ref a) = audit {
                                a.log_entry(order_to_entry(order));
                            }
                            match risk.check_order(order) {
                                Ok(()) => false,
                                Err(reason) => {
                                    debug!(
                                        order_id = order.id,
                                        symbol = sym_name,
                                        reason = ?reason,
                                        "risk check rejected order"
                                    );
                                    true
                                }
                            }
                        }
                        _ => false,
                    }
                };

                let incoming_order_id = match &cmd {
                    Command::PlaceOrder(order) => Some(order.id),
                    _ => None,
                };

                let events = if wal_pre_write_failed {
                    // Pre-mode WAL guarantee broken — reject without matching.
                    let (order_id, ts) = match &cmd {
                        Command::PlaceOrder(o) => (o.id, o.timestamp),
                        Command::CancelOrder {
                            order_id,
                            timestamp,
                            ..
                        }
                        | Command::AmendOrder {
                            order_id,
                            timestamp,
                            ..
                        } => (*order_id, *timestamp),
                    };
                    vec![MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::InternalError,
                        timestamp: ts,
                    }]
                } else if risk_rejected {
                    let (order_id, ts) = match &cmd {
                        Command::PlaceOrder(o) => (o.id, o.timestamp),
                        _ => unreachable!(),
                    };
                    vec![MatchEvent::OrderRejected {
                        order_id,
                        reason: RejectReason::RiskCheckFailed,
                        timestamp: ts,
                    }]
                } else if let Some(engine) = engines.get_mut(&symbol) {
                    // 2. Matching (GTD expiry was already swept above, before the
                    // risk check, so expired orders never count against limits).
                    let events = engine.process(cmd).to_vec();

                    // 3. Post-trade risk state updates
                    let triggered_order_ids: FxHashSet<_> = events
                        .iter()
                        .filter_map(|event| match event {
                            MatchEvent::StopTriggered { order_id, .. } => Some(*order_id),
                            _ => None,
                        })
                        .collect();

                    for event in &events {
                        match event {
                            // Stop orders are accepted once when inserted into stop_book,
                            // then accepted again when triggered into the live book.
                            // Count only the original client order acceptance (current cmd)
                            // and skip synthetic acceptances produced by stop triggers.
                            MatchEvent::OrderAccepted { order_id, .. }
                                if Some(*order_id) == incoming_order_id
                                    && !triggered_order_ids.contains(order_id) =>
                            {
                                risk.on_order_added(symbol);
                            }
                            MatchEvent::OrderFilled { .. } | MatchEvent::OrderCancelled { .. } => {
                                risk.on_order_removed(symbol);
                            }
                            MatchEvent::Trade(t) => {
                                risk.update_reference_price(symbol, t.price);
                            }
                            _ => {}
                        }
                    }

                    events
                } else {
                    vec![MatchEvent::OrderRejected {
                        order_id: 0,
                        reason: RejectReason::SymbolNotFound,
                        timestamp: cmd.timestamp(),
                    }]
                };

                // 4. WAL post-trade write (if mode == Post)
                //
                // CRITICAL: If the write fails, the trade is already applied to the
                // book and audit log but NOT journaled. On crash, book-to-WAL
                // divergence occurs. We halt the affected symbol so operators can
                // investigate, and log at CRITICAL level. Continuing to match without
                // a working WAL would silently lose every subsequent trade.
                if !risk_rejected {
                    if let (Some(ref mut w), Some(wal_cmd)) = (&mut wal, wal_cmd_for_post) {
                        if let Err(e) = w.append(&wal_cmd) {
                            tracing::error!(
                                shard_id,
                                symbol = sym_name,
                                error = %e,
                                "WAL POST-WRITE FAILED — halting symbol to prevent silent data loss"
                            );
                            // Halt the symbol to prevent further un-journaled trades.
                            if let Some(engine) = engines.get_mut(&symbol) {
                                engine.halt_trading();
                            }
                        }
                    }
                }

                // 5. Async audit (non-blocking SPSC send)
                if let Some(ref a) = audit {
                    for event in &events {
                        if let Some(entry) = match_event_to_entry(event, symbol) {
                            a.log_entry(entry);
                        }
                    }
                }

                let duration = start.elapsed();

                // ── End hot path ────────────────────────────────────────

                if let Some(ref m) = metrics {
                    m.record_events(&events, sym_name, duration);
                    m.record_matching_duration(cmd_type_str, duration);
                    if let Some(engine) = engines.get(&symbol) {
                        m.update_book_state(engine.book(), sym_name);
                        m.set_stop_orders_pending(engine.stop_book().len() as i64, sym_name);
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

                commands_since_snapshot += 1;

                if group_commit {
                    // Defer the reply until the batch is fsynced. Commit when
                    // the batch is full or the inbound queue has drained (so a
                    // lone command under light load still acks immediately).
                    pending.push((reply, reply_order_id, events));
                    if pending.len() >= MAX_GROUP_BATCH || rx.is_empty() {
                        commit_and_flush(shard_id, &mut wal, &mut engines, &mut pending);
                        maybe_snapshot(
                            shard_id,
                            snapshot_interval,
                            &mut commands_since_snapshot,
                            &engines,
                            &mut wal,
                            &snapshot_mgr,
                        );
                    }
                } else {
                    if reply.send(events).is_err() {
                        // Caller timed out / dropped receiver. Events were
                        // applied to state (book updated, WAL written, audit
                        // logged) — risk of client-server inconsistency.
                        tracing::warn!(
                            shard_id,
                            symbol = sym_name,
                            "reply dropped: caller lost response AFTER state mutation"
                        );
                    }
                    maybe_snapshot(
                        shard_id,
                        snapshot_interval,
                        &mut commands_since_snapshot,
                        &engines,
                        &mut wal,
                        &snapshot_mgr,
                    );
                }
            }
            ShardMsg::QueryOrder {
                order_id,
                symbol,
                reply,
            } => {
                let events = if let Some(engine) = engines.get(&symbol) {
                    match engine.book().get_order(order_id) {
                        Some(order) => {
                            // Use cumulative/open quantities so an iceberg reports
                            // its true filled and open size, not the visible slice
                            // (remaining_qty is only the displayed portion).
                            vec![MatchEvent::OrderStatus {
                                order_id,
                                status: order.status as u8,
                                filled_qty: order.cumulative_qty,
                                remaining_qty: order.open_qty(),
                                price: order.price,
                                timestamp: order.timestamp,
                            }]
                        }
                        None => vec![MatchEvent::OrderNotFound { order_id }],
                    }
                } else {
                    vec![MatchEvent::OrderNotFound { order_id }]
                };
                if reply.send(events).is_err() {
                    tracing::warn!(
                        shard_id,
                        order_id,
                        symbol = ?symbol,
                        "QueryOrder reply dropped: caller lost response"
                    );
                }
            }
            ShardMsg::GetOpenOrdersBySession { session_id, reply } => {
                // Scan every engine owned by this shard; collect orders
                // matching session_id across all symbols.
                let mut collected = Vec::new();
                for engine in engines.values() {
                    collected.extend(engine.book().scan_by_session_id(&session_id));
                    collected.extend(engine.stop_book().scan_by_session_id(&session_id));
                }
                let collected_count = collected.len();
                if reply.send(collected).is_err() {
                    // Producer-side drop of snapshot data — this means the
                    // client's post-gap OpenOrders resync is INCOMPLETE for
                    // this shard's share of the session. Log at ERROR since
                    // it's a correctness gap in the exactly-once contract.
                    tracing::error!(
                        shard_id,
                        session_id = %session_id.log_prefix(),
                        lost_orders = collected_count,
                        "GetOpenOrdersBySession reply dropped: snapshot INCOMPLETE for this shard"
                    );
                }
            }
            ShardMsg::AddSymbol {
                config,
                risk_config,
            } => {
                let id = config.id;
                let name = format!("{}/{}", config.base, config.quote);
                symbol_names.insert(id, name);
                let stp = risk_config
                    .as_ref()
                    .map(|rc| rc.self_trade_prevention)
                    .unwrap_or(false);
                if let Some(rc) = risk_config {
                    risk.add_symbol(rc);
                }
                // Recover this symbol's state from the latest snapshot plus any
                // WAL records written after the snapshot watermark, before the
                // engine starts accepting live commands.
                let mut engine = Engine::new(config);
                engine.set_self_trade_prevention(stp);
                let open = recover_symbol(
                    shard_id,
                    id,
                    &mut engine,
                    &replay_entries,
                    snapshot_mgr.as_ref(),
                );
                if open > 0 {
                    risk.set_open_order_count(id, open);
                }
                engines.insert(id, engine);
            }
            ShardMsg::HaltSymbol(symbol) => {
                if let Some(engine) = engines.get_mut(&symbol) {
                    engine.halt_trading();
                    risk.halt_trading(symbol);
                    debug!(symbol = ?symbol, "trading halted");
                }
            }
            ShardMsg::ResumeSymbol(symbol) => {
                if let Some(engine) = engines.get_mut(&symbol) {
                    engine.resume_trading();
                    risk.resume_trading(symbol);
                    debug!(symbol = ?symbol, "trading resumed");
                }
            }
            ShardMsg::Shutdown => break,
        }
    }

    // Commit any deferred group-commit batch before exiting so in-flight
    // callers get their replies and the records are made durable.
    if !pending.is_empty() {
        commit_and_flush(shard_id, &mut wal, &mut engines, &mut pending);
    }

    // Flush WAL on shutdown
    if let Some(ref mut w) = wal {
        if let Err(e) = w.flush() {
            tracing::error!(
                shard_id,
                error = %e,
                "WAL flush failed on shutdown — last commands may be lost"
            );
        }
    }
}

/// Extract the order_id from any Command variant for traceability on errors.
#[inline]
fn cmd_order_id(cmd: &Command) -> OrderId {
    match cmd {
        Command::PlaceOrder(order) => order.id,
        Command::CancelOrder { order_id, .. } => *order_id,
        Command::AmendOrder { order_id, .. } => *order_id,
    }
}

// ─── WAL recovery / snapshotting (startup + periodic) ────────────────────────

/// Result of initializing the WAL and scanning prior records for replay.
struct WalInit {
    wal: Option<WalWriter>,
    replay_entries: Vec<WalEntry>,
    snapshot_mgr: Option<SnapshotManager>,
    wal_mode: WalMode,
}

/// Open the shard's WAL, crash-tolerantly scan any existing records (truncating
/// a torn tail), and prepare the snapshot manager. The scanned entries are
/// returned for per-symbol replay as engines are added.
fn init_wal_and_recover(shard_id: usize, wal_config: Option<WalConfig>) -> WalInit {
    let empty = |wal_mode| WalInit {
        wal: None,
        replay_entries: Vec::new(),
        snapshot_mgr: None,
        wal_mode,
    };

    let Some(cfg) = wal_config else {
        return empty(WalMode::Disabled);
    };
    if cfg.mode == WalMode::Disabled {
        return empty(WalMode::Disabled);
    }
    let wal_mode = cfg.mode.clone();

    let wal_path = format!("{}/shard_{}.wal", cfg.path, shard_id);
    if let Some(parent) = std::path::Path::new(&wal_path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::error!(
                shard_id,
                parent = %parent.display(),
                error = %e,
                "Failed to create WAL parent directory — WAL open will also fail"
            );
        }
    }

    // Crash-tolerant recovery scan of any existing WAL.
    let mut replay_entries = Vec::new();
    let mut resume_seq = 0u64;
    if std::path::Path::new(&wal_path).exists() {
        match WalReader::open(&wal_path) {
            Ok(mut reader) => {
                let result = reader.read_recoverable();
                if let Some(reason) = &result.truncated {
                    tracing::warn!(
                        shard_id,
                        reason = %reason,
                        valid_bytes = result.valid_bytes,
                        "WAL torn/corrupt tail; truncating to last good record"
                    );
                    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&wal_path) {
                        if let Err(e) = f.set_len(result.valid_bytes) {
                            tracing::error!(shard_id, error = %e, "WAL tail truncation failed");
                        }
                    }
                }
                resume_seq = result.entries.last().map_or(0, |e| e.sequence);
                replay_entries = result.entries;
                tracing::info!(
                    shard_id,
                    records = replay_entries.len(),
                    resume_seq,
                    "WAL recovery scan complete"
                );
            }
            Err(e) => tracing::error!(shard_id, error = %e, "Failed to open WAL for recovery"),
        }
    }

    let wal = match WalWriter::open(&wal_path, cfg.sync_interval) {
        Ok(mut w) => {
            w.set_sequence(resume_seq);
            debug!(shard_id, path = %wal_path, "WAL writer ready");
            Some(w)
        }
        Err(e) => {
            tracing::error!(shard_id, error = %e, "Failed to open WAL writer");
            None
        }
    };

    let snapshot_mgr = cfg
        .snapshot_dir
        .as_ref()
        .and_then(|dir| match SnapshotManager::new(dir) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::error!(shard_id, dir = %dir, error = %e, "Failed to init snapshot manager");
                None
            }
        });

    WalInit {
        wal,
        replay_entries,
        snapshot_mgr,
        wal_mode,
    }
}

/// Restore one symbol's engine from its latest snapshot, then replay WAL
/// records written after the snapshot watermark. Returns the resulting open
/// order count (resting + pending stop) so the risk engine can be re-seeded.
fn recover_symbol(
    shard_id: usize,
    id: SymbolId,
    engine: &mut Engine,
    replay_entries: &[WalEntry],
    snapshot_mgr: Option<&SnapshotManager>,
) -> u32 {
    let mut watermark = 0u64;
    if let Some(mgr) = snapshot_mgr {
        match mgr.load_latest(id) {
            Ok(Some(snap)) => {
                watermark = snap.wal_sequence;
                engine.apply_snapshot(snap);
                tracing::info!(shard_id, symbol = ?id, watermark, "restored snapshot");
            }
            Ok(None) => {}
            Err(e) => tracing::error!(
                shard_id,
                symbol = ?id,
                error = %e,
                "snapshot load failed; replaying full WAL for this symbol"
            ),
        }
    }

    let mut replayed = 0u64;
    for entry in replay_entries {
        if entry.sequence > watermark && entry.command.symbol() == id {
            engine.process(wal_to_command(&entry.command));
            replayed += 1;
        }
    }
    if replayed > 0 {
        tracing::info!(shard_id, symbol = ?id, replayed, "replayed WAL records");
    }

    (engine.book().order_count() + engine.stop_book().len()) as u32
}

/// Group-commit: fsync the WAL once for the whole pending batch, then release
/// all deferred replies. The Pre-mode durability contract holds because a
/// success reply is only sent AFTER its record is durable.
///
/// If the fsync fails the batch is NOT durable even though the books were
/// already mutated in memory. We fail every pending command with
/// `InternalError` (so clients retry rather than assume success) and halt all
/// engines on the shard so no further un-journaled trades occur until an
/// operator intervenes. On restart, recovery rebuilds state from the durable
/// WAL/snapshot only — consistent with the InternalError the clients saw.
fn commit_and_flush(
    shard_id: usize,
    wal: &mut Option<WalWriter>,
    engines: &mut FxHashMap<SymbolId, Engine>,
    pending: &mut Vec<PendingReply>,
) {
    if pending.is_empty() {
        return;
    }

    let durable = match wal.as_mut() {
        Some(w) => match w.sync() {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    shard_id,
                    error = %e,
                    batch = pending.len(),
                    "GROUP COMMIT FSYNC FAILED — failing batch and halting shard engines"
                );
                false
            }
        },
        None => true, // WAL disabled: nothing to fsync.
    };

    if durable {
        for (reply, _order_id, events) in pending.drain(..) {
            if reply.send(events).is_err() {
                tracing::warn!(shard_id, "reply dropped after durable commit");
            }
        }
    } else {
        for engine in engines.values_mut() {
            engine.halt_trading();
        }
        for (reply, order_id, _events) in pending.drain(..) {
            let _ = reply.send(vec![MatchEvent::OrderRejected {
                order_id,
                reason: RejectReason::InternalError,
                timestamp: 0,
            }]);
        }
    }
}

/// Periodic snapshot trigger shared by the group-commit and immediate-reply
/// paths. Snapshots all symbols and truncates the WAL once the command counter
/// reaches the configured interval.
fn maybe_snapshot(
    shard_id: usize,
    snapshot_interval: u64,
    commands_since_snapshot: &mut u64,
    engines: &FxHashMap<SymbolId, Engine>,
    wal: &mut Option<WalWriter>,
    snapshot_mgr: &Option<SnapshotManager>,
) {
    if snapshot_interval == 0 {
        return;
    }
    if let Some(mgr) = snapshot_mgr {
        if *commands_since_snapshot >= snapshot_interval {
            take_shard_snapshot(shard_id, engines, wal, mgr);
            *commands_since_snapshot = 0;
        }
    }
}

/// Snapshot every symbol on the shard at the current WAL watermark, then (only
/// if all snapshots succeeded) truncate the WAL. Truncating only on full
/// success avoids dropping un-snapshotted records for a symbol whose save failed.
fn take_shard_snapshot(
    shard_id: usize,
    engines: &FxHashMap<SymbolId, Engine>,
    wal: &mut Option<WalWriter>,
    mgr: &SnapshotManager,
) {
    let wal_seq = wal.as_ref().map_or(0, |w| w.sequence());
    let mut all_ok = true;
    for (id, engine) in engines {
        let mut snap = engine.snapshot();
        snap.wal_sequence = wal_seq;
        match mgr.save(&snap) {
            Ok(_) => {
                let _ = mgr.cleanup(*id, 3);
            }
            Err(e) => {
                tracing::error!(shard_id, symbol = ?id, error = %e, "snapshot save failed");
                all_ok = false;
            }
        }
    }
    if all_ok {
        if let Some(w) = wal.as_mut() {
            if let Err(e) = w.rotate_truncate() {
                tracing::error!(shard_id, error = %e, "WAL truncate after snapshot failed");
            }
        }
    }
}

/// Convert a persistence `WalCommand` back into an engine `Command` for replay.
fn wal_to_command(cmd: &WalCommand) -> Command {
    match cmd {
        WalCommand::PlaceOrder(order) => Command::PlaceOrder(*order),
        WalCommand::CancelOrder {
            order_id,
            symbol,
            timestamp,
        } => Command::CancelOrder {
            order_id: *order_id,
            symbol: *symbol,
            timestamp: *timestamp,
        },
        WalCommand::AmendOrder {
            order_id,
            symbol,
            new_price,
            new_qty,
            timestamp,
        } => Command::AmendOrder {
            order_id: *order_id,
            symbol: *symbol,
            new_price: *new_price,
            new_qty: *new_qty,
            timestamp: *timestamp,
        },
    }
}

/// Convert an engine `Command` to a persistence `WalCommand`.
fn command_to_wal(cmd: &Command) -> WalCommand {
    match cmd {
        Command::PlaceOrder(order) => WalCommand::PlaceOrder(*order),
        Command::CancelOrder {
            order_id,
            symbol,
            timestamp,
        } => WalCommand::CancelOrder {
            order_id: *order_id,
            symbol: *symbol,
            timestamp: *timestamp,
        },
        Command::AmendOrder {
            order_id,
            symbol,
            new_price,
            new_qty,
            timestamp,
        } => WalCommand::AmendOrder {
            order_id: *order_id,
            symbol: *symbol,
            new_price: *new_price,
            new_qty: *new_qty,
            timestamp: *timestamp,
        },
    }
}

// ─── Dead-shard tests ───────────────────────────────────────────────────────
//
// These tests intentionally poison the shard sender channel to simulate a
// crashed worker thread, then verify that the router returns
// RejectReason::InternalError (not SymbolNotFound) on all affected paths.
// Kept as unit tests (not integration tests) because they need access to
// the private `shards` field to kill a specific worker.

#[cfg(test)]
mod dead_shard_tests {
    use super::*;
    use me_core::events::RejectReason;
    use me_core::order::{Order, TimeInForce};
    use me_core::symbol::SymbolConfig;
    use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};

    /// Kill a shard by dropping its receiver end. Subsequent sends return
    /// `Err(SendError)`. Reuses the `Shutdown` path to terminate the worker
    /// thread cleanly, THEN replaces the tx with a closed-channel tx.
    fn kill_shard(router: &mut Router, shard_idx: usize) {
        // Send Shutdown; worker thread exits and drops its receiver.
        let _ = router.shards[shard_idx].tx.send(ShardMsg::Shutdown);
        if let Some(handle) = router.shards[shard_idx].handle.take() {
            let _ = handle.join();
        }
        // Replace tx with a tx whose receiver has been dropped — any send
        // on this tx returns Err, triggering the "shard dead" paths.
        let (dead_tx, rx) = crossbeam_channel::bounded::<ShardMsg>(1);
        drop(rx); // close the channel immediately
        router.shards[shard_idx].tx = dead_tx;
    }

    fn btc_cfg() -> SymbolConfig {
        SymbolConfig::default_btc_usdt()
    }

    fn place_order_cmd(id: u64, symbol: SymbolId) -> Command {
        Command::PlaceOrder(Order::new_limit(
            id,
            symbol,
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(1.0),
            FixedQty::from_f64(0.01),
            id * 1000,
        ))
    }

    fn place_stop_order_cmd(id: u64, symbol: SymbolId) -> Command {
        Command::PlaceOrder(Order::new_stop_limit(
            id,
            symbol,
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(1.1),
            FixedPrice::from_f64(1.05),
            FixedQty::from_f64(0.02),
            id * 1000,
        ))
    }

    #[test]
    fn process_on_dead_shard_returns_internal_error() {
        let mut router = Router::new(1);
        router.add_symbol(btc_cfg());
        // Verify the symbol is placed on shard 0 (only shard).
        kill_shard(&mut router, 0);

        let events = router.process(place_order_cmd(1, SymbolId(0)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            MatchEvent::OrderRejected {
                reason, order_id, ..
            } => {
                assert_eq!(*order_id, 1);
                assert_eq!(
                    *reason,
                    RejectReason::InternalError,
                    "dead shard MUST reject with InternalError, not SymbolNotFound"
                );
            }
            other => panic!("expected OrderRejected, got {other:?}"),
        }
    }

    #[test]
    fn query_order_on_dead_shard_returns_internal_error() {
        let mut router = Router::new(1);
        router.add_symbol(btc_cfg());
        kill_shard(&mut router, 0);

        let events = router.query_order(42, SymbolId(0));
        assert_eq!(events.len(), 1);
        match &events[0] {
            MatchEvent::OrderRejected {
                reason, order_id, ..
            } => {
                assert_eq!(*order_id, 42);
                assert_eq!(*reason, RejectReason::InternalError);
            }
            other => panic!("expected OrderRejected InternalError, got {other:?}"),
        }
    }

    #[test]
    fn halt_symbol_on_dead_shard_returns_false() {
        let mut router = Router::new(1);
        router.add_symbol(btc_cfg());
        kill_shard(&mut router, 0);

        let halted = router.halt_symbol(SymbolId(0));
        assert!(
            !halted,
            "halt_symbol MUST return false when shard is dead (circuit breaker failed)"
        );
    }

    #[test]
    fn resume_symbol_on_dead_shard_does_not_panic() {
        // resume_symbol logs and returns; no panic on dead shard.
        let mut router = Router::new(1);
        router.add_symbol(btc_cfg());
        kill_shard(&mut router, 0);

        router.resume_symbol(SymbolId(0));
        // If we got here without panicking, the dead-shard path is handled.
    }

    #[test]
    fn get_open_orders_by_session_returns_err_on_dead_shard() {
        // With dead shards, get_open_orders_by_session MUST return Err
        // rather than a partial/empty snapshot. Clients rely on the
        // snapshot being complete for ReplayGap resync; an incomplete
        // snapshot would cause silent order loss.
        let mut router = Router::new(4);
        router.add_symbol(btc_cfg());
        let session = me_core::SessionId::from_bytes([0xAAu8; 16]);

        let mut cmd = place_order_cmd(1, SymbolId(0));
        if let Command::PlaceOrder(ref mut o) = cmd {
            o.client_session_id = session;
        }
        router.process(cmd);

        // Kill ALL shards: snapshot must fail, not return empty.
        for i in 0..4 {
            kill_shard(&mut router, i);
        }
        let result = router.get_open_orders_by_session(session);
        assert!(
            result.is_err(),
            "all shards dead: must return Err, not partial snapshot"
        );
    }

    #[test]
    fn process_on_unknown_symbol_still_returns_symbol_not_found() {
        // Sanity check: the dead-shard fix must NOT change behavior for
        // legitimately unknown symbols.
        let mut router = Router::new(1);
        router.add_symbol(btc_cfg());
        // Don't kill any shard. Just use an unregistered symbol.
        let events = router.process(place_order_cmd(99, SymbolId(123)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            MatchEvent::OrderRejected { reason, .. } => {
                assert_eq!(
                    *reason,
                    RejectReason::SymbolNotFound,
                    "unknown symbol is still SymbolNotFound, not InternalError"
                );
            }
            other => panic!("expected OrderRejected SymbolNotFound, got {other:?}"),
        }
    }

    #[test]
    fn get_open_orders_by_session_includes_pending_stop_orders() {
        let mut router = Router::new(1);
        router.add_symbol(btc_cfg());
        let session = me_core::SessionId::from_bytes([0x55u8; 16]);

        let mut limit_cmd = place_order_cmd(1, SymbolId(0));
        if let Command::PlaceOrder(ref mut o) = limit_cmd {
            o.client_session_id = session;
        }
        router.process(limit_cmd);

        let mut stop_cmd = place_stop_order_cmd(2, SymbolId(0));
        if let Command::PlaceOrder(ref mut o) = stop_cmd {
            o.client_session_id = session;
        }
        router.process(stop_cmd);

        let snapshot = router.get_open_orders_by_session(session).unwrap();
        assert_eq!(
            snapshot.len(),
            2,
            "snapshot should include resting + stop orders"
        );
        assert!(snapshot.iter().any(|o| o.id == 1));
        assert!(snapshot.iter().any(|o| o.id == 2 && o.is_stop()));
    }
}
