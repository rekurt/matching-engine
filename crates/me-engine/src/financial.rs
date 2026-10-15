//! Single-writer funded engine. Every visible decision has already reached stable storage.
use crate::{command::Command, engine::Engine};
use me_core::{
    events::MatchEvent,
    order::{Order, OrderStatus, OrderType, TimeInForce},
    symbol::SymbolConfig,
    types::{FixedPrice, FixedQty, SymbolId},
    SessionId,
};
use me_financial::*;
use me_persist::financial::FinancialJournal;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    storage_version: u32,
    engine_id: Uuid,
    definition_hash: String,
    symbols: Vec<SymbolConfig>,
}
#[derive(Clone, Serialize, Deserialize)]
enum Action {
    Command(FinancialCommand),
    Tick,
    Expire(u16),
    Maintenance(bool),
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    action: Action,
    timestamp: u64,
    decision: Decision,
    events: Vec<MatchEvent>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Metadata {
    account_id: Uuid,
    reservation_id: Uuid,
    fee_schedule: Uuid,
    version: i64,
    spec: OrderSpec,
    status: String,
}

pub struct FinancialEngine {
    manifest: Manifest,
    engines: BTreeMap<u16, Engine>,
    orders: BTreeMap<i64, Metadata>,
    requests: BTreeMap<Uuid, (String, usize)>,
    decisions: Vec<Decision>,
    journal: FinancialJournal<Record>,
    maintenance: bool,
    failed: bool,
    failure_retryable: bool,
    snapshot_path: PathBuf,
}
impl FinancialEngine {
    pub fn open(directory: impl AsRef<Path>, symbols: Vec<SymbolConfig>) -> io::Result<Self> {
        Self::open_with_init_hook(directory, symbols, |_| Ok(()))
    }
    fn open_with_init_hook(
        directory: impl AsRef<Path>,
        mut symbols: Vec<SymbolConfig>,
        mut init_hook: impl FnMut(&str) -> io::Result<()>,
    ) -> io::Result<Self> {
        if symbols.is_empty() {
            return Err(invalid("no financial instruments"));
        }
        symbols.sort_by_key(|s| s.id.0);
        if symbols.windows(2).any(|p| p[0].id == p[1].id) {
            return Err(invalid("duplicate financial symbol"));
        }
        me_persist::create_dir_all_durable(directory.as_ref())?;
        let active = directory.as_ref().join("active");
        let definition_hash = fingerprint(&symbols).map_err(invalid)?;
        if !active.exists() {
            for entry in std::fs::read_dir(directory.as_ref())? {
                if !entry?
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".initializing-")
                {
                    return Err(invalid(
                        "financial mode requires a new directory or a published generation",
                    ));
                }
            }
            if directory.as_ref().join("financial.json").exists()
                || directory.as_ref().join("financial.wal").exists()
            {
                return Err(invalid(
                    "unversioned financial directory; explicit migration required",
                ));
            }
            let staging = directory
                .as_ref()
                .join(format!(".initializing-{}", Uuid::new_v4()));
            std::fs::create_dir(&staging)?;
            let manifest = Manifest {
                storage_version: STORAGE_VERSION,
                engine_id: Uuid::new_v4(),
                definition_hash: definition_hash.clone(),
                symbols: symbols.clone(),
            };
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(staging.join("financial.json"))?;
            file.write_all(&serde_json::to_vec(&manifest).map_err(invalid)?)?;
            file.sync_all()?;
            init_hook("manifest")?;
            let (journal, _) = FinancialJournal::<Record>::open(staging.join("financial.wal"))?;
            journal.sync()?;
            drop(journal);
            File::open(&staging)?.sync_all()?;
            init_hook("wal")?;
            if let Err(e) = std::fs::rename(&staging, &active) {
                if !active.exists() {
                    return Err(e);
                }
                std::fs::remove_dir_all(&staging)?;
            }
            File::open(directory.as_ref())?.sync_all()?;
        }
        let manifest_path = active.join("financial.json");
        let journal_path = active.join("financial.wal");
        if !manifest_path.exists() || !journal_path.exists() {
            return Err(invalid("published financial generation is incomplete"));
        }
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(manifest_path)?).map_err(invalid)?;
        if manifest.storage_version != STORAGE_VERSION
            || manifest.definition_hash != definition_hash
            || fingerprint(&manifest.symbols).map_err(invalid)? != definition_hash
        {
            return Err(invalid("incompatible financial storage or instruments"));
        }
        let (journal, records) = FinancialJournal::<Record>::open(&journal_path)?;
        let snapshot_path = active.join("financial.snapshot.json");
        let saved_snapshot: Option<serde_json::Value> = if snapshot_path.exists() {
            let envelope: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&snapshot_path)?).map_err(invalid)?;
            let payload = envelope
                .get("payload")
                .ok_or_else(|| invalid("missing financial snapshot payload"))?;
            if envelope.get("digest").and_then(|v| v.as_str())
                != Some(fingerprint(payload).map_err(invalid)?.as_str())
            {
                return Err(invalid("financial snapshot checksum mismatch"));
            }
            let sequence = payload["watermark"]["sequence"]
                .as_u64()
                .ok_or_else(|| invalid("invalid snapshot watermark"))?;
            if sequence == 0 || sequence > records.len() as u64 {
                return Err(invalid("financial snapshot is ahead of journal"));
            }
            Some(payload.clone())
        } else {
            None
        };
        let engines = symbols
            .into_iter()
            .map(|s| {
                let id = s.id.0;
                let mut e = Engine::new(s);
                e.set_self_trade_prevention(true);
                (id, e)
            })
            .collect();
        let mut runtime = Self {
            manifest,
            engines,
            orders: BTreeMap::new(),
            requests: BTreeMap::new(),
            decisions: Vec::new(),
            journal,
            maintenance: false,
            failed: false,
            failure_retryable: false,
            snapshot_path,
        };
        for record in records {
            let (decision, events) = runtime.evaluate(&record.action, record.timestamp)?;
            if fingerprint(&(&decision, &events)).map_err(invalid)?
                != fingerprint(&(&record.decision, &record.events)).map_err(invalid)?
            {
                return Err(invalid("financial replay diverged"));
            }
            runtime.publish(record.action, decision)?;
            if let Some(snapshot) = &saved_snapshot {
                if snapshot["watermark"]["sequence"].as_u64() == Some(runtime.watermark().sequence)
                    && fingerprint(snapshot).map_err(invalid)?
                        != fingerprint(&runtime.snapshot_value()?).map_err(invalid)?
                {
                    return Err(invalid("financial snapshot differs from confirmed journal"));
                }
            }
        }
        if runtime.journal.sequence() != runtime.decisions.len() as u64 {
            return Err(invalid("financial watermark mismatch"));
        }
        Ok(runtime)
    }
    pub fn identity(&self) -> EngineIdentity {
        EngineIdentity {
            storage_version: STORAGE_VERSION,
            engine_id: self.manifest.engine_id,
            definition_hash: self.manifest.definition_hash.clone(),
            capabilities: REQUIRED_CAPABILITIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            watermarks: vec![self.watermark()],
            maintenance: self.maintenance,
            storage: None,
        }
    }
    pub fn watermark(&self) -> Watermark {
        Watermark {
            shard: 0,
            sequence: self.decisions.len() as u64,
            digest: self
                .decisions
                .last()
                .map(|d| d.digest().expect("serializable decision"))
                .unwrap_or_else(|| "0".repeat(64)),
        }
    }
    pub fn decisions(&self, after: u64, digest: &str, limit: usize) -> io::Result<DecisionPage> {
        self.healthy()?;
        if after > self.decisions.len() as u64 || limit == 0 || limit > 256 {
            return Err(invalid("invalid financial cursor/page size"));
        }
        let expected = if after == 0 {
            "0".repeat(64)
        } else {
            self.decisions[after as usize - 1]
                .digest()
                .map_err(invalid)?
        };
        if digest != expected {
            return Err(invalid("financial cursor lineage mismatch"));
        }
        let mut decisions = Vec::new();
        let mut bytes = 0;
        for decision in self.decisions.iter().skip(after as usize).take(limit) {
            let size = serde_json::to_vec(decision).map_err(invalid)?.len();
            if !decisions.is_empty() && bytes + size > 8 * 1024 * 1024 {
                break;
            }
            bytes += size;
            decisions.push(decision.clone());
        }
        Ok(DecisionPage {
            watermark: self.watermark(),
            decisions,
        })
    }
    pub fn submit(&mut self, command: FinancialCommand, timestamp: u64) -> io::Result<Decision> {
        self.healthy()?;
        if let Some((hash, index)) = self.requests.get(&command.request_id) {
            if *hash != fingerprint(&command).map_err(invalid)? {
                return Err(invalid("financial idempotency conflict"));
            }
            return Ok(self.decisions[*index].clone());
        }
        if self.maintenance {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "financial engine is in maintenance",
            ));
        }
        while let Some(symbol) = self.next_expiry_symbol(timestamp) {
            self.commit(Action::Expire(symbol), timestamp)?;
        }
        self.commit(Action::Command(command), timestamp)
    }
    pub fn tick(&mut self, timestamp: u64) -> io::Result<Option<Decision>> {
        self.healthy()?;
        if self.maintenance {
            return Ok(None);
        }
        self.next_expiry_symbol(timestamp)
            .map(|symbol| self.commit(Action::Expire(symbol), timestamp))
            .transpose()
    }
    fn next_expiry_symbol(&self, timestamp: u64) -> Option<u16> {
        self.engines
            .iter()
            .find_map(|(symbol, engine)| engine.has_due_gtd(timestamp).then_some(*symbol))
    }
    pub fn maintenance(&mut self, enabled: bool, timestamp: u64) -> io::Result<Watermark> {
        self.commit(Action::Maintenance(enabled), timestamp)?;
        if enabled {
            if let Err(e) = self.save_snapshot() {
                self.failed = true;
                self.failure_retryable = true;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, e));
            }
        }
        Ok(self.watermark())
    }
    fn snapshot_value(&self) -> io::Result<serde_json::Value> {
        let books:Vec<_>=self.engines.values().map(|engine| {
            let mut book=engine.snapshot();
            book.resting_orders.sort_by_key(|o|o.id);
            book.stop_orders.sort_by_key(|o|o.id);
            serde_json::json!({"symbol":book.symbol,"sequence":book.sequence,"trade_id_gen":book.trade_id_gen,"resting_orders":book.resting_orders,"stop_orders":book.stop_orders})
        }).collect();
        Ok(
            serde_json::json!({"storage_version":STORAGE_VERSION,"engine_id":self.manifest.engine_id,
            "definition_hash":self.manifest.definition_hash,"watermark":self.watermark(),
            "maintenance":self.maintenance,"orders":self.orders,"requests":self.requests,"books":books}),
        )
    }
    fn save_snapshot(&self) -> io::Result<()> {
        let payload = self.snapshot_value()?;
        let envelope =
            serde_json::json!({"digest":fingerprint(&payload).map_err(invalid)?,"payload":payload});
        let temporary = self
            .snapshot_path
            .with_extension(format!("tmp-{}", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(&envelope).map_err(invalid)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.snapshot_path)?;
        File::open(self.snapshot_path.parent().expect("snapshot directory"))?.sync_all()?;
        Ok(())
    }
    pub fn healthy(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::new(
                if self.failure_retryable {
                    io::ErrorKind::WouldBlock
                } else {
                    io::ErrorKind::InvalidData
                },
                "financial engine failed; restart/reconciliation required",
            ))
        } else {
            Ok(())
        }
    }
    fn commit(&mut self, action: Action, timestamp: u64) -> io::Result<Decision> {
        self.healthy()?;
        let (decision, events) = match self.evaluate(&action, timestamp) {
            Ok(value) => value,
            Err(e) => {
                self.failed = true;
                return Err(e);
            }
        };
        let record = Record {
            action: action.clone(),
            timestamp,
            decision: decision.clone(),
            events,
        };
        if let Err(e) = self.journal.append(&record) {
            self.failed = true;
            self.failure_retryable = !matches!(
                e.kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
            );
            return Err(if self.failure_retryable {
                io::Error::new(io::ErrorKind::WouldBlock, e)
            } else {
                e
            });
        }
        self.publish(action, decision.clone())?;
        Ok(decision)
    }
    fn publish(&mut self, action: Action, decision: Decision) -> io::Result<()> {
        if let Action::Command(c) = action {
            self.requests.insert(
                c.request_id,
                (fingerprint(&c).map_err(invalid)?, self.decisions.len()),
            );
        }
        self.decisions.push(decision);
        Ok(())
    }
    fn evaluate(
        &mut self,
        action: &Action,
        timestamp: u64,
    ) -> io::Result<(Decision, Vec<MatchEvent>)> {
        let mut events = Vec::new();
        let mut affected = BTreeSet::new();
        let mut commands = Vec::new();
        match action {
            Action::Maintenance(enabled) => self.maintenance = *enabled,
            // Retain replay of pre-release journals; new sweeps use bounded Expire records.
            Action::Tick => {
                for e in self.engines.values_mut() {
                    events.extend(e.expire_gtd_at(timestamp));
                }
            }
            Action::Expire(symbol) => {
                events.extend(
                    self.engines
                        .get_mut(symbol)
                        .ok_or_else(|| invalid("unknown expiry symbol"))?
                        .expire_gtd_at_limit(timestamp, 128),
                );
            }
            Action::Command(c) => {
                for engine in self.engines.values_mut() {
                    events.extend(engine.expire_gtd_at(timestamp));
                }
                c.spec.validate().map_err(invalid)?;
                if c.account_id.is_nil()
                    || c.request_id.is_nil()
                    || c.reservation_id.is_nil()
                    || c.reservation_id != c.request_id
                    || c.fee_schedule.is_nil()
                    || c.order_id <= 0
                    || c.version <= 0
                {
                    return Err(invalid("invalid funded identity"));
                }
                let engine = self
                    .engines
                    .get_mut(&c.spec.symbol)
                    .ok_or_else(|| invalid("unknown financial symbol"))?;
                let previous = self.orders.get(&c.order_id).cloned();
                match c.operation {
                    Operation::Place => {
                        if previous.is_some() || c.version != 1 {
                            return Err(invalid("reused financial order identity"));
                        }
                    }
                    Operation::Cancel | Operation::Amend => {
                        let old = previous
                            .as_ref()
                            .ok_or_else(|| invalid("unknown financial order"))?;
                        if old.account_id != c.account_id
                            || old.spec.symbol != c.spec.symbol
                            || old.spec.side != c.spec.side
                            || old.version != c.expected_version
                            || old.fee_schedule != c.fee_schedule
                        {
                            return Err(invalid("financial ownership/version mismatch"));
                        }
                        let mut allowed = old.spec.clone();
                        if c.operation == Operation::Amend {
                            allowed.price_raw = c.spec.price_raw;
                            allowed.qty_raw = c.spec.qty_raw;
                        }
                        if allowed != c.spec {
                            return Err(invalid("unsupported financial amendment fields"));
                        }
                        if c.operation == Operation::Amend && c.version <= old.version {
                            return Err(invalid("non-increasing order version"));
                        }
                        if c.operation == Operation::Cancel && c.version != old.version {
                            return Err(invalid("cancel version mismatch"));
                        }
                    }
                }
                if matches!(c.spec.order_type, 1 | 3) {
                    engine
                        .book_mut()
                        .set_market_protection(
                            c.order_id as u64,
                            FixedPrice(c.spec.protection_price_raw),
                        )
                        .map_err(|_| invalid("invalid protection"))?;
                }
                let cmd = match c.operation {
                    Operation::Place => Command::PlaceOrder(make_order(c, timestamp)?),
                    Operation::Cancel => Command::CancelOrder {
                        order_id: c.order_id as u64,
                        symbol: SymbolId(c.spec.symbol),
                        timestamp,
                    },
                    Operation::Amend => Command::AmendOrder {
                        order_id: c.order_id as u64,
                        symbol: SymbolId(c.spec.symbol),
                        new_price: Some(FixedPrice(c.spec.price_raw)),
                        new_qty: Some(FixedQty(c.spec.qty_raw)),
                        timestamp,
                    },
                };
                if c.operation == Operation::Place
                    && c.spec.time_in_force == 3
                    && c.spec.expire_at <= timestamp
                {
                    events.push(MatchEvent::OrderRejected {
                        order_id: c.order_id as u64,
                        reason: me_core::events::RejectReason::InvalidExpiry,
                        timestamp,
                    });
                } else {
                    events.extend_from_slice(engine.process_with_session_at(
                        cmd,
                        SessionId::from_bytes(*c.account_id.as_bytes()),
                        timestamp,
                    ));
                }
                let accepted=!events.iter().any(|e|matches!(e,MatchEvent::OrderRejected {order_id,..} if *order_id==c.order_id as u64));
                if c.operation == Operation::Place || (accepted && c.operation == Operation::Amend)
                {
                    self.orders.insert(
                        c.order_id,
                        Metadata {
                            account_id: c.account_id,
                            reservation_id: c.reservation_id,
                            fee_schedule: c.fee_schedule,
                            version: c.version,
                            spec: c.spec.clone(),
                            status: "pending".into(),
                        },
                    );
                }
                affected.insert(c.order_id);
                commands.push(CommandOutcome {
                    request_id: c.request_id,
                    accepted,
                });
            }
        }
        if events.iter().any(|event| {
            matches!(
                event,
                MatchEvent::OrderRejected {
                    reason: me_core::events::RejectReason::InternalError,
                    ..
                }
            )
        }) || self
            .engines
            .values()
            .any(|e| e.book().invariant_violations_total() != 0)
        {
            return Err(invalid("financial matching invariant failed"));
        }
        let mut fills = Vec::new();
        let mut terminal = BTreeMap::new();
        for event in &events {
            match event {
                MatchEvent::Trade(t) => {
                    let maker = t.maker_order_id as i64;
                    let taker = t.taker_order_id as i64;
                    let m = self
                        .orders
                        .get(&maker)
                        .ok_or_else(|| invalid("unknown financial maker"))?;
                    let tkr = self
                        .orders
                        .get(&taker)
                        .ok_or_else(|| invalid("unknown financial taker"))?;
                    if m.account_id == tkr.account_id {
                        return Err(invalid("financial self trade"));
                    }
                    fills.push(Fill {
                        maker_order: maker,
                        taker_order: taker,
                        maker_version: m.version,
                        taker_version: tkr.version,
                        price_raw: t.price.raw(),
                        qty_raw: t.qty.raw(),
                    });
                    affected.insert(maker);
                    affected.insert(taker);
                }
                MatchEvent::OrderFilled { order_id, .. } => {
                    affected.insert(*order_id as i64);
                    terminal.insert(*order_id as i64, "filled");
                }
                MatchEvent::OrderCancelled { order_id, .. } => {
                    affected.insert(*order_id as i64);
                    terminal.insert(*order_id as i64, "cancelled");
                }
                MatchEvent::OrderRejected { order_id, .. } => {
                    affected.insert(*order_id as i64);
                    terminal.entry(*order_id as i64).or_insert("rejected");
                }
                MatchEvent::OrderAccepted { order_id, .. }
                | MatchEvent::OrderPartiallyFilled { order_id, .. }
                | MatchEvent::OrderAmended { order_id, .. }
                | MatchEvent::StopTriggered { order_id, .. } => {
                    affected.insert(*order_id as i64);
                }
                MatchEvent::OrderStatus { .. } | MatchEvent::OrderNotFound { .. } => {}
            }
        }
        let mut orders = Vec::new();
        let mut bindings = Vec::new();
        for id in affected {
            let metadata = self
                .orders
                .get(&id)
                .ok_or_else(|| invalid("unknown financial event order"))?;
            let engine = &self.engines[&metadata.spec.symbol];
            let live = engine
                .book()
                .get_order(id as u64)
                .or_else(|| engine.stop_book().get(id as u64));
            let (status, remaining) = if let Some(order) = live {
                (
                    "open",
                    order
                        .qty
                        .raw()
                        .checked_sub(order.cumulative_qty.raw())
                        .ok_or_else(|| invalid("invalid financial remainder"))?,
                )
            } else {
                (
                    if matches!(
                        metadata.status.as_str(),
                        "filled" | "cancelled" | "rejected"
                    ) {
                        metadata.status.as_str()
                    } else {
                        *terminal
                            .get(&id)
                            .ok_or_else(|| invalid("missing terminal financial outcome"))?
                    },
                    0,
                )
            };
            if live.is_none() {
                self.engines
                    .get_mut(&metadata.spec.symbol)
                    .unwrap()
                    .book_mut()
                    .remove_market_protection(id as u64);
            }
            bindings.push(OrderBinding {
                order_id: id,
                version: metadata.version,
                account_id: metadata.account_id,
                reservation_id: metadata.reservation_id,
                fee_schedule: metadata.fee_schedule,
            });
            orders.push(OrderOutcome {
                order_id: id,
                version: metadata.version,
                remaining,
                status: status.into(),
            });
            self.orders.get_mut(&id).unwrap().status = orders.last().unwrap().status.clone();
        }
        let watermark = self.watermark();
        let decision = Decision {
            engine_id: self.manifest.engine_id,
            shard: 0,
            sequence: watermark
                .sequence
                .checked_add(1)
                .ok_or_else(|| invalid("financial sequence exhausted"))?,
            previous_digest: watermark.digest,
            commands,
            fills,
            orders,
            bindings,
        };
        Ok((decision, events))
    }
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn make_order(c: &FinancialCommand, timestamp: u64) -> io::Result<Order> {
    let side = match c.spec.side {
        Side::Buy => me_core::types::Side::Buy,
        Side::Sell => me_core::types::Side::Sell,
    };
    let tif = match c.spec.time_in_force {
        0 => TimeInForce::GTC,
        1 => TimeInForce::IOC,
        2 => TimeInForce::FOK,
        3 => TimeInForce::GTD,
        _ => return Err(invalid("time in force")),
    };
    let mut order = Order::new_limit(
        c.order_id as u64,
        SymbolId(c.spec.symbol),
        side,
        tif,
        FixedPrice(c.spec.price_raw),
        FixedQty(c.spec.qty_raw),
        timestamp,
    );
    order.order_type = match c.spec.order_type {
        0 => OrderType::Limit,
        1 => OrderType::Market,
        2 => OrderType::StopLimit,
        3 => OrderType::StopMarket,
        _ => return Err(invalid("order type")),
    };
    order.stop_price = FixedPrice(c.spec.stop_price_raw);
    order.trail_offset = FixedPrice(c.spec.trail_offset_raw);
    order.display_qty = FixedQty(c.spec.display_qty_raw);
    order.expire_at = c.spec.expire_at;
    order.flags = c.spec.flags;
    order.client_session_id = SessionId::from_bytes(*c.account_id.as_bytes());
    order.status = OrderStatus::New;
    Ok(order)
}

