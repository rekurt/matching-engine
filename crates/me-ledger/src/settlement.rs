use crate::{
    store::{post, retryable, Posting},
    *,
};
use sqlx::row::Row;
use sqlx_postgres::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

struct Accounting {
    state: OrderState,
    fees: Fees,
    base: String,
    quote: String,
    pending: Option<Uuid>,
    amendment: Option<OrderSpec>,
}
impl Ledger {
    pub async fn bind_engine(&self, engine: Uuid, definition_hash: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("INSERT INTO engine_binding(singleton,engine_id,definition_hash) VALUES(TRUE,$1,$2) ON CONFLICT(singleton) DO NOTHING").bind(engine).bind(definition_hash).execute(&mut *tx).await?;
        let row = sqlx::query::query(
            "SELECT engine_id,definition_hash FROM engine_binding WHERE singleton=TRUE FOR UPDATE",
        )
        .fetch_one(&mut *tx)
        .await?;
        if row.get::<Uuid, _>("engine_id") != engine
            || row.get::<String, _>("definition_hash") != definition_hash
        {
            return Err(Error::Reconciliation(
                "engine identity/configuration mismatch".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn cursor(&self, engine: Uuid, shard: u32) -> Result<(u64, String)> {
        if shard > i32::MAX as u32 {
            return Err(Error::Invalid("shard"));
        }
        let row = sqlx::query::query(
            "SELECT sequence::TEXT AS sequence,digest FROM cursors WHERE engine_id=$1 AND shard=$2",
        )
        .bind(engine)
        .bind(shard as i32)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok((
                r.get::<String, _>("sequence")
                    .parse()
                    .map_err(|_| Error::Reconciliation("invalid cursor".into()))?,
                r.get("digest"),
            )),
            None => Ok((0, "0".repeat(64))),
        }
    }
    pub async fn settle(&self, decision: &Decision) -> Result<bool> {
        if decision.sequence == 0
            || decision.shard > i32::MAX as u32
            || decision.previous_digest.len() != 64
        {
            return Err(Error::Invalid("decision cursor"));
        }
        for attempt in 0..8 {
            match self.settle_once(decision).await {
                Err(e) if retryable(&e) && attempt < 7 => {
                    tokio::time::sleep(std::time::Duration::from_millis(2 << attempt)).await
                }
                Err(Error::Reconciliation(reason)) => {
                    sqlx::query::query("UPDATE engine_binding SET reconciliation_reason=COALESCE(reconciliation_reason,$1) WHERE singleton=TRUE").bind(&reason).execute(&self.pool).await?;
                    return Err(Error::Reconciliation(reason));
                }
                Err(e) if integrity_failure(&e) => {
                    let reason = e.to_string();
                    self.block(&reason).await?;
                    return Err(Error::Reconciliation(reason));
                }
                result => return result,
            }
        }
        unreachable!()
    }
    async fn settle_once(&self, d: &Decision) -> Result<bool> {
        let hash = d.digest()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let engine: Uuid = sqlx::query_scalar::query_scalar(
            "SELECT engine_id FROM engine_binding WHERE singleton=TRUE",
        )
        .fetch_one(&mut *tx)
        .await?;
        if engine != d.engine_id {
            return Err(Error::Reconciliation("unknown engine".into()));
        }
        if let Some(row)=sqlx::query::query("SELECT digest FROM decisions WHERE engine_id=$1 AND shard=$2 AND sequence=$3::TEXT::NUMERIC").bind(d.engine_id).bind(d.shard as i32).bind(d.sequence.to_string()).fetch_optional(&mut *tx).await? {
            return if row.get::<String,_>("digest")==hash {Ok(false)}else{Err(Error::Reconciliation("decision identity reused".into()))};
        }
        sqlx::query::query("INSERT INTO cursors(engine_id,shard,sequence,digest) VALUES($1,$2,0,$3) ON CONFLICT DO NOTHING").bind(d.engine_id).bind(d.shard as i32).bind("0".repeat(64)).execute(&mut *tx).await?;
        let row=sqlx::query::query("SELECT sequence::TEXT AS sequence,digest FROM cursors WHERE engine_id=$1 AND shard=$2 FOR UPDATE").bind(d.engine_id).bind(d.shard as i32).fetch_one(&mut *tx).await?;
        let seq: u64 = row
            .get::<String, _>("sequence")
            .parse()
            .map_err(|_| Error::Reconciliation("invalid cursor".into()))?;
        if seq.checked_add(1) != Some(d.sequence)
            || row.get::<String, _>("digest") != d.previous_digest
        {
            return Err(Error::Reconciliation(
                "decision gap or lineage mismatch".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for fill in &d.fills {
            ids.insert(fill.maker_order);
            ids.insert(fill.taker_order);
        }
        for outcome in &d.orders {
            ids.insert(outcome.order_id);
        }
        let mut required = BTreeSet::new();
        for fill in &d.fills {
            required.insert((fill.maker_order, fill.maker_version));
            required.insert((fill.taker_order, fill.taker_version));
        }
        for outcome in &d.orders {
            required.insert((outcome.order_id, outcome.version));
        }
        let mut bound = BTreeSet::new();
        for binding in &d.bindings {
            if !bound.insert((binding.order_id, binding.version)) {
                return Err(Error::Reconciliation("duplicate financial binding".into()));
            }
            let row=sqlx::query::query("SELECT o.account_id,o.fee_schedule,v.request_id FROM orders o JOIN order_versions v ON v.order_id=o.id WHERE o.id=$1 AND v.version=$2").bind(binding.order_id).bind(binding.version).fetch_optional(&mut *tx).await?.ok_or_else(||Error::Reconciliation("unknown financial binding".into()))?;
            if row.get::<Uuid, _>("account_id") != binding.account_id
                || row.get::<Uuid, _>("fee_schedule") != binding.fee_schedule
                || row.get::<Uuid, _>("request_id") != binding.reservation_id
            {
                return Err(Error::Reconciliation("financial binding mismatch".into()));
            }
        }
        if bound != required {
            return Err(Error::Reconciliation(
                "incomplete financial bindings".into(),
            ));
        }
        let outcomes: BTreeSet<_> = d.orders.iter().map(|o| o.order_id).collect();
        if d.fills
            .iter()
            .any(|f| !outcomes.contains(&f.maker_order) || !outcomes.contains(&f.taker_order))
        {
            return Err(Error::Reconciliation(
                "fill missing final order outcome".into(),
            ));
        }
        for result in &d.commands {
            let payload: serde_json::Value = sqlx::query_scalar::query_scalar(
                "SELECT payload FROM outbox WHERE request_id=$1 AND NOT completed",
            )
            .bind(result.request_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::Reconciliation("unknown command binding".into()))?;
            let id = payload["order_id"]
                .as_i64()
                .ok_or_else(|| Error::Reconciliation("invalid command binding".into()))?;
            if !outcomes.contains(&id) {
                return Err(Error::Reconciliation(
                    "command missing order outcome".into(),
                ));
            }
            let outcome = d.orders.iter().find(|o| o.order_id == id).unwrap();
            let operation = payload["operation"]
                .as_str()
                .ok_or_else(|| Error::Reconciliation("missing command operation".into()))?;
            let version = payload["version"]
                .as_i64()
                .ok_or_else(|| Error::Reconciliation("missing command version".into()))?;
            if (result.accepted
                && matches!(operation, "place" | "amend")
                && outcome.version != version)
                || (!result.accepted && operation == "place" && outcome.status != "rejected")
                || (result.accepted && operation == "cancel" && outcome.status != "cancelled")
                || (result.accepted && outcome.status == "rejected")
            {
                return Err(Error::Reconciliation(
                    "command outcome contradicts order outcome".into(),
                ));
            }
        }
        let mut orders = BTreeMap::new();
        for id in ids {
            let row=sqlx::query::query("SELECT o.*,o.remaining::TEXT AS remaining_units,o.held::TEXT AS held_units,m.base,m.quote,f.maker_bps,f.taker_bps FROM orders o JOIN markets m ON m.symbol=o.symbol JOIN fee_schedules f ON f.id=o.fee_schedule WHERE o.id=$1 FOR UPDATE OF o").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(||Error::Reconciliation(format!("unknown order {id}")))?;
            let fees = Fees {
                id: row.get("fee_schedule"),
                maker_bps: row.get::<i32, _>("maker_bps") as u16,
                taker_bps: row.get::<i32, _>("taker_bps") as u16,
            };
            let pending: Option<Uuid> = row.get("pending_request");
            let amendment = if let Some(request) = pending {
                sqlx::query_scalar::query_scalar::<_, serde_json::Value>("SELECT v.spec FROM order_versions v JOIN requests r ON r.id=v.request_id WHERE v.request_id=$1 AND r.operation='amend'")
                    .bind(request).fetch_optional(&mut *tx).await?
                    .map(serde_json::from_value).transpose()?
            } else {
                None
            };
            let base = row.get("base");
            let quote = row.get("quote");
            orders.insert(
                id,
                Accounting {
                    state: crate::store::order_row(row)?,
                    fees,
                    base,
                    quote,
                    pending,
                    amendment,
                },
            );
        }
        let mut postings = Vec::new();
        for fill in &d.fills {
            if fill.maker_order == fill.taker_order || fill.qty_raw == 0 || fill.price_raw <= 0 {
                return Err(Error::Reconciliation("invalid fill".into()));
            }
            let maker = &orders[&fill.maker_order];
            let taker = &orders[&fill.taker_order];
            if maker.state.spec.symbol != taker.state.spec.symbol
                || maker.state.spec.side == taker.state.spec.side
                || maker.state.account_id == taker.state.account_id
            {
                return Err(Error::Reconciliation("fill counterparties".into()));
            }
            for (id, version, is_maker) in [
                (fill.maker_order, fill.maker_version, true),
                (fill.taker_order, fill.taker_version, false),
            ] {
                let spec = version_spec(&mut tx, id, version, d, false).await?;
                let order = orders.get_mut(&id).unwrap();
                if matches!(
                    order.state.status.as_str(),
                    "filled" | "cancelled" | "rejected"
                ) || version < order.state.version
                {
                    return Err(Error::Reconciliation(
                        "fill exceeds live quantity/version".into(),
                    ));
                }
                if version > order.state.version {
                    let filled = order
                        .state
                        .spec
                        .qty_raw
                        .checked_sub(order.state.remaining)
                        .ok_or_else(|| {
                            Error::Reconciliation("invalid cumulative quantity".into())
                        })?;
                    order.state.remaining = spec.qty_raw.checked_sub(filled).ok_or_else(|| {
                        Error::Reconciliation("fill after filled-down amendment".into())
                    })?;
                    order.state.spec = spec.clone();
                    order.state.version = version;
                }
                if order.state.remaining < fill.qty_raw {
                    return Err(Error::Reconciliation(
                        "fill exceeds remaining quantity".into(),
                    ));
                }
                let cap = spec.reserve_price();
                if (spec.side == Side::Buy && fill.price_raw > cap)
                    || (spec.side == Side::Sell && fill.price_raw < cap)
                {
                    return Err(Error::Reconciliation(
                        "fill violates protected price".into(),
                    ));
                }
                let fee = Amount::fee(
                    fill.price_raw,
                    fill.qty_raw,
                    if is_maker {
                        order.fees.maker_bps
                    } else {
                        order.fees.taker_bps
                    },
                )?;
                let base = Amount::base(fill.qty_raw);
                let quote = Amount::quote(fill.price_raw, fill.qty_raw)?;
                let account = Some(order.state.account_id);
                let debit = if spec.side == Side::Buy {
                    postings.push(Posting::new(account, &order.base, "available", base));
                    postings.push(Posting::new(
                        account,
                        &order.quote,
                        "reserved",
                        quote.checked_add(&fee)?.negated(),
                    ));
                    quote.checked_add(&fee)?
                } else {
                    postings.push(Posting::new(
                        account,
                        &order.base,
                        "reserved",
                        base.negated(),
                    ));
                    postings.push(Posting::new(
                        account,
                        &order.quote,
                        "available",
                        quote.checked_sub(&fee)?,
                    ));
                    base
                };
                postings.push(Posting::new(None, &order.quote, "fees", fee));
                order.state.held = order.state.held.checked_sub(&debit)?;
                if order.state.held.is_negative() {
                    return Err(Error::Reconciliation("order reserve exhausted".into()));
                }
                order.state.remaining -= fill.qty_raw;
            }
        }
        let mut seen = BTreeSet::new();
        for outcome in &d.orders {
            if !seen.insert(outcome.order_id) {
                return Err(Error::Reconciliation("duplicate order outcome".into()));
            }
            let order = orders.get_mut(&outcome.order_id).unwrap();
            if matches!(
                order.state.status.as_str(),
                "filled" | "cancelled" | "rejected"
            ) && (outcome.status != order.state.status
                || outcome.version != order.state.version
                || outcome.remaining != 0)
            {
                return Err(Error::Reconciliation("terminal order changed state".into()));
            }
            let spec = version_spec(
                &mut tx,
                outcome.order_id,
                outcome.version,
                d,
                outcome.status == "rejected",
            )
            .await?;
            if outcome.version < order.state.version
                || spec.symbol != order.state.spec.symbol
                || spec.side != order.state.spec.side
                || outcome.remaining > spec.qty_raw
            {
                return Err(Error::Reconciliation("invalid order outcome".into()));
            }
            let closed = matches!(outcome.status.as_str(), "filled" | "cancelled" | "rejected");
            if (!closed && outcome.remaining == 0) || (!closed && outcome.status != "open") {
                return Err(Error::Reconciliation("unknown order state".into()));
            }
            if closed && outcome.remaining != 0 {
                return Err(Error::Reconciliation("closed order has remainder".into()));
            }
            if outcome.status == "filled" && order.state.remaining != 0 {
                return Err(Error::Reconciliation(
                    "missing fills before completion".into(),
                ));
            }
            if !closed {
                let cumulative = order
                    .state
                    .spec
                    .qty_raw
                    .checked_sub(order.state.remaining)
                    .ok_or_else(|| Error::Reconciliation("invalid cumulative quantity".into()))?;
                if outcome.remaining != spec.qty_raw.saturating_sub(cumulative) {
                    return Err(Error::Reconciliation("outcome omits executions".into()));
                }
            }
            let remaining = outcome.remaining;
            let mut target = if closed {
                Amount::zero()
            } else {
                spec.reserve(remaining, &order.fees)?
            };
            // Old-version executions can precede the pending amend in the engine log.
            // Keep coverage for both versions until that command has a durable outcome.
            if !closed
                && !d
                    .commands
                    .iter()
                    .any(|c| Some(c.request_id) == order.pending)
            {
                if let Some(next) = &order.amendment {
                    let filled = spec
                        .qty_raw
                        .checked_sub(remaining)
                        .ok_or_else(|| Error::Reconciliation("invalid amended quantity".into()))?;
                    let next_remaining = next.qty_raw.saturating_sub(filled);
                    target = match spec.side {
                        Side::Buy => amend_buy_reserve(
                            spec.reserve_price(),
                            remaining,
                            next.reserve_price(),
                            next_remaining,
                            order.fees.maker_bps.max(order.fees.taker_bps),
                        )?,
                        Side::Sell => Amount::base(remaining.max(next_remaining)),
                    };
                }
            }
            if target > order.state.held {
                return Err(Error::Reconciliation("unreserved order outcome".into()));
            }
            let released = order.state.held.checked_sub(&target)?;
            if !released.is_zero() {
                let asset = if spec.side == Side::Buy {
                    &order.quote
                } else {
                    &order.base
                };
                postings.push(Posting::new(
                    Some(order.state.account_id),
                    asset,
                    "reserved",
                    released.negated(),
                ));
                postings.push(Posting::new(
                    Some(order.state.account_id),
                    asset,
                    "available",
                    released,
                ));
            }
            order.state.held = target;
            order.state.remaining = remaining;
            order.state.spec = spec;
            order.state.version = outcome.version;
            order.state.status = outcome.status.clone();
        }
        if postings.iter().any(|p| !p.amount.is_zero()) {
            post(
                &mut tx,
                &format!("decision:{}:{}:{}", d.engine_id, d.shard, d.sequence),
                "settlement",
                &serde_json::to_value(d)?,
                postings,
            )
            .await?;
        }
        for order in orders.values() {
            sqlx::query::query("UPDATE orders SET version=$2,spec=$3,remaining=$4::TEXT::NUMERIC,held=$5::TEXT::NUMERIC,status=$6 WHERE id=$1").bind(order.state.id).bind(order.state.version).bind(serde_json::to_value(&order.state.spec)?).bind(order.state.remaining.to_string()).bind(order.state.held.units()).bind(&order.state.status).execute(&mut *tx).await?;
        }
        for result in &d.commands {
            let affected = sqlx::query::query(
                "UPDATE requests SET status=$2 WHERE id=$1 AND status='pending'",
            )
            .bind(result.request_id)
            .bind(if result.accepted {
                "accepted"
            } else {
                "rejected"
            })
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if affected != 1 {
                return Err(Error::Reconciliation(
                    "unknown or completed command outcome".into(),
                ));
            }
            sqlx::query::query("UPDATE outbox SET completed=TRUE WHERE request_id=$1")
                .bind(result.request_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query::query("UPDATE orders SET pending_request=NULL WHERE pending_request=$1")
                .bind(result.request_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query::query("INSERT INTO decisions(engine_id,shard,sequence,digest,payload) VALUES($1,$2,$3::TEXT::NUMERIC,$4,$5)").bind(d.engine_id).bind(d.shard as i32).bind(d.sequence.to_string()).bind(&hash).bind(serde_json::to_value(d)?).execute(&mut *tx).await?;
        sqlx::query::query("UPDATE cursors SET sequence=$3::TEXT::NUMERIC,digest=$4 WHERE engine_id=$1 AND shard=$2").bind(d.engine_id).bind(d.shard as i32).bind(d.sequence.to_string()).bind(hash).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
}
async fn version_spec(
    conn: &mut PgConnection,
    id: i64,
    version: i64,
    decision: &Decision,
    allow_rejection: bool,
) -> Result<OrderSpec> {
    let row=sqlx::query::query("SELECT v.spec,r.id,r.status,r.operation FROM order_versions v JOIN requests r ON r.id=v.request_id WHERE v.order_id=$1 AND v.version=$2")
        .bind(id).bind(version).fetch_optional(conn).await?
        .ok_or_else(|| Error::Reconciliation("unknown order version".into()))?;
    let status: String = row.get("status");
    let accepted_now = decision
        .commands
        .iter()
        .any(|c| c.request_id == row.get::<Uuid, _>("id") && c.accepted);
    let rejected_place_now = allow_rejection
        && row.get::<String, _>("operation") == "place"
        && decision
            .commands
            .iter()
            .any(|c| c.request_id == row.get::<Uuid, _>("id") && !c.accepted);
    if status != "accepted" && !(status == "pending" && (accepted_now || rejected_place_now)) {
        return Err(Error::Reconciliation(
            "execution references an unaccepted order version".into(),
        ));
    }
    Ok(serde_json::from_value(row.get("spec"))?)
}

fn integrity_failure(error: &Error) -> bool {
    match error {
        Error::Database(sqlx::error::Error::Database(e)) => e
            .code()
            .is_some_and(|c| c.starts_with("22") || c.starts_with("23")),
        Error::Database(
            sqlx::error::Error::RowNotFound
            | sqlx::error::Error::ColumnNotFound(_)
            | sqlx::error::Error::ColumnDecode { .. }
            | sqlx::error::Error::Decode(_),
        ) => true,
        Error::Database(_) => false,
        _ => true,
    }
}
