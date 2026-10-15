use crate::{
    store::{post, retryable, Posting},
    *,
};
use sqlx::row::Row;
use uuid::Uuid;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Amend {
    pub price_raw: Option<i64>,
    pub qty_raw: Option<u64>,
}
impl Ledger {
    pub async fn request(&self, account: Uuid, key: &str) -> Result<serde_json::Value> {
        let row=sqlx::query::query("SELECT id,status,operation,payload FROM requests WHERE account_id=$1 AND idempotency_key=$2").bind(account).bind(key).fetch_one(&self.pool).await?;
        Ok(
            serde_json::json!({"id":row.get::<Uuid,_>("id"),"status":row.get::<String,_>("status"),"operation":row.get::<String,_>("operation"),"payload":row.get::<serde_json::Value,_>("payload")}),
        )
    }
    pub async fn cancel(&self, account: Uuid, key: &str, id: i64) -> Result<Uuid> {
        self.mutate(account, key, id, None).await
    }
    pub async fn amend(&self, account: Uuid, key: &str, id: i64, amend: Amend) -> Result<Uuid> {
        if amend.price_raw.is_none() && amend.qty_raw.is_none() {
            return Err(Error::Invalid("empty amendment"));
        }
        self.mutate(account, key, id, Some(amend)).await
    }
    async fn mutate(
        &self,
        account: Uuid,
        key: &str,
        id: i64,
        amend: Option<Amend>,
    ) -> Result<Uuid> {
        if key.is_empty() || key.len() > 128 {
            return Err(Error::Invalid("idempotency key"));
        }
        for attempt in 0..8 {
            match self.mutate_once(account, key, id, &amend).await {
                Err(e) if retryable(&e) && attempt < 7 => {
                    tokio::time::sleep(std::time::Duration::from_millis(2 << attempt)).await
                }
                r => return r,
            }
        }
        unreachable!()
    }
    async fn mutate_once(
        &self,
        account: Uuid,
        key: &str,
        id: i64,
        amend: &Option<Amend>,
    ) -> Result<Uuid> {
        let operation = if amend.is_some() { "amend" } else { "cancel" };
        let payload = serde_json::json!({"order_id":id,"amend":amend});
        let hash = fingerprint(&(operation, account, &payload))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        if let Some(row) = sqlx::query::query(
            "SELECT id,payload_hash FROM requests WHERE account_id=$1 AND idempotency_key=$2",
        )
        .bind(account)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        {
            return if row.get::<String, _>("payload_hash") == hash {
                Ok(row.get("id"))
            } else {
                Err(Error::Conflict)
            };
        }
        crate::store::ensure_admission(&mut tx).await?;
        let active: bool =
            sqlx::query_scalar::query_scalar("SELECT active FROM accounts WHERE id=$1 FOR UPDATE")
                .bind(account)
                .fetch_one(&mut *tx)
                .await?;
        if !active {
            return Err(Error::Invalid("account inactive"));
        }
        let row=sqlx::query::query("SELECT o.*,o.remaining::TEXT AS remaining_units,o.held::TEXT AS held_units,m.base,m.quote,f.maker_bps,f.taker_bps FROM orders o JOIN markets m ON m.symbol=o.symbol JOIN fee_schedules f ON f.id=o.fee_schedule WHERE o.id=$1 AND o.account_id=$2 FOR UPDATE OF o").bind(id).bind(account).fetch_one(&mut *tx).await?;
        if row.get::<Option<Uuid>, _>("pending_request").is_some() {
            return Err(Error::Invalid("order has an unresolved request"));
        }
        let fees = Fees {
            id: row.get("fee_schedule"),
            maker_bps: row.get::<i32, _>("maker_bps") as u16,
            taker_bps: row.get::<i32, _>("taker_bps") as u16,
        };
        let base: String = row.get("base");
        let quote: String = row.get("quote");
        let state = crate::store::order_row(row)?;
        if state.status != "open" {
            return Err(Error::Invalid("order is not open"));
        }
        let request = Uuid::new_v4();
        sqlx::query::query("INSERT INTO requests(id,account_id,idempotency_key,payload_hash,operation,payload) VALUES($1,$2,$3,$4,$5,$6)").bind(request).bind(account).bind(key).bind(hash).bind(operation).bind(&payload).execute(&mut *tx).await?;
        let mut version = state.version;
        let mut spec = state.spec.clone();
        if let Some(change) = amend {
            if let Some(price) = change.price_raw {
                spec.price_raw = price;
            }
            if let Some(qty) = change.qty_raw {
                spec.qty_raw = qty;
            }
            spec.validate()?;
            let filled = state
                .spec
                .qty_raw
                .checked_sub(state.remaining)
                .ok_or_else(|| Error::Reconciliation("invalid cumulative quantity".into()))?;
            let new_remaining = spec.qty_raw.saturating_sub(filled);
            let required = match spec.side {
                Side::Buy => amend_buy_reserve(
                    state.spec.reserve_price(),
                    state.remaining,
                    spec.reserve_price(),
                    new_remaining,
                    fees.maker_bps.max(fees.taker_bps),
                )?,
                Side::Sell => Amount::base(state.remaining.max(new_remaining)),
            };
            if required > state.held {
                let extra = required.checked_sub(&state.held)?;
                let asset = if spec.side == Side::Buy {
                    &quote
                } else {
                    &base
                };
                post(
                    &mut tx,
                    &format!("amend-reserve:{request}"),
                    "reserve",
                    &payload,
                    vec![
                        Posting::new(Some(account), asset, "available", extra.negated()),
                        Posting::new(Some(account), asset, "reserved", extra),
                    ],
                )
                .await?;
                sqlx::query::query("UPDATE orders SET held=$2::TEXT::NUMERIC WHERE id=$1")
                    .bind(id)
                    .bind(required.units())
                    .execute(&mut *tx)
                    .await?;
            }
            version = sqlx::query_scalar::query_scalar::<_, i64>(
                "SELECT max(version)+1 FROM order_versions WHERE order_id=$1",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query::query(
                "INSERT INTO order_versions(order_id,version,spec,request_id) VALUES($1,$2,$3,$4)",
            )
            .bind(id)
            .bind(version)
            .bind(serde_json::to_value(&spec)?)
            .bind(request)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query::query("UPDATE orders SET pending_request=$2 WHERE id=$1")
            .bind(id)
            .bind(request)
            .execute(&mut *tx)
            .await?;
        let envelope = serde_json::json!({"operation":operation,"order_id":id,"account_id":account,"version":version,"expected_version":state.version,"reservation_id":request,"fee_schedule":fees.id,"spec":spec});
        sqlx::query::query("INSERT INTO outbox(request_id,payload) VALUES($1,$2)")
            .bind(request)
            .bind(envelope)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(request)
    }
}
