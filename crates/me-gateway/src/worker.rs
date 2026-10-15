use crate::client::Client;
use me_financial::{transport::*, *};
use me_ledger::{GatewayLease, Ledger};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Mutex;
#[derive(Clone)]
pub struct State {
    pub ledger: Ledger,
    pub lease: Arc<Mutex<GatewayLease>>,
    pub client: Arc<Mutex<Client>>,
    pub ready: Arc<AtomicBool>,
    pub lag: Arc<AtomicU64>,
    pub disk_available: Arc<AtomicU64>,
    pub disk_minimum: Arc<AtomicU64>,
    pub journal_bytes: Arc<AtomicU64>,
    pub errors: Arc<AtomicU64>,
    pub engine: EngineIdentity,
    pub maintenance_lock: Arc<Mutex<()>>,
}
impl State {
    pub async fn verify(&self) -> io::Result<()> {
        self.lease.lock().await.verify().await.map_err(error)
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub async fn identity(&self) -> io::Result<EngineIdentity> {
        let identity: EngineIdentity = self
            .client
            .lock()
            .await
            .query(FINANCIAL_IDENTITY, &())
            .await?;
        if identity.engine_id != self.engine.engine_id
            || identity.definition_hash != self.engine.definition_hash
            || identity.storage_version != STORAGE_VERSION
            || identity.watermarks.len() != 1
            || identity.watermarks[0].shard != 0
            || REQUIRED_CAPABILITIES
                .iter()
                .any(|c| !identity.capabilities.iter().any(|v| v == c))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "matcher identity/capabilities changed",
            ));
        }
        Ok(identity)
    }
    async fn settle_to_head(&self) -> io::Result<()> {
        loop {
            let (after, digest) = self
                .ledger
                .cursor(self.engine.engine_id, 0)
                .await
                .map_err(error)?;
            let page: DecisionPage = self
                .client
                .lock()
                .await
                .query(
                    FINANCIAL_DECISIONS,
                    &serde_json::json!({"after":after,"digest":digest,"limit":32}),
                )
                .await?;
            if page.watermark.shard != 0
                || page.watermark.sequence < after
                || (page.watermark.sequence == after && page.watermark.digest != digest)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "settlement source rolled back",
                ));
            }
            self.lag
                .store(page.watermark.sequence - after, Ordering::Release);
            if page.decisions.is_empty() {
                if page.watermark.sequence != after {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing durable decisions",
                    ));
                }
                return Ok(());
            }
            for decision in page.decisions {
                if let Err(e) = self.ledger.settle(&decision).await {
                    match e {
                        me_ledger::Error::Database(_) => return Err(error(e)),
                        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
                    }
                }
            }
        }
    }
    async fn cycle(&self) -> io::Result<()> {
        self.verify().await?;
        let status = self.ledger.status().await.map_err(error)?;
        if let Some(reason) = status.reconciliation_reason {
            return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
        }
        let identity = self.identity().await?;
        self.settle_to_head().await?;
        let storage = identity.storage.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing matcher storage status")
        })?;
        self.disk_available
            .store(storage.available_bytes, Ordering::Release);
        self.disk_minimum
            .store(storage.minimum_free_bytes, Ordering::Release);
        self.journal_bytes
            .store(storage.journal_bytes, Ordering::Release);
        if storage.available_bytes < storage.minimum_free_bytes {
            self.ready.store(false, Ordering::Release);
            return Ok(());
        }
        if let Some(item) = self
            .ledger
            .outbox(1)
            .await
            .map_err(error)?
            .into_iter()
            .next()
        {
            if identity.maintenance {
                self.ready.store(false, Ordering::Release);
                return Ok(());
            }
            self.verify().await?;
            let frame = Frame {
                message_type: FINANCIAL_COMMAND,
                sequence: item.wire_request_id,
                request_id: item.wire_request_id,
                payload: serde_json::to_vec(&item.command).map_err(error)?,
            };
            let bytes = if let Some(bytes) = item.encoded_frame {
                bytes
            } else {
                self.ledger
                    .save_frame(item.request_id, &frame.encode()?)
                    .await
                    .map_err(error)?
            };
            let result: Decision = self
                .client
                .lock()
                .await
                .exchange(&bytes, item.wire_request_id)
                .await?;
            if result.engine_id != self.engine.engine_id
                || !result
                    .commands
                    .iter()
                    .any(|c| c.request_id == item.request_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "matcher command result mismatch",
                ));
            }
            self.settle_to_head().await?;
        }
        self.ready.store(
            !status.maintenance && !identity.maintenance,
            Ordering::Release,
        );
        Ok(())
    }
    pub async fn run(self) -> io::Result<()> {
        loop {
            if let Err(e) = self.verify().await {
                self.ready.store(false, Ordering::Release);
                return Err(e);
            }
            if let Err(e) = self.cycle().await {
                self.errors.fetch_add(1, Ordering::Relaxed);
                self.ready.store(false, Ordering::Release);
                if e.kind() == io::ErrorKind::InvalidData {
                    self.ledger.block(&e.to_string()).await.map_err(error)?;
                }
                tracing::warn!(error=%e,"Financial gateway admission paused");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub async fn maintenance(&self, enabled: bool) -> io::Result<Watermark> {
        let _guard = self.maintenance_lock.lock().await;
        self.verify().await?;
        self.ready.store(false, Ordering::Release);
        if enabled {
            self.ledger.set_maintenance(true).await.map_err(error)?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let state = self.ledger.status().await.map_err(error)?;
                if state.reconciliation_reason.is_some() {
                    break;
                }
                if state.pending_outbox == 0 && state.pending_requests == 0 {
                    break;
                }
                if tokio::time::Instant::now() > deadline {
                    return Err(error(
                        "admission is paused; unresolved intentions are still draining",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else {
            self.ledger.audit().await.map_err(error)?;
            if self
                .ledger
                .status()
                .await
                .map_err(error)?
                .reconciliation_reason
                .is_some()
            {
                return Err(error("reconciliation required"));
            }
        }
        let watermark: Watermark = self
            .client
            .lock()
            .await
            .query(
                FINANCIAL_MAINTENANCE,
                &serde_json::json!({"enabled":enabled}),
            )
            .await?;
        if self
            .ledger
            .status()
            .await
            .map_err(error)?
            .reconciliation_reason
            .is_none()
        {
            self.settle_to_head().await?;
        }
        if !enabled {
            self.ledger.set_maintenance(false).await.map_err(error)?;
        }
        Ok(watermark)
    }
}