#[cfg(test)]
mod initialization_tests {
    use super::*;
    #[test]
    #[ignore = "subprocess crash helper"]
    fn initialization_crash_child() {
        let Ok(directory) = std::env::var("ME_TEST_FIN_INIT_DIRECTORY") else {
            return;
        };
        let phase = std::env::var("ME_TEST_FIN_INIT_PHASE").unwrap();
        let _ = FinancialEngine::open_with_init_hook(
            directory,
            vec![SymbolConfig::default_btc_usdt()],
            |at| {
                if at == phase {
                    std::process::exit(93);
                }
                Ok(())
            },
        );
        panic!("initialization failpoint was not reached");
    }
    #[test]
    fn pre_release_unbounded_tick_records_still_replay() {
        let directory =
            std::env::temp_dir().join(format!("me-financial-old-tick-{}", Uuid::new_v4()));
        let mut engine =
            FinancialEngine::open(&directory, vec![SymbolConfig::default_btc_usdt()]).unwrap();
        let decision = engine.commit(Action::Tick, 1).unwrap();
        assert!(decision.commands.is_empty());
        drop(engine);
        let engine =
            FinancialEngine::open(&directory, vec![SymbolConfig::default_btc_usdt()]).unwrap();
        assert_eq!(engine.watermark().sequence, 1);
        assert_eq!(engine.watermark().digest, decision.digest().unwrap());
        drop(engine);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn snapshot_io_failure_stays_retryable_until_restart() {
        let directory =
            std::env::temp_dir().join(format!("me-financial-snapshot-error-{}", Uuid::new_v4()));
        let mut engine =
            FinancialEngine::open(&directory, vec![SymbolConfig::default_btc_usdt()]).unwrap();
        let id = engine.identity().engine_id;
        engine.snapshot_path = directory.join("missing-parent/snapshot.json");
        assert_eq!(
            engine.maintenance(true, 1).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            engine.healthy().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(engine);
        let mut engine =
            FinancialEngine::open(&directory, vec![SymbolConfig::default_btc_usdt()]).unwrap();
        assert!(engine.identity().maintenance);
        assert_eq!(engine.identity().engine_id, id);
        engine.maintenance(true, 2).unwrap();
        engine.healthy().unwrap();
        drop(engine);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn abrupt_initialization_never_publishes_half_a_generation() {
        for phase in ["manifest", "wal"] {
            let directory =
                std::env::temp_dir().join(format!("me-financial-init-{}", Uuid::new_v4()));
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "financial::initialization_tests::initialization_crash_child",
                    "--ignored",
                ])
                .env("ME_TEST_FIN_INIT_DIRECTORY", &directory)
                .env("ME_TEST_FIN_INIT_PHASE", phase)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(93));
            assert!(!directory.join("active").exists());
            let engine =
                FinancialEngine::open(&directory, vec![SymbolConfig::default_btc_usdt()]).unwrap();
            assert_eq!(engine.watermark().sequence, 0);
            drop(engine);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
