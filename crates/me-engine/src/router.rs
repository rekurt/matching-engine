use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Upper bound on how long a caller waits for a shard to reply before treating
/// the command as failed. Bounds the blast radius of a wedged shard (e.g. a
/// stalled disk during WAL fsync) so it cannot hang every client thread.
const SHARD_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Persist the deterministic decision and fsync before replying.
    Pre,
    /// Compatibility alias for durable decision journaling.
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
    /// retaining the decision journal. `0` disables periodic snapshots.
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
/// Commands pass risk checks and matching, then durable decision commit, then audit.
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

#[derive(Clone, Debug)]
pub struct DurableRequest {
    pub session_id: me_core::SessionId,
    pub request_id: u64,
    pub item_index: u32,
    pub events: Vec<MatchEvent>,
    pub owners: Vec<(OrderId, me_core::SessionId)>,
    pub expiration_count: usize,
    pub expiration_owners: Vec<(OrderId, me_core::SessionId)>,
}

enum ShardMsg {
    RequestLookup {
        key: (me_core::SessionId, u64, u32),
        reply: Sender<Option<DurableRequest>>,
    },
    Requests {
        reply: Sender<Vec<DurableRequest>>,
    },
    Health {
        reply: Sender<()>,
    },
    States {
        reply: Sender<Vec<(SymbolId, bool)>>,
    },
    Command {
        cmd: Command,
        request_id: Option<u64>,
        item_index: u32,
        /// Session that issued the command, used to enforce order ownership on
        /// cancel/amend. `SessionId::NONE` is a privileged/internal caller.
        requesting_session: me_core::SessionId,
        reply: Sender<Vec<MatchEvent>>,
    },
    QueryOrder {
        order_id: OrderId,
        symbol: SymbolId,
        /// Session that issued the query, used to enforce order ownership.
        /// `SessionId::NONE` is a privileged/internal caller.
        requesting_session: me_core::SessionId,
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
        assert!(num_shards > 0, "router requires at least one shard");
        if let Some(cfg) = wal_config
            .as_ref()
            .filter(|cfg| cfg.mode != WalMode::Disabled)
        {
            if let Err(e) = check_topology(&cfg.path, num_shards) {
                panic!("durable router topology validation failed: {e}");
            }
        }
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
        assert!(
            !self.symbol_to_shard.contains_key(&id),
            "symbol {id:?} is already registered"
        );
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

    /// Process a command synchronously as a privileged/internal caller — no
    /// per-session ownership enforcement on cancel/amend.
    pub fn process(&self, cmd: Command) -> Vec<MatchEvent> {
        self.process_with_session(cmd, me_core::SessionId::NONE)
    }

    /// Process a command synchronously on behalf of `requesting_session`.
    /// Cancel/amend on an order owned by a different session is rejected with
    /// `OrderNotFound` inside the engine (see `Engine::process_with_session`).
    pub fn process_with_session(
        &self,
        cmd: Command,
        requesting_session: me_core::SessionId,
    ) -> Vec<MatchEvent> {
        self.process_request(cmd, requesting_session, None, 0)
    }

    pub fn process_with_request(
        &self,
        cmd: Command,
        session: me_core::SessionId,
        request_id: u64,
    ) -> Vec<MatchEvent> {
        self.process_request(cmd, session, Some(request_id), 0)
    }

    pub fn process_batch_item(
        &self,
        cmd: Command,
        session: me_core::SessionId,
        request_id: u64,
        item_index: u32,
    ) -> Vec<MatchEvent> {
        self.process_with_request_item(cmd, session, request_id, item_index)
    }

    pub fn symbol_states(&self) -> Result<Vec<(SymbolId, bool)>, String> {
        let mut states = Vec::new();
        for shard in &self.shards {
            let (tx, rx) = bounded(1);
            shard
                .tx
                .send_timeout(ShardMsg::States { reply: tx }, SHARD_REPLY_TIMEOUT)
                .map_err(|e| e.to_string())?;
            states.extend(
                rx.recv_timeout(SHARD_REPLY_TIMEOUT)
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(states)
    }

    pub fn process_with_request_item(
        &self,
        cmd: Command,
        session: me_core::SessionId,
        request_id: u64,
        item_index: u32,
    ) -> Vec<MatchEvent> {
        self.process_request(cmd, session, Some(request_id), item_index)
    }

    pub fn recovered_requests(&self) -> Result<Vec<DurableRequest>, String> {
        let mut records = Vec::new();
        for shard in &self.shards {
            let (tx, rx) = bounded(1);
            shard
                .tx
                .send_timeout(ShardMsg::Requests { reply: tx }, SHARD_REPLY_TIMEOUT)
                .map_err(|e| e.to_string())?;
            records.extend(
                rx.recv_timeout(SHARD_REPLY_TIMEOUT)
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(records)
    }

    pub fn request_record(
        &self,
        session: me_core::SessionId,
        request_id: u64,
    ) -> Result<Option<DurableRequest>, String> {
        self.request_record_item(session, request_id, 0)
    }

    pub fn request_record_item(
        &self,
        session: me_core::SessionId,
        request_id: u64,
        item_index: u32,
    ) -> Result<Option<DurableRequest>, String> {
        for shard in &self.shards {
            let (tx, rx) = bounded(1);
            shard
                .tx
                .send_timeout(
                    ShardMsg::RequestLookup {
                        key: (session, request_id, item_index),
                        reply: tx,
                    },
                    SHARD_REPLY_TIMEOUT,
                )
                .map_err(|e| e.to_string())?;
            if let Some(record) = rx
                .recv_timeout(SHARD_REPLY_TIMEOUT)
                .map_err(|e| e.to_string())?
            {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    pub fn health(&self) -> Result<(), String> {
        for shard in &self.shards {
            let (tx, rx) = bounded(1);
            shard
                .tx
                .send_timeout(ShardMsg::Health { reply: tx }, SHARD_REPLY_TIMEOUT)
                .map_err(|e| e.to_string())?;
            rx.recv_timeout(SHARD_REPLY_TIMEOUT)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn process_request(
        &self,
        cmd: Command,
        requesting_session: me_core::SessionId,
        request_id: Option<u64>,
        item_index: u32,
    ) -> Vec<MatchEvent> {
        let symbol = cmd.symbol();
        let ts = cmd.timestamp();
        let order_id = cmd_order_id(&cmd);
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None if request_id.is_some() => (symbol.0 as usize) % self.num_shards,
            None => {
                return vec![MatchEvent::OrderRejected {
                    order_id,
                    reason: RejectReason::SymbolNotFound,
                    timestamp: ts,
                }];
            }
        };

        let (reply_tx, reply_rx) = bounded(1);
        if let Some(metrics) = &self.metrics {
            metrics.set_shard_queue_depth(shard_idx, self.shards[shard_idx].tx.len() as i64);
        }
        if self.shards[shard_idx]
            .tx
            .send_timeout(
                ShardMsg::Command {
                    cmd,
                    requesting_session,
                    request_id,
                    item_index,
                    reply: reply_tx,
                },
                SHARD_REPLY_TIMEOUT,
            )
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
        if let Some(metrics) = &self.metrics {
            metrics.set_shard_queue_depth(shard_idx, self.shards[shard_idx].tx.len() as i64);
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
        if let Err(e) = self.shards[shard_idx]
            .tx
            .send_timeout(ShardMsg::HaltSymbol(symbol), SHARD_REPLY_TIMEOUT)
        {
            tracing::error!(
                shard_idx,
                symbol = ?symbol,
                error = %e,
                "CIRCUIT BREAKER FAILED: halt_symbol could not dispatch (shard dead)"
            );
            return false;
        }
        self.health().is_ok()
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
            .send_timeout(ShardMsg::ResumeSymbol(symbol), SHARD_REPLY_TIMEOUT)
        {
            tracing::error!(
                shard_idx,
                symbol = ?symbol,
                error = %e,
                "resume_symbol dispatch failed (shard dead)"
            );
            return false;
        }
        self.health().is_ok()
    }

    /// Query order status as a privileged/internal caller — no ownership check.
    pub fn query_order(&self, order_id: OrderId, symbol: SymbolId) -> Vec<MatchEvent> {
        self.query_order_for_session(order_id, symbol, me_core::SessionId::NONE)
    }

    /// Query order status on behalf of `requesting_session`. A query for an
    /// order owned by another session returns `OrderNotFound`, preventing
    /// enumeration of other clients' orders by id.
    pub fn query_order_for_session(
        &self,
        order_id: OrderId,
        symbol: SymbolId,
        requesting_session: me_core::SessionId,
    ) -> Vec<MatchEvent> {
        let shard_idx = match self.symbol_to_shard.get(&symbol) {
            Some(&idx) => idx,
            None => return vec![MatchEvent::OrderNotFound { order_id }],
        };

        let (reply_tx, reply_rx) = bounded(1);
        if let Some(metrics) = &self.metrics {
            metrics.set_shard_queue_depth(shard_idx, self.shards[shard_idx].tx.len() as i64);
        }
        if self.shards[shard_idx]
            .tx
            .send_timeout(
                ShardMsg::QueryOrder {
                    order_id,
                    symbol,
                    requesting_session,
                    reply: reply_tx,
                },
                SHARD_REPLY_TIMEOUT,
            )
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
        if let Some(metrics) = &self.metrics {
            metrics.set_shard_queue_depth(shard_idx, self.shards[shard_idx].tx.len() as i64);
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
                .send_timeout(
                    ShardMsg::GetOpenOrdersBySession {
                        session_id,
                        reply: tx,
                    },
                    SHARD_REPLY_TIMEOUT,
                )
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
            match rx.recv_timeout(SHARD_REPLY_TIMEOUT) {
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

fn check_topology(path: &str, shards: usize) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let dir = std::path::Path::new(path);
    me_persist::create_dir_all_durable(dir)?;
    let manifest = dir.join("topology");
    let expected = format!("decision-journal-v1\nshards={shards}\n");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest)
    {
        Ok(mut file) => {
            file.write_all(expected.as_bytes())?;
            file.sync_all()?;
            std::fs::File::open(dir)?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut actual = String::new();
            std::fs::File::open(manifest)?.read_to_string(&mut actual)?;
            if actual != expected {
                return Err(std::io::Error::other("journal topology mismatch"));
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
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

    let wal_pre_open_failed = wal_mode != WalMode::Disabled && wal.is_none();
    if wal_pre_open_failed {
        tracing::error!(
            shard_id,
            "WAL configured but unavailable; terminating shard"
        );
    }
    if wal_pre_open_failed {
        return;
    }
    let recovered_requests: Vec<DurableRequest> = replay_entries
        .iter()
        .filter_map(|entry| {
            if let WalCommand::Decision {
                session_id,
                request_id: Some(request_id),
                item_index,
                events,
                expirations,
                expiration_owners,
                owners,
                ..
            } = &entry.command
            {
                Some(DurableRequest {
                    session_id: *session_id,
                    request_id: *request_id,
                    item_index: *item_index,
                    events: expirations.iter().chain(events).cloned().collect(),
                    owners: owners.clone(),
                    expiration_count: expirations.len(),
                    expiration_owners: expiration_owners.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    let mut request_order: Vec<_> = recovered_requests
        .iter()
        .map(|r| (r.session_id, r.request_id, r.item_index))
        .collect();
    let mut requests: FxHashMap<_, _> = recovered_requests
        .into_iter()
        .map(|r| ((r.session_id, r.request_id, r.item_index), r))
        .collect();
    let mut owner_index: FxHashMap<(SymbolId, OrderId), me_core::SessionId> = FxHashMap::default();
    let registered_symbols: FxHashSet<_> = replay_entries
        .iter()
        .filter_map(|entry| match entry.command {
            WalCommand::SymbolConfig { symbol, .. } => Some(symbol),
            _ => None,
        })
        .collect();
    let mut commands_since_snapshot: u64 = 0;

    while let Ok(msg) = rx.recv() {
        if let Some(metrics) = &metrics {
            metrics.set_shard_queue_depth(shard_id, rx.len() as i64);
        }
        match msg {
            ShardMsg::RequestLookup { key, reply } => {
                let _ = reply.send(requests.get(&key).cloned());
            }
            ShardMsg::States { reply } => {
                let _ = reply.send(
                    engines
                        .iter()
                        .map(|(id, engine)| {
                            (
                                *id,
                                engine.trading_state() == crate::engine::TradingState::Active,
                            )
                        })
                        .collect(),
                );
            }
            ShardMsg::Health { reply } => {
                if registered_symbols
                    .iter()
                    .all(|symbol| engines.contains_key(symbol))
                {
                    let _ = reply.send(());
                }
            }
            ShardMsg::Requests { reply } => {
                let _ = reply.send(
                    request_order
                        .iter()
                        .filter_map(|key| requests.get(key).cloned())
                        .collect(),
                );
            }
            ShardMsg::Command {
                cmd,
                requesting_session,
                request_id,
                item_index,
                reply,
            } => {
                if let Some(id) = request_id {
                    if let Some(record) = requests.get(&(requesting_session, id, item_index)) {
                        let _ = reply.send(record.events.clone());
                        continue;
                    }
                }
                let incoming_owner = match &cmd {
                    Command::PlaceOrder(o) => Some((o.id, o.client_session_id)),
                    _ => None,
                };
                let reply_order_id = cmd_order_id(&cmd);
                let journal_command = command_to_wal(&cmd);
                let processing_timestamp = me_clock::Clock::now_ns(&me_clock::SystemClock);
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

                trace!(symbol = sym_name, cmd_type = cmd_type_str, "processing");

                // Risk → matching → durable decision → audit.

                let start = std::time::Instant::now();

                let expirations = engines
                    .get_mut(&symbol)
                    .map_or_else(Vec::new, Engine::expire_gtd_now);
                let expiration_owners: Vec<_> = expirations
                    .iter()
                    .filter_map(|event| match event {
                        MatchEvent::OrderCancelled { order_id, .. } => owner_index
                            .get(&(symbol, *order_id))
                            .map(|owner| (*order_id, *owner)),
                        _ => None,
                    })
                    .collect();
                for event in &expirations {
                    if matches!(event, MatchEvent::OrderCancelled { .. }) {
                        risk.on_order_removed(symbol);
                    }
                }

                // 1. Pre-trade risk check (PlaceOrder only)
                let risk_rejected = {
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
                        Command::AmendOrder {
                            order_id,
                            new_price,
                            new_qty,
                            ..
                        } => engines
                            .get(&symbol)
                            .filter(|engine| {
                                engine.is_mutation_authorized(*order_id, requesting_session)
                            })
                            .and_then(|engine| {
                                engine.amended_order(*order_id, *new_price, *new_qty).ok()
                            })
                            .is_some_and(|order| risk.check_amend(&order).is_err()),
                        _ => false,
                    }
                };

                let events = if risk_rejected {
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
                        reason: RejectReason::RiskCheckFailed,
                        timestamp: ts,
                    }]
                } else if let Some(engine) = engines.get_mut(&symbol) {
                    // 2. Matching (GTD expiry was already swept above, before the
                    // risk check, so expired orders never count against limits).
                    // Ownership of cancel/amend is enforced here against the
                    // issuing session.
                    engine.set_risk_config(risk.config(symbol).cloned());
                    let events = engine
                        .process_with_session_at(cmd, requesting_session, processing_timestamp)
                        .to_vec();

                    for event in &events {
                        if let MatchEvent::Trade(trade) = event {
                            risk.update_reference_price(symbol, trade.price);
                        }
                    }

                    events
                } else {
                    vec![MatchEvent::OrderRejected {
                        order_id: reply_order_id,
                        reason: RejectReason::SymbolNotFound,
                        timestamp: cmd.timestamp(),
                    }]
                };

                if let Some(engine) = engines.get(&symbol) {
                    risk.set_open_order_count(
                        symbol,
                        (engine.book().order_count() + engine.stop_book().len())
                            .try_into()
                            .unwrap_or(u32::MAX),
                    );
                }
                let mut affected = FxHashSet::default();
                for event in expirations.iter().chain(&events) {
                    match event {
                        MatchEvent::Trade(t) => {
                            affected.insert(t.maker_order_id);
                            affected.insert(t.taker_order_id);
                        }
                        MatchEvent::OrderAccepted { order_id, .. }
                        | MatchEvent::OrderRejected { order_id, .. }
                        | MatchEvent::OrderPartiallyFilled { order_id, .. }
                        | MatchEvent::OrderFilled { order_id, .. }
                        | MatchEvent::OrderCancelled { order_id, .. }
                        | MatchEvent::OrderAmended { order_id, .. }
                        | MatchEvent::StopTriggered { order_id, .. }
                        | MatchEvent::OrderStatus { order_id, .. }
                        | MatchEvent::OrderNotFound { order_id } => {
                            affected.insert(*order_id);
                        }
                    }
                }
                if let Some((id, owner)) = incoming_owner {
                    if events.iter().any(|event| matches!(event, MatchEvent::OrderAccepted { order_id, .. } if *order_id == id)) {
                        owner_index.insert((symbol, id), owner);
                    }
                }
                let owners: Vec<_> = affected
                    .iter()
                    .filter_map(|id| owner_index.get(&(symbol, *id)).map(|owner| (*id, *owner)))
                    .collect();
                for id in &affected {
                    let owner = engines
                        .get(&symbol)
                        .and_then(|engine| {
                            engine
                                .book()
                                .get_order(*id)
                                .or_else(|| engine.stop_book().get(*id))
                        })
                        .map(|order| order.client_session_id);
                    if let Some(owner) = owner {
                        owner_index.insert((symbol, *id), owner);
                    } else {
                        owner_index.remove(&(symbol, *id));
                    }
                }
                if let Some(w) = wal.as_mut() {
                    let apply = !risk_rejected && engines.contains_key(&symbol);
                    let decision = WalCommand::Decision {
                        command: Box::new(journal_command),
                        session_id: requesting_session,
                        request_id,
                        item_index,
                        events: events.clone(),
                        expirations: expirations.clone(),
                        expiration_owners: expiration_owners.clone(),
                        owners: owners.clone(),
                        processing_timestamp,
                        apply,
                    };
                    if let Err(e) = w
                        .append_nosync(&decision)
                        .and_then(|_| sync_wal(w, metrics.as_deref()))
                    {
                        tracing::error!(shard_id, error = %e, "decision journal failed; terminating shard");
                        return;
                    }
                }
                let expiration_count = expirations.len();
                let events: Vec<_> = expirations.into_iter().chain(events).collect();
                if let Some(request_id) = request_id {
                    let key = (requesting_session, request_id, item_index);
                    request_order.push(key);
                    requests.insert(
                        key,
                        DurableRequest {
                            session_id: requesting_session,
                            request_id,
                            item_index,
                            events: events.clone(),
                            owners,
                            expiration_count,
                            expiration_owners,
                        },
                    );
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

                if reply.send(events).is_err() {
                    tracing::warn!(shard_id, "reply dropped after decision commit");
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

            ShardMsg::QueryOrder {
                order_id,
                symbol,
                requesting_session,
                reply,
            } => {
                let events = if let Some(engine) = engines.get(&symbol) {
                    // Look in the live book first, then the stop book: a pending
                    // stop is an accepted, resting order (OpenOrders already
                    // reports it via stop_book), so GetOrderStatus must too rather
                    // than claiming it vanished until it triggers or cancels.
                    let found = engine
                        .book()
                        .get_order(order_id)
                        .or_else(|| engine.stop_book().get(order_id));
                    // Enforce ownership: a client may only query its own orders.
                    // A real session querying another session's order gets
                    // OrderNotFound (no enumeration). NONE (internal/admin) and
                    // NONE-owned orders are exempt, mirroring cancel/amend.
                    let owned = match found {
                        Some(order) => {
                            requesting_session.is_none()
                                || order.client_session_id.is_none()
                                || order.client_session_id == requesting_session
                        }
                        None => true,
                    };
                    match found.filter(|_| owned) {
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
                let fingerprint = format!("{config:?}|{risk_config:?}");
                let saved = replay_entries
                    .iter()
                    .find_map(|entry| match &entry.command {
                        WalCommand::SymbolConfig {
                            symbol,
                            fingerprint,
                        } if *symbol == id => Some(fingerprint),
                        _ => None,
                    });
                if saved.is_some_and(|previous| previous != &fingerprint) {
                    tracing::error!(
                        shard_id,
                        "symbol/risk configuration changed; recovery refused"
                    );
                    return;
                }
                let name = format!("{}/{}", config.base, config.quote);
                symbol_names.insert(id, name);
                let stp = risk_config
                    .as_ref()
                    .map(|rc| rc.self_trade_prevention)
                    .unwrap_or(false);
                if let Some(rc) = risk_config.clone() {
                    risk.add_symbol(rc);
                }
                // Replay authoritative decisions before accepting live commands.
                let mut engine = Engine::new(config);
                engine.set_self_trade_prevention(stp);
                engine.set_risk_config(risk_config);
                let open = match recover_symbol(
                    shard_id,
                    id,
                    &mut engine,
                    &replay_entries,
                    snapshot_mgr.as_ref(),
                    &mut risk,
                ) {
                    Ok(open) => open,
                    Err(e) => {
                        tracing::error!(error = %e, "symbol recovery failed");
                        return;
                    }
                };
                if saved.is_none() {
                    if let Some(w) = wal.as_mut() {
                        if w.append_nosync(&WalCommand::SymbolConfig {
                            symbol: id,
                            fingerprint,
                        })
                        .and_then(|_| sync_wal(w, metrics.as_deref()))
                        .is_err()
                        {
                            return;
                        }
                    }
                }
                if open > 0 {
                    risk.set_open_order_count(id, open);
                }
                let snapshot = engine.snapshot();
                for order in snapshot.resting_orders.iter().chain(&snapshot.stop_orders) {
                    owner_index.insert((id, order.id), order.client_session_id);
                }
                engines.insert(id, engine);
            }
            ShardMsg::HaltSymbol(symbol) => {
                if let Some(w) = wal.as_mut() {
                    if w.append_nosync(&WalCommand::TradingState {
                        symbol,
                        halted: true,
                    })
                    .and_then(|_| sync_wal(w, metrics.as_deref()))
                    .is_err()
                    {
                        return;
                    }
                }
                if let Some(engine) = engines.get_mut(&symbol) {
                    engine.halt_trading();
                    risk.halt_trading(symbol);
                    debug!(symbol = ?symbol, "trading halted");
                }
            }
            ShardMsg::ResumeSymbol(symbol) => {
                if let Some(w) = wal.as_mut() {
                    if w.append_nosync(&WalCommand::TradingState {
                        symbol,
                        halted: false,
                    })
                    .and_then(|_| sync_wal(w, metrics.as_deref()))
                    .is_err()
                    {
                        return;
                    }
                }
                if let Some(engine) = engines.get_mut(&symbol) {
                    engine.resume_trading();
                    risk.resume_trading(symbol);
                    debug!(symbol = ?symbol, "trading resumed");
                }
            }
            ShardMsg::Shutdown => break,
        }
    }

    // Flush WAL on shutdown
    if let Some(ref mut w) = wal {
        if let Err(e) = sync_wal(w, metrics.as_deref()) {
            tracing::error!(
                shard_id,
                error = %e,
                "WAL flush failed on shutdown — last commands may be lost"
            );
        }
    }
}

fn sync_wal(wal: &mut WalWriter, metrics: Option<&EngineMetrics>) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    let result = wal.sync();
    if let Some(metrics) = metrics {
        metrics.record_wal_flush_duration(start.elapsed());
    }
    result
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

/// Strictly validate the journal, preserving damaged files for operator recovery.
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
        if let Err(e) = me_persist::create_dir_all_durable(parent) {
            tracing::error!(
                shard_id,
                parent = %parent.display(),
                error = %e,
                "Failed to create WAL parent directory — WAL open will also fail"
            );
        }
    }

    let writer = match WalWriter::open(&wal_path, cfg.sync_interval) {
        Ok(writer) => writer,
        Err(e) => {
            tracing::error!(error = %e, "WAL lock/open failed");
            return empty(wal_mode);
        }
    };
    let mut replay_entries = Vec::new();
    match WalReader::open(&wal_path) {
        Ok(mut reader) => match reader.read_all() {
            Ok(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    if entry.sequence != index as u64 + 1 {
                        tracing::error!(
                            shard_id,
                            "noncontiguous or compacted WAL; recovery refused"
                        );
                        return empty(wal_mode);
                    }
                }
                replay_entries = entries;
            }
            Err(e) => {
                tracing::error!(error = %e, "WAL recovery refused; damaged journal retained");
                return empty(wal_mode);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::error!(error = %e, "WAL recovery failed");
            return empty(wal_mode);
        }
    }

    let wal = Some(writer);

    let snapshot_mgr = match cfg.snapshot_dir.as_ref() {
        Some(dir) => match SnapshotManager::new(dir) {
            Ok(mgr) => Some(mgr),
            Err(e) => {
                tracing::error!(error = %e, "snapshot directory unavailable");
                return empty(wal_mode);
            }
        },
        None => None,
    };

    WalInit {
        wal,
        replay_entries,
        snapshot_mgr,
        wal_mode,
    }
}

/// Rebuild a symbol from its full decision history and validate replayed events.
fn recover_symbol(
    _shard_id: usize,
    id: SymbolId,
    engine: &mut Engine,
    replay_entries: &[WalEntry],
    snapshot_mgr: Option<&SnapshotManager>,
    risk: &mut RiskEngine,
) -> std::io::Result<u32> {
    let mut watermark = 0;
    if let Some(mgr) = snapshot_mgr {
        if let Some(snap) = match mgr.load_latest(id) {
            Ok(snapshot) => snapshot,
            Err(e)
                if replay_entries
                    .first()
                    .is_some_and(|entry| entry.sequence == 1) =>
            {
                tracing::warn!(error = %e, "damaged checkpoint ignored; replaying complete decision journal");
                None
            }
            Err(e) => return Err(e),
        } {
            if replay_entries
                .first()
                .is_none_or(|entry| entry.sequence != 1)
            {
                // Legacy compacted journals have no complete decision history.
                return Err(std::io::Error::other(
                    "compacted legacy journal requires explicit migration",
                ));
            }
            watermark = snap.wal_sequence;
        }
    }
    if watermark > replay_entries.last().map_or(0, |e| e.sequence) {
        return Err(std::io::Error::other("snapshot ahead of journal"));
    }
    for entry in replay_entries
        .iter()
        .filter(|entry| entry.command.symbol() == id)
    {
        match &entry.command {
            WalCommand::Decision {
                command,
                session_id,
                events,
                expirations,
                processing_timestamp,
                apply,
                ..
            } => {
                for expiration in expirations {
                    if let MatchEvent::OrderCancelled {
                        order_id,
                        timestamp,
                        ..
                    } = expiration
                    {
                        let actual = engine.process(Command::CancelOrder {
                            order_id: *order_id,
                            symbol: id,
                            timestamp: *timestamp,
                        });
                        if actual != std::slice::from_ref(expiration) {
                            return Err(std::io::Error::other("expiry replay divergence"));
                        }
                    } else {
                        return Err(std::io::Error::other("invalid expiry decision"));
                    }
                }
                if *apply {
                    engine.set_risk_config(risk.config(id).cloned());
                    let actual = engine.process_with_session_at(
                        wal_to_command(command),
                        *session_id,
                        *processing_timestamp,
                    );
                    if actual != events {
                        return Err(std::io::Error::other(format!(
                            "replay divergence at {}",
                            entry.sequence
                        )));
                    }
                }
                for event in events {
                    if let MatchEvent::Trade(t) = event {
                        risk.update_reference_price(id, t.price);
                    }
                }
            }
            WalCommand::TradingState { halted, .. } => {
                if *halted {
                    engine.halt_trading();
                    risk.halt_trading(id);
                } else {
                    engine.resume_trading();
                    risk.resume_trading(id);
                }
            }
            WalCommand::SymbolConfig { .. } => {}
            // Only synthetic expiry cancels are permitted outside decision records.
            WalCommand::CancelOrder { .. } => {
                engine.process(wal_to_command(&entry.command));
            }
            _ => {
                return Err(std::io::Error::other(
                    "legacy undecided WAL requires explicit migration",
                ))
            }
        }
    }
    Ok((engine.book().order_count() + engine.stop_book().len()) as u32)
}

/// Periodically save a checkpoint without deleting the decision history.
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

/// Snapshot every registered symbol at the current journal watermark.
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
    if !all_ok {
        tracing::error!(
            shard_id,
            "checkpoint incomplete; full decision journal retained"
        );
    }
}

/// Convert a persistence `WalCommand` back into an engine `Command` for replay.
fn wal_to_command(cmd: &WalCommand) -> Command {
    match cmd {
        WalCommand::Decision { command, .. } => wal_to_command(command),
        WalCommand::TradingState { .. } | WalCommand::SymbolConfig { .. } => {
            panic!("control records are not commands")
        }
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
