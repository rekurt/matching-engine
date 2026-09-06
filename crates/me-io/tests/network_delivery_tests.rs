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
async fn exercise(tcp: bool) {
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let router = Arc::new(router);
    let store = Arc::new(SessionStore::new(1000, Duration::from_secs(60)));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let address = if tcp {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string()
    } else {
        std::env::temp_dir()
            .join(format!(
                "me-delivery-{}.sock",
                me_core::SessionId::random().log_prefix()
            ))
            .to_str()
            .unwrap()
            .to_owned()
    };
    let addr = address.clone();
    let r = router.clone();
    let ss = store.clone();
    let server = tokio::spawn(async move {
        if tcp {
            TcpServer::new_with_config(r, None, ss, 1000, 5000, Arc::new(me_clock::SystemClock))
                .listen(&addr, rx)
                .await
        } else {
            UdsServer::new_with_config(r, None, ss, 1000, 5000, Arc::new(me_clock::SystemClock))
                .listen(&addr, rx)
                .await
        }
    });
    let mut maker = connect(tcp, &address).await;
    let mut taker = connect(tcp, &address).await;
    send(&mut maker, 5, 1, 0, &[]).await;
    let ack = recv(&mut maker).await;
    assert_eq!(ack.0, 0x1a);
    let mut offset = 1;
    let mut sid = Vec::new();
    while offset < ack.3.len() {
        let tag = u16::from_le_bytes(ack.3[offset..offset + 2].try_into().unwrap());
        let len = u16::from_le_bytes(ack.3[offset + 2..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if tag == 2 {
            sid = ack.3[offset..offset + len].to_vec();
        }
        offset += len;
    }
    assert_eq!(sid.len(), 16);
    let mut duplicate = connect(tcp, &address).await;
    let mut resume_payload = sid.clone();
    resume_payload.extend_from_slice(&0u64.to_le_bytes());
    send(&mut duplicate, 6, 1, 0, &resume_payload).await;
    let mut one = [0u8];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), duplicate.read(&mut one))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    send(&mut taker, 5, 1, 0, &[]).await;
    recv(&mut taker).await;
    send(&mut maker, 1, 2, 10, &place(1, 1)).await;
    let accepted = recv(&mut maker).await;
    assert_eq!(accepted.0, 0x10);
    send(&mut maker, 1, 3, 10, &place(3, 1)).await;
    let rejected_reuse = recv(&mut maker).await;
    assert_eq!(rejected_reuse.1, 0);
    send(&mut maker, 1, 4, 10, &place(1, 1)).await;
    assert_eq!(recv(&mut maker).await, accepted);
    send(&mut taker, 1, 2, 20, &place(2, 0)).await;
    let first = recv(&mut maker).await;
    let second = recv(&mut maker).await;
    assert_eq!((first.0, second.0), (0x12, 0x13));
    assert_eq!((first.2, second.2), (0, 0));
    assert_eq!((first.1, second.1), (accepted.1 + 1, accepted.1 + 2));
    drop(maker);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut resumed = connect(tcp, &address).await;
    let mut payload = sid;
    payload.extend_from_slice(&accepted.1.to_le_bytes());
    send(&mut resumed, 6, 1, 0, &payload).await;
    let resumed_ack = recv(&mut resumed).await;
    assert_eq!((resumed_ack.0, resumed_ack.1), (0x1b, 0));
    assert_eq!(recv(&mut resumed).await, first);
    assert_eq!(recv(&mut resumed).await, second);
    tx.send(true).unwrap();
    server.await.unwrap().unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_maker_delivery_and_exact_replay() {
    exercise(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uds_maker_delivery_and_exact_replay() {
    exercise(false).await;
}
