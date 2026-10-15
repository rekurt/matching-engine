use me_ledger::{Amount, Fees, Funding, Ledger, OrderSpec, Side};
use sqlx::row::Row;
use uuid::Uuid;

#[derive(Clone)]
struct TestLedger {
    ledger: Ledger,
    pool: sqlx_postgres::PgPool,
}
impl std::ops::Deref for TestLedger {
    type Target = Ledger;
    fn deref(&self) -> &Ledger {
        &self.ledger
    }
}
impl TestLedger {
    fn pool(&self) -> &sqlx_postgres::PgPool {
        &self.pool
    }
}
// Each test uses a private schema, including the SQLx migration table.
async fn setup() -> TestLedger {
    let url = std::env::var("LEDGER_TEST_DATABASE_URL")
        .expect("set LEDGER_TEST_DATABASE_URL to run PostgreSQL tests");
    let schema = format!("test_{}", Uuid::new_v4().simple());
    let pool = sqlx_postgres::PgPool::connect(&url).await.unwrap();
    sqlx::query::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let sep = if url.contains('?') { '&' } else { '?' };
    let url = format!("{url}{sep}options=-csearch_path%3D{schema}");
    let ledger = Ledger::connect(&url).await.unwrap();
    let pool = sqlx_postgres::PgPool::connect(&url).await.unwrap();
    let ledger = TestLedger { ledger, pool };
    ledger.migrate().await.unwrap();
    ledger.add_asset("BTC", 8).await.unwrap();
    ledger.add_asset("USDT", 8).await.unwrap();
    ledger
        .add_market(
            0,
            "BTC",
            "USDT",
            &Fees {
                id: Uuid::new_v4(),
                maker_bps: 10,
                taker_bps: 20,
            },
            "fixture",
        )
        .await
        .unwrap();
    ledger
}
fn spec() -> OrderSpec {
    OrderSpec {
        symbol: 0,
        side: Side::Buy,
        order_type: 0,
        time_in_force: 0,
        price_raw: 100_00000000,
        protection_price_raw: 100_00000000,
        qty_raw: 1_00000000,
        stop_price_raw: 0,
        trail_offset_raw: 0,
        display_qty_raw: 0,
        expire_at: 0,
        flags: 0,
    }
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn funding_reservation_idempotency_and_immutable_history() {
    let ledger = setup().await;
    let account = new_account(&ledger).await;
    let funding = Funding {
        account_id: account,
        asset: "USDT".into(),
        amount: "200".parse().unwrap(),
        external_reference: "bank-1".into(),
    };
    let first = ledger.fund(&funding).await.unwrap();
    assert_eq!(ledger.fund(&funding).await.unwrap(), first);
    let order = ledger.place(account, "one", &spec()).await.unwrap();
    assert_eq!(order.held.to_string(), "100.2");
    assert_eq!(
        ledger.place(account, "one", &spec()).await.unwrap().id,
        order.id
    );
    let mut changed = spec();
    changed.qty_raw *= 2;
    assert!(ledger.place(account, "one", &changed).await.is_err());
    assert!(ledger.place(account, "two", &spec()).await.is_err());
    let balance = ledger
        .balances(account)
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.asset == "USDT")
        .unwrap();
    assert_eq!(balance.available.to_string(), "99.8");
    assert_eq!(balance.reserved.to_string(), "100.2");
    let n: i64 = sqlx::query_scalar::query_scalar("SELECT count(*) FROM outbox")
        .fetch_one(ledger.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert!(sqlx::query::query("UPDATE postings SET amount=0")
        .execute(ledger.pool())
        .await
        .is_err());
    assert!(sqlx::query::query("DELETE FROM journals")
        .execute(ledger.pool())
        .await
        .is_err());
    let n:i64=sqlx::query_scalar::query_scalar("SELECT count(*) FROM (SELECT w.asset FROM postings p JOIN wallets w ON w.id=p.wallet_id GROUP BY w.asset HAVING sum(p.amount)<>0) x").fetch_one(ledger.pool()).await.unwrap();
    assert_eq!(n, 0);
}
#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn concurrent_orders_cannot_overspend() {
    let ledger = setup().await;
    let account = new_account(&ledger).await;
    ledger
        .fund(&Funding {
            account_id: account,
            asset: "USDT".into(),
            amount: "100.2".parse().unwrap(),
            external_reference: "one".into(),
        })
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..12 {
        let l = ledger.clone();
        tasks.push(tokio::spawn(async move {
            l.place(account, &i.to_string(), &spec()).await
        }));
    }
    let mut successes = 0;
    for t in tasks {
        if t.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1);
    let row =
        sqlx::query::query("SELECT min(balance)::TEXT AS minimum FROM wallets WHERE account_id=$1")
            .bind(account)
            .fetch_one(ledger.pool())
            .await
            .unwrap();
    assert!(!row.get::<String, _>("minimum").starts_with('-'));
}
#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn database_rejects_unbalanced_or_late_postings() {
    let ledger = setup().await;
    let account = new_account(&ledger).await;
    let id = ledger
        .fund(&Funding {
            account_id: account,
            asset: "USDT".into(),
            amount: Amount::base(1),
            external_reference: "seed".into(),
        })
        .await
        .unwrap();
    let mut tx = ledger.pool().begin().await.unwrap();
    let key: String = sqlx::query_scalar::query_scalar(
        "SELECT wallet_id FROM postings WHERE journal_id=$1 LIMIT 1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(sqlx::query::query(
        "INSERT INTO postings(journal_id,ordinal,wallet_id,amount) VALUES($1,99,$2,1)"
    )
    .bind(id)
    .bind(&key)
    .execute(&mut *tx)
    .await
    .is_err());
    tx.rollback().await.unwrap();
    let mut tx = ledger.pool().begin().await.unwrap();
    let journal = Uuid::new_v4();
    sqlx::query::query("INSERT INTO journals(id,dedup_key,payload_hash,kind,payload) VALUES($1,'bad',repeat('0',64),'bad','{}')").bind(journal).execute(&mut *tx).await.unwrap();
    sqlx::query::query(
        "INSERT INTO postings(journal_id,ordinal,wallet_id,amount) VALUES($1,0,$2,1)",
    )
    .bind(journal)
    .bind(key)
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(tx.commit().await.is_err());
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn atomic_two_sided_settlement_and_replay() {
    use me_ledger::{CommandOutcome, Decision, Fill, OrderOutcome};
    let ledger = setup().await;
    let buyer = new_account(&ledger).await;
    let seller = new_account(&ledger).await;
    ledger
        .fund(&Funding {
            account_id: buyer,
            asset: "USDT".into(),
            amount: "1000".parse().unwrap(),
            external_reference: "buyer".into(),
        })
        .await
        .unwrap();
    ledger
        .fund(&Funding {
            account_id: seller,
            asset: "BTC".into(),
            amount: "10".parse().unwrap(),
            external_reference: "seller".into(),
        })
        .await
        .unwrap();
    let buy = ledger.place(buyer, "buy", &spec()).await.unwrap();
    let mut sell_spec = spec();
    sell_spec.side = Side::Sell;
    sell_spec.price_raw = 90_00000000;
    sell_spec.qty_raw = 2_00000000;
    let sell = ledger.place(seller, "sell", &sell_spec).await.unwrap();
    let engine = Uuid::new_v4();
    ledger.bind_engine(engine, "fixture").await.unwrap();
    let buy_request: Uuid =
        sqlx::query_scalar::query_scalar("SELECT pending_request FROM orders WHERE id=$1")
            .bind(buy.id)
            .fetch_one(ledger.pool())
            .await
            .unwrap();
    let sell_request: Uuid =
        sqlx::query_scalar::query_scalar("SELECT pending_request FROM orders WHERE id=$1")
            .bind(sell.id)
            .fetch_one(ledger.pool())
            .await
            .unwrap();
    let mut first = Decision {
        engine_id: engine,
        shard: 0,
        sequence: 1,
        previous_digest: "0".repeat(64),
        bindings: vec![],
        commands: vec![CommandOutcome {
            request_id: buy_request,
            accepted: true,
        }],
        fills: vec![],
        orders: vec![OrderOutcome {
            order_id: buy.id,
            version: 1,
            remaining: 1_00000000,
            status: "open".into(),
        }],
    };
    attach_bindings(&ledger, &mut first).await;
    assert!(ledger.settle(&first).await.unwrap());
    let mut second = Decision {
        engine_id: engine,
        shard: 0,
        sequence: 2,
        previous_digest: first.digest().unwrap(),
        bindings: vec![],
        commands: vec![CommandOutcome {
            request_id: sell_request,
            accepted: true,
        }],
        fills: vec![Fill {
            maker_order: buy.id,
            taker_order: sell.id,
            maker_version: 1,
            taker_version: 1,
            price_raw: 100_00000000,
            qty_raw: 1_00000000,
        }],
        orders: vec![
            OrderOutcome {
                order_id: buy.id,
                version: 1,
                remaining: 0,
                status: "filled".into(),
            },
            OrderOutcome {
                order_id: sell.id,
                version: 1,
                remaining: 1_00000000,
                status: "open".into(),
            },
        ],
    };
    attach_bindings(&ledger, &mut second).await;
    assert!(ledger.settle(&second).await.unwrap());
    assert!(!ledger.settle(&second).await.unwrap());
    assert_eq!(
        ledger.cursor(engine, 0).await.unwrap(),
        (2, second.digest().unwrap())
    );
    let b = ledger.balances(buyer).await.unwrap();
    let s = ledger.balances(seller).await.unwrap();
    assert_eq!(
        b.iter()
            .find(|x| x.asset == "BTC")
            .unwrap()
            .available
            .to_string(),
        "1"
    );
    assert_eq!(
        b.iter()
            .find(|x| x.asset == "USDT")
            .unwrap()
            .available
            .to_string(),
        "899.9"
    );
    assert_eq!(
        s.iter()
            .find(|x| x.asset == "USDT")
            .unwrap()
            .available
            .to_string(),
        "99.8"
    );
    assert_eq!(
        s.iter()
            .find(|x| x.asset == "BTC")
            .unwrap()
            .reserved
            .to_string(),
        "1"
    );
    let fees: String = sqlx::query_scalar::query_scalar(
        "SELECT balance::TEXT FROM wallets WHERE bucket='fees' AND asset='USDT'",
    )
    .fetch_one(ledger.pool())
    .await
    .unwrap();
    assert_eq!(fees, "30000000000000000000");
    let mut wrong = second.clone();
    wrong.fills[0].qty_raw += 1;
    assert!(ledger.settle(&wrong).await.is_err());
    let mut gap = second.clone();
    gap.sequence = 4;
    assert!(ledger.settle(&gap).await.is_err());
    assert_eq!(ledger.cursor(engine, 0).await.unwrap().0, 2);
}

