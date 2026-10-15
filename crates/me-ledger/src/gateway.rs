use crate::*;
use me_financial::FinancialCommand;
use sqlx::row::Row;
use sqlx_postgres::PgConnection;
use uuid::Uuid;
pub struct GatewayLease {
    connection: PgConnection,
    key: i64,
    valid: bool,
}
impl GatewayLease {
    pub async fn verify(&mut self) -> Result<()> {
        if !self.valid {
            return Err(Error::Reconciliation(
                "gateway lease lost; restart required".into(),
            ));
        }
        let result=sqlx::query_scalar::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND objsubid=1 AND classid::BIGINT=$1 AND objid::BIGINT=$2)")
            .bind(((self.key as u64)>>32) as i64).bind(((self.key as u64) & 0xffff_ffff) as i64).fetch_one(&mut self.connection).await;
        match result {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.valid = false;
                Err(Error::Reconciliation("gateway lease lost".into()))
            }
            Err(e) => {
                self.valid = false;
                Err(e.into())
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct OutboxItem {
    pub sequence: i64,
    pub request_id: Uuid,
    pub wire_request_id: u64,
    pub command: FinancialCommand,
    pub encoded_frame: Option<Vec<u8>>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct LedgerStatus {
    pub engine_id: Uuid,
    pub definition_hash: String,
    pub maintenance: bool,
    pub reconciliation_reason: Option<String>,
    pub pending_outbox: i64,
    pub pending_requests: i64,
    pub oldest_pending_seconds: f64,
}
impl Ledger {
    /// Only an admission hint; mutation methods still verify the complete request hash.
    pub async fn has_request(&self, account: Uuid, key: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM requests WHERE account_id=$1 AND idempotency_key=$2)",
        )
        .bind(account)
        .bind(key)
        .fetch_one(&self.pool)
        .await?)
    }
    pub async fn has_external_reference(&self, reference: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM journals WHERE dedup_key=$1)",
        )
        .bind(format!("external:{reference}"))
        .fetch_one(&self.pool)
        .await?)
    }
    pub async fn acquire_gateway(&self) -> Result<GatewayLease> {
        let mut connection = self.pool.acquire().await?.detach();
        let key: i64 = sqlx::query_scalar::query_scalar(
            "SELECT hashtextextended(current_database()||':'||current_schema()||':me-gateway',0)",
        )
        .fetch_one(&mut connection)
        .await?;
        let acquired: bool = sqlx::query_scalar::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut connection)
            .await?;
        if !acquired {
            return Err(Error::Invalid("another gateway owns this ledger"));
        }
        Ok(GatewayLease {
            connection,
            valid: true,
            key,
        })
    }
    pub async fn outbox(&self, limit: u16) -> Result<Vec<OutboxItem>> {
        if limit == 0 || limit > 256 {
            return Err(Error::Invalid("outbox page size"));
        }
        let rows=sqlx::query::query("SELECT sequence,request_id,engine_request_id,payload,encoded_frame FROM outbox WHERE NOT completed ORDER BY sequence LIMIT $1").bind(i64::from(limit)).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                let mut payload: serde_json::Value = row.get("payload");
                let request_id: Uuid = row.get("request_id");
                payload["request_id"] = serde_json::to_value(request_id)?;
                let command = serde_json::from_value(payload)?;
                let wire: i64 = row.get("engine_request_id");
                let wire_request_id = wire
                    .try_into()
                    .map_err(|_| Error::Reconciliation("invalid engine request ID".into()))?;
                Ok(OutboxItem {
                    sequence: row.get("sequence"),
                    request_id,
                    wire_request_id,
                    command,
                    encoded_frame: row.get("encoded_frame"),
                })
            })
            .collect()
    }
    pub async fn save_frame(&self, request: Uuid, bytes: &[u8]) -> Result<Vec<u8>> {
        let row=sqlx::query::query("UPDATE outbox SET encoded_frame=COALESCE(encoded_frame,$2) WHERE request_id=$1 RETURNING encoded_frame").bind(request).bind(bytes).fetch_one(&self.pool).await?;
        let stored: Vec<u8> = row.get("encoded_frame");
        if stored != bytes {
            return Err(Error::Reconciliation("outbox frame changed".into()));
        }
        Ok(stored)
    }
    pub async fn reserved_totals(&self) -> Result<Vec<(String, Amount)>> {
        let rows=sqlx::query::query("SELECT asset,sum(balance)::TEXT AS units FROM wallets WHERE bucket='reserved' GROUP BY asset ORDER BY asset").fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| Ok((row.get("asset"), crate::store::units(row.get("units"))?)))
            .collect()
    }
    pub async fn status(&self) -> Result<LedgerStatus> {
        let row=sqlx::query::query("SELECT *, (SELECT count(*) FROM outbox WHERE NOT completed) AS pending_outbox,(SELECT count(*) FROM requests WHERE status='pending') AS pending_requests, COALESCE((SELECT EXTRACT(EPOCH FROM now()-min(created_at))::DOUBLE PRECISION FROM requests WHERE status='pending'),0) AS oldest_pending_seconds FROM engine_binding WHERE singleton=TRUE").fetch_one(&self.pool).await?;
        Ok(LedgerStatus {
            engine_id: row.get("engine_id"),
            definition_hash: row.get("definition_hash"),
            maintenance: row.get("maintenance"),
            reconciliation_reason: row.get("reconciliation_reason"),
            pending_outbox: row.get("pending_outbox"),
            pending_requests: row.get("pending_requests"),
            oldest_pending_seconds: row.get("oldest_pending_seconds"),
        })
    }
    pub async fn set_maintenance(&self, enabled: bool) -> Result<()> {
        let n=sqlx::query::query("UPDATE engine_binding SET maintenance=$1 WHERE singleton=TRUE AND ($1 OR reconciliation_reason IS NULL)").bind(enabled).execute(&self.pool).await?.rows_affected();
        if n != 1 {
            return Err(Error::Reconciliation(
                "cannot resume without reconciliation".into(),
            ));
        }
        Ok(())
    }
    pub async fn block(&self, reason: &str) -> Result<()> {
        sqlx::query::query("UPDATE engine_binding SET reconciliation_reason=COALESCE(reconciliation_reason,$1) WHERE singleton=TRUE").bind(reason).execute(&self.pool).await?;
        Ok(())
    }
    /// Check materialized funds and reservations against their immutable sources in one snapshot.
    pub async fn audit(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let wallet_mismatch:bool=sqlx::query_scalar::query_scalar("SELECT EXISTS(SELECT 1 FROM wallets w LEFT JOIN (SELECT wallet_id,sum(amount) AS total FROM postings GROUP BY wallet_id) p ON p.wallet_id=w.id WHERE w.balance<>COALESCE(p.total,0))").fetch_one(&mut *tx).await?;
        let reserve_mismatch:bool=sqlx::query_scalar::query_scalar("SELECT EXISTS(SELECT 1 FROM wallets w LEFT JOIN (SELECT o.account_id,CASE WHEN o.spec->>'side'='buy' THEN m.quote ELSE m.base END AS asset,sum(o.held) AS held FROM orders o JOIN markets m ON m.symbol=o.symbol GROUP BY o.account_id,asset) o ON o.account_id=w.account_id AND o.asset=w.asset WHERE w.bucket='reserved' AND w.balance<>COALESCE(o.held,0))").fetch_one(&mut *tx).await?;
        let unbalanced:bool=sqlx::query_scalar::query_scalar("SELECT EXISTS(SELECT 1 FROM postings p JOIN wallets w ON w.id=p.wallet_id GROUP BY p.journal_id,w.asset HAVING sum(p.amount)<>0)").fetch_one(&mut *tx).await?;
        let outbox_mismatch:bool=sqlx::query_scalar::query_scalar("SELECT EXISTS(SELECT 1 FROM requests r LEFT JOIN outbox o ON o.request_id=r.id WHERE o.request_id IS NULL OR ((r.status='pending')=o.completed))").fetch_one(&mut *tx).await?;
        tx.commit().await?;
        if wallet_mismatch || reserve_mismatch || unbalanced || outbox_mismatch {
            return Err(Error::Reconciliation(format!("audit: wallets={wallet_mismatch} reserves={reserve_mismatch} journals={unbalanced} intents={outbox_mismatch}")));
        }
        Ok(())
    }
}

