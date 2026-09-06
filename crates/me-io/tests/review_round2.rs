use me_core::symbol::SymbolConfig;
use me_engine::router::Router;
use me_io::{session_store::SessionStore, tcp_server::TcpServer};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type Socket = Box<dyn Stream>;
async fn send(s: &mut Socket, kind: u8, seq: u64, request: u64, payload: &[u8]) {
    let mut data = (payload.len() as u32).to_le_bytes().to_vec();
    data.extend_from_slice(&[2, kind]);
    data.extend_from_slice(&seq.to_le_bytes());
    data.extend_from_slice(&request.to_le_bytes());
    data.extend_from_slice(payload);
    s.write_all(&data).await.unwrap();
}
async fn recv(s: &mut Socket) -> (u8, u64, u64, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut h = [0; 22];
        s.read_exact(&mut h).await.unwrap();
        let mut payload = vec![0; u32::from_le_bytes(h[..4].try_into().unwrap()) as usize];
        s.read_exact(&mut payload).await.unwrap();
        (
            h[5],
            u64::from_le_bytes(h[6..14].try_into().unwrap()),
            u64::from_le_bytes(h[14..22].try_into().unwrap()),
            payload,
        )
    })
    .await
    .unwrap()
}
fn place(id: u64, side: u8) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&[side, 0, 0]);
    p.extend_from_slice(&10_000_000_000i64.to_le_bytes());
    p.extend_from_slice(&0i64.to_le_bytes());
    p.extend_from_slice(&100_000_000u64.to_le_bytes());
    p
}
async fn connect(tcp: bool, address: &str) -> Socket {
    for _ in 0..100 {
        let result: std::io::Result<Socket> = if tcp {
            tokio::net::TcpStream::connect(address)
                .await
                .map(|s| Box::new(s) as Socket)
        } else {
            tokio::net::UnixStream::connect(address)
                .await
                .map(|s| Box::new(s) as Socket)
        };
        if let Ok(s) = result {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("listener did not start")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_larger_than_replay_capacity_returns_gap_instead_of_hanging() {
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let router = Arc::new(router);
    let store = Arc::new(SessionStore::new(2, Duration::from_secs(60)));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let addr = address.clone();
    let server = tokio::spawn(async move {
        TcpServer::new_with_config(
            router,
            None,
            store,
            1000,
            5000,
            Arc::new(me_clock::SystemClock),
        )
        .listen(&addr, rx)
        .await
    });
    let mut maker = connect(true, &address).await;
    let mut taker = connect(true, &address).await;
    send(&mut maker, 5, 1, 0, &[]).await;
    recv(&mut maker).await;
    send(&mut taker, 5, 1, 0, &[]).await;
    recv(&mut taker).await;
    send(&mut maker, 1, 2, 10, &place(1, 1)).await;
    assert_eq!(recv(&mut maker).await.0, 0x10);
    send(&mut taker, 1, 2, 20, &place(2, 0)).await;
    let result = tokio::time::timeout(Duration::from_millis(500), recv(&mut taker)).await;
    tx.send(true).unwrap();
    server.await.unwrap().unwrap();
    assert!(
        result.is_ok(),
        "a committed command must send an outcome or explicit replay gap, never silently stall"
    );
}

#[test]
fn corrupt_session_journal_is_rejected_before_serving() {
    let path = std::env::temp_dir().join(format!(
        "me-review-corrupt-{}.json",
        me_core::SessionId::random().log_prefix()
    ));
    let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
    let id = store.create();
    store.checkpoint().unwrap();
    drop(store);
    let mut data = std::fs::read(&path).unwrap();
    assert!(data.len() > 8);
    data[8] ^= 1;
    std::fs::write(&path, &data).unwrap();
    let result = SessionStore::open(&path, 32, Duration::from_secs(60));
    std::fs::remove_file(path).unwrap();
    assert!(
        result.is_err(),
        "corrupt session journal must fail recovery"
    );
    let _ = id;
}

#[test]
fn triggered_stop_terminal_rejection_goes_to_stop_owner() {
    use me_core::{MatchEvent, RejectReason};
    use me_engine::router::DurableRequest;
    use me_io::session_store::ReplayResult;
    let store = SessionStore::new(32, Duration::from_secs(60));
    let owner = store.create();
    let requester = store.create();
    let response = store
        .commit_request(DurableRequest {
            session_id: requester,
            request_id: 1,
            item_index: 0,
            expiration_count: 0,
            expiration_owners: vec![],
            owners: vec![(42, owner)],
            events: vec![
                MatchEvent::StopTriggered {
                    order_id: 42,
                    trigger_price: me_core::types::FixedPrice::from_raw(100),
                    timestamp: 1,
                },
                MatchEvent::OrderRejected {
                    order_id: 42,
                    reason: RejectReason::SelfTradePrevented,
                    timestamp: 1,
                },
            ],
        })
        .unwrap();
    let ReplayResult::Ok(frames) = store.resume(&owner, 0) else {
        panic!("owner replay unavailable");
    };
    assert_eq!(
        frames.len(),
        2,
        "stop owner must receive both trigger and terminal rejection; requester got {} bytes",
        response.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_larger_than_replay_capacity_remains_deliverable() {
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let router = Arc::new(router);
    let store = Arc::new(SessionStore::new(2, Duration::from_secs(60)));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let addr = address.clone();
    let server = tokio::spawn(async move {
        TcpServer::new_with_config(
            router,
            None,
            store,
            1000,
            5000,
            Arc::new(me_clock::SystemClock),
        )
        .listen(&addr, rx)
        .await
    });
    let mut maker = connect(true, &address).await;
    let mut taker = connect(true, &address).await;
    send(&mut maker, 5, 1, 0, &[]).await;
    recv(&mut maker).await;
    send(&mut taker, 5, 1, 0, &[]).await;
    recv(&mut taker).await;
    send(&mut maker, 1, 2, 10, &place(1, 1)).await;
    assert_eq!(recv(&mut maker).await.0, 0x10);
    send(&mut maker, 9, 3, 11, &[]).await;
    let result = recv(&mut maker).await;
    tx.send(true).unwrap();
    server.await.unwrap().unwrap();
    assert_eq!(
        result.0, 0x1F,
        "resync must return OpenOrdersBegin, not another gap"
    );
}

#[test]
fn expired_gtd_terminal_event_reaches_owner_outbox() {
    use me_core::{
        types::{FixedPrice, FixedQty, Side, SymbolId},
        Order, TimeInForce,
    };
    use me_engine::command::Command;
    use me_io::session_store::ReplayResult;
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(32, Duration::from_secs(60));
    let owner = store.create();
    let requester = store.create();
    let now = me_clock::Clock::now_ns(&me_clock::SystemClock);
    let mut order = Order::new_limit(
        42,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTD,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        now,
    );
    order.client_session_id = owner;
    order.expire_at = now + 5_000_000;
    router.process_with_request(Command::PlaceOrder(order), owner, 1);
    store
        .commit_request(router.request_record(owner, 1).unwrap().unwrap())
        .unwrap();
    std::thread::sleep(Duration::from_millis(10));
    order.id = 43;
    order.client_session_id = requester;
    order.tif = TimeInForce::GTC;
    order.expire_at = 0;
    router.process_with_request(Command::PlaceOrder(order), requester, 1);
    store
        .commit_request(router.request_record(requester, 1).unwrap().unwrap())
        .unwrap();
    let ReplayResult::Ok(frames) = store.resume(&owner, 0) else {
        panic!("owner replay unavailable");
    };
    router.shutdown();
    assert_eq!(
        frames.len(),
        2,
        "expired GTD owner must receive terminal cancellation after accepted"
    );
}
