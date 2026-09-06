use me_core::symbol::SymbolConfig;
use me_engine::router::Router;
use me_io::{session_store::SessionStore, tcp_server::TcpServer, uds_server::UdsServer};
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

async fn large_snapshot(tcp: bool) {
    use me_core::{
        types::{FixedPrice, FixedQty, Side, SymbolId},
        Order, TimeInForce,
    };
    use me_engine::command::Command;
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let router = Arc::new(router);
    let journal_path = std::env::temp_dir().join(format!(
        "me-network-snapshot-{}",
        me_core::SessionId::random().log_prefix()
    ));
    let store =
        Arc::new(SessionStore::open(&journal_path, 10_000, Duration::from_secs(60)).unwrap());
    let session = store.create();
    for id in 1..=3200 {
        let mut order = Order::new_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1,
        );
        order.client_session_id = session;
        assert!(router
            .process(Command::PlaceOrder(order))
            .iter()
            .any(|e| matches!(e, me_core::MatchEvent::OrderAccepted { .. })));
    }
    let (tx, rx) = tokio::sync::watch::channel(false);
    let address = if tcp {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string()
    } else {
        std::env::temp_dir()
            .join(format!("me-snapshot-{}.sock", session.log_prefix()))
            .to_str()
            .unwrap()
            .to_owned()
    };
    let addr = address.clone();
    let r = router.clone();
    let ss = store.clone();
    let metrics = Arc::new(me_metrics::EngineMetrics::new(&Default::default()));
    let server_metrics = metrics.clone();
    let server = tokio::spawn(async move {
        if tcp {
            TcpServer::new_with_config(
                r,
                Some(server_metrics),
                ss,
                1000,
                5000,
                Arc::new(me_clock::SystemClock),
            )
            .listen(&addr, rx)
            .await
        } else {
            UdsServer::new_with_config(
                r,
                Some(server_metrics),
                ss,
                1000,
                5000,
                Arc::new(me_clock::SystemClock),
            )
            .listen(&addr, rx)
            .await
        }
    });
    let mut socket = connect(tcp, &address).await;
    let mut resume = session.as_bytes().to_vec();
    resume.extend_from_slice(&0u64.to_le_bytes());
    send(&mut socket, 6, 1, 0, &resume).await;
    assert_eq!(recv(&mut socket).await.0, 0x1b);
    let mut original = Vec::new();
    for seq in 2..=3 {
        send(&mut socket, 9, seq, 123, &[]).await;
        let begin = recv(&mut socket).await;
        assert_eq!(
            begin.0, 0x1f,
            "large snapshot must not be disconnected as backpressure"
        );
        assert_eq!(u32::from_le_bytes(begin.3[..4].try_into().unwrap()), 3200);
        let mut frames = vec![begin];
        for _ in 0..3200 {
            let frame = recv(&mut socket).await;
            assert_eq!(frame.0, 0x21);
            frames.push(frame);
        }
        let end = recv(&mut socket).await;
        assert_eq!(end.0, 0x22);
        frames.push(end);
        if seq == 2 {
            original = frames;
        } else {
            assert_eq!(frames, original);
        }
    }
    drop(socket);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut resumed = connect(tcp, &address).await;
    send(&mut resumed, 6, 1, 0, &resume).await;
    let resumed_ack = recv(&mut resumed).await;
    assert_eq!(
        resumed_ack.0, 0x1b,
        "large replay must not trigger backpressure"
    );
    for expected in original {
        assert_eq!(recv(&mut resumed).await, expected);
    }
    let transport = if tcp { "tcp" } else { "uds" };
    tokio::task::yield_now().await;
    assert_eq!(
        metrics
            .connection_frames_out_total
            .with_label_values(&[transport])
            .get(),
        9608
    );
    assert_eq!(
        metrics
            .connection_bytes_out_total
            .with_label_values(&[transport])
            .get(),
        787444
    );
    tx.send(true).unwrap();
    server.await.unwrap().unwrap();
    Arc::try_unwrap(router).ok().unwrap().shutdown();
    let cached = store.dedup_lookup(&session, 123).unwrap();
    drop(store);
    let recovered = SessionStore::open(&journal_path, 10_000, Duration::from_secs(60)).unwrap();
    assert_eq!(recovered.dedup_lookup(&session, 123), Some(cached));
    assert_eq!(recovered.next_outbound_seq(&session), 3203);
    std::fs::remove_file(journal_path).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_large_snapshot_and_retry() {
    large_snapshot(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uds_large_snapshot_and_retry() {
    large_snapshot(false).await;
}