impl Ledger {
    pub async fn has_engine(&self) -> Result<bool> {
        Ok(
            sqlx::query_scalar::query_scalar("SELECT EXISTS(SELECT 1 FROM engine_binding)")
                .fetch_one(&self.pool)
                .await?,
        )
    }
    pub async fn provision_account(&self, key: &str, payload: &serde_json::Value) -> Result<Uuid> {
        if key.is_empty() || key.len() > 256 {
            return Err(Error::Invalid("account idempotency key"));
        }
        let hash = fingerprint(payload)?;
        let mut tx = self.pool.begin().await?;
        // The named advisory lock serializes account creation without locking trading accounts.
        sqlx::query::query("SELECT pg_advisory_xact_lock(hashtextextended($1,1))")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        if let Some(row) =
            sqlx::query::query("SELECT payload_hash,account_id FROM account_keys WHERE key=$1")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?
        {
            return if row.get::<String, _>("payload_hash") == hash {
                Ok(row.get("account_id"))
            } else {
                Err(Error::Conflict)
            };
        }
        let id = Uuid::new_v4();
        sqlx::query::query("INSERT INTO accounts(id) VALUES($1)")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query::query(
            "INSERT INTO account_keys(key,payload_hash,account_id) VALUES($1,$2,$3)",
        )
        .bind(key)
        .bind(hash)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }
    pub async fn clear_reconciliation(
        &self,
        engine: Uuid,
        sequence: u64,
        digest: &str,
    ) -> Result<()> {
        self.audit().await?;
        let status = self.status().await?;
        if !status.maintenance
            || status.engine_id != engine
            || status.pending_requests != 0
            || status.pending_outbox != 0
            || self.cursor(engine, 0).await? != (sequence, digest.to_owned())
        {
            return Err(Error::Reconciliation(
                "source/cursor or unresolved intentions require manual reconciliation".into(),
            ));
        }
        // Admission remains paused until the gateway explicitly resumes after source verification.
        sqlx::query::query("UPDATE engine_binding SET reconciliation_reason=NULL,maintenance=TRUE WHERE singleton=TRUE").execute(&self.pool).await?;
        Ok(())
    }
    pub async fn account_events(
        &self,
        account: Uuid,
        after: u64,
        digest: &str,
        limit: u16,
    ) -> Result<serde_json::Value> {
        if limit == 0 || limit > 256 {
            return Err(Error::Invalid("event page size"));
        }
        let status = self.status().await?;
        let previous = if after == 0 {
            "0".repeat(64)
        } else {
            sqlx::query_scalar::query_scalar::<_,String>("SELECT digest FROM decisions WHERE engine_id=$1 AND shard=0 AND sequence=$2::TEXT::NUMERIC").bind(status.engine_id).bind(after.to_string()).fetch_optional(&self.pool).await?.ok_or_else(||Error::Reconciliation("unknown event cursor".into()))?
        };
        if previous != digest {
            return Err(Error::Reconciliation(
                "event cursor lineage mismatch".into(),
            ));
        }
        let rows=sqlx::query::query("SELECT payload FROM decisions WHERE engine_id=$1 AND shard=0 AND sequence>$2::TEXT::NUMERIC ORDER BY sequence LIMIT $3").bind(status.engine_id).bind(after.to_string()).bind(i64::from(limit)).fetch_all(&self.pool).await?;
        let mut result = Vec::new();
        let mut cursor = me_financial::Watermark {
            shard: 0,
            sequence: after,
            digest: previous,
        };
        for row in rows {
            let decision: Decision = serde_json::from_value(row.get("payload"))?;
            let ids: Vec<_> = decision.orders.iter().map(|o| o.order_id).collect();
            let own: std::collections::BTreeSet<i64> = sqlx::query_scalar::query_scalar(
                "SELECT id FROM orders WHERE account_id=$1 AND id=ANY($2)",
            )
            .bind(account)
            .bind(&ids)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .collect();
            let requests: Vec<_> = decision.commands.iter().map(|c| c.request_id).collect();
            let own_requests: std::collections::BTreeSet<Uuid> = sqlx::query_scalar::query_scalar(
                "SELECT id FROM requests WHERE account_id=$1 AND id=ANY($2)",
            )
            .bind(account)
            .bind(&requests)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .collect();
            let mut events = Vec::new();
            let mut index = 0;
            for c in &decision.commands {
                if own_requests.contains(&c.request_id) {
                    events.push(serde_json::json!({"index":index,"type":"request","request_id":c.request_id,"accepted":c.accepted}));
                }
                index += 1;
            }
            for f in &decision.fills {
                for (id, maker) in [(f.maker_order, true), (f.taker_order, false)] {
                    if own.contains(&id) {
                        let row=sqlx::query::query("SELECT f.maker_bps,f.taker_bps,m.base,m.quote FROM orders o JOIN fee_schedules f ON f.id=o.fee_schedule JOIN markets m ON m.symbol=o.symbol WHERE o.id=$1").bind(id).fetch_one(&self.pool).await?;
                        let bps =
                            row.get::<i32, _>(if maker { "maker_bps" } else { "taker_bps" }) as u16;
                        events.push(serde_json::json!({"index":index,"type":"trade","order_id":id,"maker":maker,"price":Amount::quote(f.price_raw,100_000_000)?,"quantity":Amount::base(f.qty_raw),"fee":Amount::fee(f.price_raw,f.qty_raw,bps)?,"base":row.get::<String,_>("base"),"quote":row.get::<String,_>("quote")}));
                    }
                }
                index += 1;
            }
            for o in &decision.orders {
                if own.contains(&o.order_id) {
                    events.push(serde_json::json!({"index":index,"type":"order","order_id":o.order_id,"version":o.version,"status":o.status,"remaining":Amount::base(o.remaining)}));
                }
                index += 1;
            }
            cursor = me_financial::Watermark {
                shard: 0,
                sequence: decision.sequence,
                digest: decision.digest()?,
            };
            result.push(
                serde_json::json!({"engine_id":status.engine_id,"cursor":cursor,"events":events}),
            );
        }
        Ok(serde_json::json!({"decisions":result,"cursor":cursor}))
    }
}