async fn seed(ledger: &Ledger, asset: &str, amount: &str) -> Uuid {
    let account = new_account(ledger).await;
    ledger
        .fund(&Funding {
            account_id: account,
            asset: asset.into(),
            amount: amount.parse().unwrap(),
            external_reference: account.to_string(),
        })
        .await
        .unwrap();
    account
}
async fn pending(ledger: &TestLedger, order: i64) -> Uuid {
    sqlx::query_scalar::query_scalar("SELECT pending_request FROM orders WHERE id=$1")
        .bind(order)
        .fetch_one(ledger.pool())
        .await
        .unwrap()
}
fn outcome(id: i64, version: i64, remaining: u64, status: &str) -> me_ledger::OrderOutcome {
    me_ledger::OrderOutcome {
        order_id: id,
        version,
        remaining,
        status: status.into(),
    }
}
async fn apply(
    ledger: &TestLedger,
    engine: Uuid,
    command: Option<(Uuid, bool)>,
    fills: Vec<me_ledger::Fill>,
    orders: Vec<me_ledger::OrderOutcome>,
) {
    let (seq, digest) = ledger.cursor(engine, 0).await.unwrap();
    let mut decision = me_ledger::Decision {
        engine_id: engine,
        shard: 0,
        sequence: seq + 1,
        previous_digest: digest,
        bindings: vec![],
        commands: command
            .into_iter()
            .map(|(request_id, accepted)| me_ledger::CommandOutcome {
                request_id,
                accepted,
            })
            .collect(),
        fills,
        orders,
    };
    attach_bindings(ledger, &mut decision).await;
    ledger.settle(&decision).await.unwrap();
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn amendment_covers_old_fills_until_new_version_is_confirmed() {
    use me_ledger::{Amend, Fill};
    let ledger = setup().await;
    let buyer = seed(&ledger, "USDT", "10000").await;
    let seller = seed(&ledger, "BTC", "20").await;
    let engine = Uuid::new_v4();
    ledger.bind_engine(engine, "fixture").await.unwrap();
    let mut buy_spec = spec();
    buy_spec.price_raw = 200_00000000;
    buy_spec.qty_raw = 10_00000000;
    let buy = ledger.place(buyer, "buy", &buy_spec).await.unwrap();
    apply(
        &ledger,
        engine,
        Some((pending(&ledger, buy.id).await, true)),
        vec![],
        vec![outcome(buy.id, 1, buy_spec.qty_raw, "open")],
    )
    .await;
    let amend = Amend {
        price_raw: Some(150_00000000),
        qty_raw: Some(20_00000000),
    };
    let request = ledger
        .amend(buyer, "amend", buy.id, amend.clone())
        .await
        .unwrap();
    assert_eq!(
        request,
        ledger.amend(buyer, "amend", buy.id, amend).await.unwrap()
    );
    assert_eq!(
        ledger.order(buyer, buy.id).await.unwrap().held.to_string(),
        "3507"
    );
    assert!(ledger.cancel(buyer, "cancel", buy.id).await.is_err());
    let mut sell_spec = spec();
    sell_spec.side = Side::Sell;
    sell_spec.qty_raw = 5_00000000;
    let sell = ledger.place(seller, "sell", &sell_spec).await.unwrap();
    apply(
        &ledger,
        engine,
        Some((pending(&ledger, sell.id).await, true)),
        vec![Fill {
            maker_order: buy.id,
            taker_order: sell.id,
            maker_version: 1,
            taker_version: 1,
            price_raw: 200_00000000,
            qty_raw: 5_00000000,
        }],
        vec![
            outcome(buy.id, 1, 5_00000000, "open"),
            outcome(sell.id, 1, 0, "filled"),
        ],
    )
    .await;
    // Both five old fills at 200 and ten subsequent fills at 150 remain possible.
    assert_eq!(
        ledger.order(buyer, buy.id).await.unwrap().held.to_string(),
        "2505"
    );
    sell_spec.price_raw = 150_00000000;
    sell_spec.qty_raw = 15_00000000;
    let ask = ledger.place(seller, "ask", &sell_spec).await.unwrap();
    apply(
        &ledger,
        engine,
        Some((pending(&ledger, ask.id).await, true)),
        vec![],
        vec![outcome(ask.id, 1, 15_00000000, "open")],
    )
    .await;
    apply(
        &ledger,
        engine,
        Some((request, true)),
        vec![Fill {
            maker_order: ask.id,
            taker_order: buy.id,
            maker_version: 1,
            taker_version: 2,
            price_raw: 150_00000000,
            qty_raw: 15_00000000,
        }],
        vec![
            outcome(ask.id, 1, 0, "filled"),
            outcome(buy.id, 2, 0, "filled"),
        ],
    )
    .await;
    let order = ledger.order(buyer, buy.id).await.unwrap();
    assert_eq!(order.held, Amount::zero());
    assert_eq!(order.version, 2);
    let balances = ledger.balances(buyer).await.unwrap();
    assert_eq!(
        balances
            .iter()
            .find(|b| b.asset == "USDT")
            .unwrap()
            .available
            .to_string(),
        "6744.5"
    );
    assert_eq!(
        balances
            .iter()
            .find(|b| b.asset == "BTC")
            .unwrap()
            .available
            .to_string(),
        "20"
    );
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn cancellation_keeps_reserve_until_durable_confirmation() {
    let ledger = setup().await;
    let account = seed(&ledger, "USDT", "1000").await;
    let engine = Uuid::new_v4();
    ledger.bind_engine(engine, "fixture").await.unwrap();
    let order = ledger.place(account, "place", &spec()).await.unwrap();
    apply(
        &ledger,
        engine,
        Some((pending(&ledger, order.id).await, true)),
        vec![],
        vec![outcome(order.id, 1, 1_00000000, "open")],
    )
    .await;
    let cancel = ledger.cancel(account, "cancel", order.id).await.unwrap();
    assert_eq!(
        cancel,
        ledger.cancel(account, "cancel", order.id).await.unwrap()
    );
    assert_eq!(
        ledger
            .order(account, order.id)
            .await
            .unwrap()
            .held
            .to_string(),
        "100.2"
    );
    apply(
        &ledger,
        engine,
        Some((cancel, true)),
        vec![],
        vec![outcome(order.id, 1, 0, "cancelled")],
    )
    .await;
    assert_eq!(
        ledger.order(account, order.id).await.unwrap().held,
        Amount::zero()
    );
    assert_eq!(
        serde_json::to_value(&order).unwrap(),
        serde_json::to_value(ledger.place(account, "place", &spec()).await.unwrap()).unwrap()
    );
    assert_eq!(
        ledger
            .balances(account)
            .await
            .unwrap()
            .iter()
            .find(|b| b.asset == "USDT")
            .unwrap()
            .available
            .to_string(),
        "1000"
    );
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn external_transfers_preserve_dust_and_replay_after_suspension() {
    let ledger = setup().await;
    let account = seed(&ledger, "USDT", "10").await;
    let transfer = Funding {
        account_id: account,
        asset: "USDT".into(),
        amount: "1.00000001".parse().unwrap(),
        external_reference: "withdrawal".into(),
    };
    let id = ledger.withdraw(&transfer).await.unwrap();
    assert_eq!(id, ledger.withdraw(&transfer).await.unwrap());
    assert!(ledger.fund(&transfer).await.is_err());
    let mut too_precise = transfer.clone();
    too_precise.amount = "0.000000001".parse().unwrap();
    too_precise.external_reference = "precision".into();
    assert!(ledger.withdraw(&too_precise).await.is_err());
    sqlx::query::query("UPDATE accounts SET active=FALSE WHERE id=$1")
        .bind(account)
        .execute(ledger.pool())
        .await
        .unwrap();
    assert_eq!(id, ledger.withdraw(&transfer).await.unwrap());
    let b = ledger.balances(account).await.unwrap();
    assert_eq!(
        b.iter()
            .find(|b| b.asset == "USDT")
            .unwrap()
            .available
            .to_string(),
        "8.99999999"
    );
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn rejected_amendment_cannot_supply_an_execution_version() {
    use me_ledger::{Amend, Decision, Fill};
    let ledger = setup().await;
    let engine = Uuid::new_v4();
    ledger.bind_engine(engine, "fixture").await.unwrap();
    let buyer = seed(&ledger, "USDT", "1000").await;
    let seller = seed(&ledger, "BTC", "1").await;
    let buy = ledger.place(buyer, "buy", &spec()).await.unwrap();
    apply(
        &ledger,
        engine,
        Some((pending(&ledger, buy.id).await, true)),
        vec![],
        vec![outcome(buy.id, 1, 1_00000000, "open")],
    )
    .await;
    let amend = ledger
        .amend(
            buyer,
            "amend",
            buy.id,
            Amend {
                price_raw: Some(200_00000000),
                qty_raw: None,
            },
        )
        .await
        .unwrap();
    apply(
        &ledger,
        engine,
        Some((amend, false)),
        vec![],
        vec![outcome(buy.id, 1, 1_00000000, "open")],
    )
    .await;
    let mut ask = spec();
    ask.side = Side::Sell;
    ask.qty_raw = 50_000000;
    ask.price_raw = 150_00000000;
    let sell = ledger.place(seller, "sell", &ask).await.unwrap();
    apply(
        &ledger,
        engine,
        Some((pending(&ledger, sell.id).await, true)),
        vec![],
        vec![outcome(sell.id, 1, 50_000000, "open")],
    )
    .await;
    let (seq, digest) = ledger.cursor(engine, 0).await.unwrap();
    let before = serde_json::to_value(ledger.balances(buyer).await.unwrap()).unwrap();
    let mut invalid = Decision {
        engine_id: engine,
        shard: 0,
        sequence: seq + 1,
        previous_digest: digest,
        bindings: vec![],
        commands: vec![],
        fills: vec![Fill {
            maker_order: sell.id,
            taker_order: buy.id,
            maker_version: 1,
            taker_version: 2,
            price_raw: 150_00000000,
            qty_raw: 50_000000,
        }],
        orders: vec![
            outcome(buy.id, 2, 50_000000, "open"),
            outcome(sell.id, 1, 0, "filled"),
        ],
    };
    attach_bindings(&ledger, &mut invalid).await;
    assert!(matches!(
        ledger.settle(&invalid).await,
        Err(me_ledger::Error::Reconciliation(_))
    ));
    assert_eq!(
        serde_json::to_value(ledger.balances(buyer).await.unwrap()).unwrap(),
        before
    );
    assert_eq!(ledger.cursor(engine, 0).await.unwrap().0, seq);
    assert!(ledger.place(buyer, "blocked", &spec()).await.is_err());
    assert_eq!(
        serde_json::to_value(ledger.place(buyer, "buy", &spec()).await.unwrap()).unwrap(),
        serde_json::to_value(buy).unwrap()
    );
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database"]
async fn gateway_lease_outbox_bytes_and_accounting_audit() {
    let ledger = setup().await;
    let account = seed(&ledger, "USDT", "1000").await;
    let mut lease = ledger.acquire_gateway().await.unwrap();
    lease.verify().await.unwrap();
    assert!(ledger.acquire_gateway().await.is_err());
    let engine = Uuid::new_v4();
    ledger.bind_engine(engine, "fixture").await.unwrap();
    let order = ledger.place(account, "buy", &spec()).await.unwrap();
    let items = ledger.outbox(1).await.unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.command.order_id, order.id);
    assert_eq!(item.command.request_id, item.request_id);
    let bytes = serde_json::to_vec(&item.command).unwrap();
    assert_eq!(
        ledger.save_frame(item.request_id, &bytes).await.unwrap(),
        bytes
    );
    assert!(ledger
        .save_frame(item.request_id, b"changed")
        .await
        .is_err());
    ledger.audit().await.unwrap();
    ledger.set_maintenance(true).await.unwrap();
    assert!(ledger.place(account, "paused", &spec()).await.is_err());
    apply(
        &ledger,
        engine,
        Some((item.request_id, true)),
        vec![],
        vec![outcome(order.id, 1, 1_00000000, "open")],
    )
    .await;
    assert!(ledger.outbox(1).await.unwrap().is_empty());
    ledger.audit().await.unwrap();
    ledger.set_maintenance(false).await.unwrap();
    sqlx::query::query(
        "UPDATE wallets SET balance=balance+1 WHERE account_id=$1 AND bucket='available'",
    )
    .bind(account)
    .execute(ledger.pool())
    .await
    .unwrap();
    assert!(ledger.audit().await.is_err());
    let backend: i32 = sqlx::query_scalar::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype='advisory' AND objsubid=1 AND ((classid::BIGINT<<32)|objid::BIGINT)=hashtextextended(current_database()||':'||current_schema()||':me-gateway',0)",
    )
    .fetch_one(ledger.pool())
    .await
    .unwrap();
    sqlx::query::query("SELECT pg_terminate_backend($1)")
        .bind(backend)
        .execute(ledger.pool())
        .await
        .unwrap();
    assert!(lease.verify().await.is_err());
    assert!(lease.verify().await.is_err());
}

async fn attach_bindings(ledger: &TestLedger, d: &mut me_ledger::Decision) {
    let mut versions = std::collections::BTreeSet::new();
    for f in &d.fills {
        versions.insert((f.maker_order, f.maker_version));
        versions.insert((f.taker_order, f.taker_version));
    }
    for o in &d.orders {
        versions.insert((o.order_id, o.version));
    }
    for (order_id, version) in versions {
        let row=sqlx::query::query("SELECT o.account_id,o.fee_schedule,v.request_id FROM orders o JOIN order_versions v ON v.order_id=o.id WHERE o.id=$1 AND v.version=$2").bind(order_id).bind(version).fetch_one(ledger.pool()).await.unwrap();
        d.bindings.push(me_ledger::OrderBinding {
            order_id,
            version,
            account_id: row.get("account_id"),
            fee_schedule: row.get("fee_schedule"),
            reservation_id: row.get("request_id"),
        });
    }
}

async fn new_account(ledger: &Ledger) -> Uuid {
    ledger
        .provision_account(&Uuid::new_v4().to_string(), &serde_json::json!({}))
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires LEDGER_TEST_DATABASE_URL"]
async fn fee_version_changes_require_maintenance_and_preserve_existing_orders() {
    let ledger = setup().await;
    let engine = Uuid::new_v4();
    ledger.bind_engine(engine, "fixture").await.unwrap();
    let account = seed(&ledger, "USDT", "10000").await;
    let spec = spec();
    let before = ledger
        .place(account, "before-fee-change", &spec)
        .await
        .unwrap();
    let fees = Fees {
        id: Uuid::new_v4(),
        maker_bps: 100,
        taker_bps: 200,
    };
    assert!(ledger
        .add_market(0, "BTC", "USDT", &fees, "fixture")
        .await
        .is_err());
    ledger.set_maintenance(true).await.unwrap();
    ledger
        .add_market(0, "BTC", "USDT", &fees, "fixture")
        .await
        .unwrap();
    ledger.set_maintenance(false).await.unwrap();
    let after = ledger
        .place(account, "after-fee-change", &spec)
        .await
        .unwrap();
    assert_eq!(before.held.to_string(), "100.2");
    assert_eq!(after.held.to_string(), "102");
    let fee_ids: Vec<Uuid> =
        sqlx::query_scalar::query_scalar("SELECT fee_schedule FROM orders ORDER BY id")
            .fetch_all(ledger.pool())
            .await
            .unwrap();
    assert_ne!(fee_ids[0], fee_ids[1]);
    assert_eq!(fee_ids[1], fees.id);
    let seller = seed(&ledger, "BTC", "2").await;
    let mut sell_spec = spec.clone();
    sell_spec.side = Side::Sell;
    sell_spec.qty_raw = 2_00000000;
    let sell = ledger
        .place(seller, "sell-across-fee-versions", &sell_spec)
        .await
        .unwrap();
    let mut commands = Vec::new();
    let mut outcomes = Vec::new();
    for id in [before.id, after.id, sell.id] {
        let request: Uuid =
            sqlx::query_scalar::query_scalar("SELECT pending_request FROM orders WHERE id=$1")
                .bind(id)
                .fetch_one(ledger.pool())
                .await
                .unwrap();
        commands.push(me_ledger::CommandOutcome {
            request_id: request,
            accepted: true,
        });
        outcomes.push(me_ledger::OrderOutcome {
            order_id: id,
            version: 1,
            remaining: 0,
            status: "filled".into(),
        });
    }
    let mut decision = me_ledger::Decision {
        engine_id: engine,
        shard: 0,
        sequence: 1,
        previous_digest: "0".repeat(64),
        commands,
        fills: [before.id, after.id]
            .into_iter()
            .map(|id| me_ledger::Fill {
                maker_order: id,
                taker_order: sell.id,
                maker_version: 1,
                taker_version: 1,
                price_raw: 100_00000000,
                qty_raw: 1_00000000,
            })
            .collect(),
        orders: outcomes,
        bindings: vec![],
    };
    attach_bindings(&ledger, &mut decision).await;
    assert!(ledger.settle(&decision).await.unwrap());
    assert!(!ledger.settle(&decision).await.unwrap());
    let buy_balances = ledger.balances(account).await.unwrap();
    assert_eq!(buy_balances[0].available.to_string(), "2");
    assert_eq!(buy_balances[1].available.to_string(), "9798.9");
    assert_eq!(
        ledger.balances(seller).await.unwrap()[1]
            .available
            .to_string(),
        "196"
    );
    let collected: String = sqlx::query_scalar::query_scalar(
        "SELECT balance::TEXT FROM wallets WHERE bucket='fees' AND asset='USDT'",
    )
    .fetch_one(ledger.pool())
    .await
    .unwrap();
    assert_eq!(
        Amount::from_units(collected.parse().unwrap())
            .unwrap()
            .to_string(),
        "5.1"
    );
}

#[tokio::test]
#[ignore = "requires LEDGER_TEST_DATABASE_URL"]
async fn incompatible_schema_and_changed_migration_fail_closed() {
    let ledger = setup().await;
    sqlx::query::query("UPDATE financial_schema SET version=2")
        .execute(ledger.pool())
        .await
        .unwrap();
    assert!(ledger.migrate().await.is_err());
    sqlx::query::query("UPDATE financial_schema SET version=1")
        .execute(ledger.pool())
        .await
        .unwrap();
    sqlx::query::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex')")
        .execute(ledger.pool())
        .await
        .unwrap();
    assert!(ledger.migrate().await.is_err());
}
