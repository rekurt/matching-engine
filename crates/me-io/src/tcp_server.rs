use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info, info_span, warn, Instrument};

use me_engine::router::ShardedRouterV2;
use me_metrics::EngineMetrics;

use crate::codec::{self, ConnectionCodecState, DecodedFrame};
use crate::protocol::{
    ConnectionState, ErrorCode, MAX_WRITE_BUFFER_SIZE, PROTOCOL_VERSION,
    CAP_BATCH_ORDERS, CAP_HEARTBEAT, CAP_ORDER_FLAGS, CAP_ORDER_QUERY,
    CAP_SESSION_RECOVERY,
};
use crate::session_store::SessionStore;

#[cfg(feature = "tls")]
use tokio_rustls::{rustls, TlsAcceptor};

/// TCP server for the binary protocol.
///
/// Architecture:
/// - tokio for async accept + read/write (I/O multiplexing only)
/// - Matching logic is synchronous via ShardedRouterV2 (crossbeam channels)
/// - Each TCP connection is handled in its own tokio task
/// - Graceful shutdown via a `watch` channel (SIGTERM/SIGINT)
pub struct TcpServer {
    router: Arc<ShardedRouterV2>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    #[cfg(feature = "tls")]
    tls_acceptor: Option<TlsAcceptor>,
}

impl TcpServer {
    pub fn new(router: Arc<ShardedRouterV2>) -> Self {
        Self::new_with_config(router, None, Arc::new(SessionStore::new(1000, Duration::from_secs(60))), 5_000, 15_000)
    }

    pub fn new_with_metrics(
        router: Arc<ShardedRouterV2>,
        metrics: Option<Arc<EngineMetrics>>,
    ) -> Self {
        Self::new_with_config(router, metrics, Arc::new(SessionStore::new(1000, Duration::from_secs(60))), 5_000, 15_000)
    }

    pub fn new_with_config(
        router: Arc<ShardedRouterV2>,
        metrics: Option<Arc<EngineMetrics>>,
        session_store: Arc<SessionStore>,
        heartbeat_interval_ms: u32,
        heartbeat_timeout_ms: u64,
    ) -> Self {
        Self {
            router,
            metrics,
            session_store,
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
            #[cfg(feature = "tls")]
            tls_acceptor: None,
        }
    }

    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, acceptor: TlsAcceptor) -> Self {
        self.tls_acceptor = Some(acceptor);
        self
    }

    /// Start listening on the given address.
    ///
    /// `shutdown_rx` is a watch channel that signals when the server should stop
    /// accepting new connections and drain existing ones.
    pub async fn listen(
        &self,
        addr: &str,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!(addr = addr, "TCP server listening");

        #[cfg(feature = "tls")]
        let tls_active = self.tls_acceptor.is_some();
        #[cfg(not(feature = "tls"))]
        let tls_active = false;

        let caps = build_capabilities(tls_active);

        loop {
            tokio::select! {
                biased;

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("TCP server shutting down");
                        break;
                    }
                }

                result = listener.accept() => {
                    let (stream, peer) = result?;
                    let router = Arc::clone(&self.router);
                    let metrics = self.metrics.clone();
                    let conn_shutdown = shutdown_rx.clone();
                    let session_store = Arc::clone(&self.session_store);
                    let hb_interval = self.heartbeat_interval_ms;
                    let hb_timeout = self.heartbeat_timeout_ms;

                    #[cfg(feature = "tls")]
                    let tls_acceptor = self.tls_acceptor.clone();

                    let span = info_span!("tcp_conn", peer = %peer);
                    tokio::spawn(async move {
                        #[cfg(feature = "tls")]
                        let result = if let Some(acceptor) = tls_acceptor {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    handle_connection(tls_stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps).await
                                }
                                Err(e) => {
                                    warn!(error = %e, "TLS handshake failed");
                                    Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e.to_string()))
                                }
                            }
                        } else {
                            handle_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps).await
                        };

                        #[cfg(not(feature = "tls"))]
                        let result = handle_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps).await;

                        if let Err(e) = result {
                            warn!(error = %e, "connection error");
                        }
                    }.instrument(span));
                }
            }
        }

        Ok(())
    }
}

fn build_capabilities(tls_active: bool) -> u64 {
    let mut caps = CAP_SESSION_RECOVERY | CAP_HEARTBEAT | CAP_BATCH_ORDERS | CAP_ORDER_FLAGS | CAP_ORDER_QUERY;
    if tls_active {
        caps |= crate::protocol::CAP_TLS;
    }
    caps
}

async fn handle_connection<S>(
    mut stream: S,
    router: Arc<ShardedRouterV2>,
    mut shutdown_rx: watch::Receiver<bool>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Some(ref m) = metrics {
        m.record_connection_open("tcp");
    }
    info!("client connected");

    let result = handle_connection_inner(
        &mut stream,
        router,
        &mut shutdown_rx,
        &metrics,
        &session_store,
        heartbeat_interval_ms,
        heartbeat_timeout_ms,
        capabilities,
    )
    .await;

    if let Some(ref m) = metrics {
        m.record_connection_close("tcp");
    }
    info!("client disconnected");

    result
}

async fn handle_connection_inner<S>(
    stream: &mut S,
    router: Arc<ShardedRouterV2>,
    shutdown_rx: &mut watch::Receiver<bool>,
    metrics: &Option<Arc<EngineMetrics>>,
    session_store: &Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut read_buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);
    let mut conn = ConnectionCodecState::new();
    let timeout = Duration::from_millis(heartbeat_timeout_ms);

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = ConnectionState::Closing;
                    write_buf.clear();
                    codec::encode_error(
                        0,
                        ErrorCode::ServerShuttingDown,
                        "Server shutting down",
                        &mut write_buf,
                    );
                    let _ = stream.write_all(&write_buf).await;
                    return Ok(());
                }
            }

            read_result = tokio::time::timeout(timeout, stream.read_buf(&mut read_buf)) => {
                match read_result {
                    Err(_elapsed) => {
                        // No data received within the heartbeat timeout window.
                        write_buf.clear();
                        codec::encode_error(
                            0,
                            ErrorCode::HeartbeatTimeout,
                            "Heartbeat timeout",
                            &mut write_buf,
                        );
                        if let Some(ref m) = metrics {
                            m.record_protocol_error("heartbeat_timeout");
                        }
                        let _ = stream.write_all(&write_buf).await;
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Heartbeat timeout",
                        ));
                    }
                    Ok(Err(e)) => return Err(e),
                    Ok(Ok(0)) => return Ok(()), // Connection closed by client
                    Ok(Ok(n)) => {
                        if let Some(ref m) = metrics {
                            m.record_bytes_in("tcp", n);
                        }

                        loop {
                            match codec::decode_frame(&read_buf, &mut conn) {
                                Ok((frame, consumed)) => {
                                    read_buf.advance(consumed);

                                    if let Some(ref m) = metrics {
                                        m.record_frame_in("tcp");
                                    }

                                    write_buf.clear();
                                    match frame {
                                        DecodedFrame::Handshake { client_version, seq } => {
                                            debug!(version = client_version, "handshake received");
                                            if client_version != PROTOCOL_VERSION {
                                                codec::encode_error(
                                                    seq,
                                                    ErrorCode::UnsupportedVersion,
                                                    &format!(
                                                        "Server requires protocol v{PROTOCOL_VERSION}, client sent v{client_version}"
                                                    ),
                                                    &mut write_buf,
                                                );
                                                stream.write_all(&write_buf).await?;
                                                if let Some(ref m) = metrics {
                                                    m.record_bytes_out("tcp", write_buf.len());
                                                    m.record_frame_out("tcp", 1);
                                                }
                                                return Ok(());
                                            }
                                            let session_id = session_store.create();
                                            conn.state = ConnectionState::Active;
                                            conn.session_id = session_id;
                                            codec::encode_handshake_ack(
                                                seq,
                                                session_id,
                                                capabilities,
                                                heartbeat_interval_ms,
                                                &mut write_buf,
                                            );
                                        }
                                        DecodedFrame::ResumeSession { session_id, last_recv_seq, seq } => {
                                            match session_store.resume(&session_id, last_recv_seq) {
                                                Some(replay_frames) => {
                                                    let count = replay_frames.len() as u32;
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = session_id;
                                                    codec::encode_session_resumed(count, seq, &mut write_buf);
                                                    codec::encode_replay_frames(&replay_frames, &mut write_buf);
                                                    debug!(replayed = count, "session resumed");
                                                }
                                                None => {
                                                    // Session expired or unknown — fall back to new handshake
                                                    let new_id = session_store.create();
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = new_id;
                                                    codec::encode_handshake_ack(
                                                        seq,
                                                        new_id,
                                                        capabilities,
                                                        heartbeat_interval_ms,
                                                        &mut write_buf,
                                                    );
                                                    debug!("session not found, issuing new handshake ack");
                                                }
                                            }
                                        }
                                        DecodedFrame::Ping(seq) => {
                                            codec::encode_pong(seq, &mut write_buf);
                                        }
                                        DecodedFrame::Command(cmd, seq) => {
                                            let events = router.process(cmd);
                                            codec::encode_events(&events, seq, &mut write_buf);
                                        }
                                        DecodedFrame::Batch(cmds, seq) => {
                                            for cmd in cmds {
                                                let events = router.process(cmd);
                                                codec::encode_events(&events, seq, &mut write_buf);
                                            }
                                        }
                                        DecodedFrame::QueryOrder { order_id, symbol_id, seq } => {
                                            let symbol = me_core::types::SymbolId(symbol_id);
                                            let events = router.query_order(order_id, symbol);
                                            codec::encode_events(&events, seq, &mut write_buf);
                                        }
                                    }

                                    // Push outbound frame to session store for potential replay.
                                    if !write_buf.is_empty() && conn.state == ConnectionState::Active {
                                        let frame_bytes = Bytes::copy_from_slice(&write_buf);
                                        session_store.push(&conn.session_id, conn.server_frame_seq, frame_bytes);
                                        conn.server_frame_seq += 1;
                                    }

                                    if write_buf.len() > MAX_WRITE_BUFFER_SIZE {
                                        warn!("backpressure disconnect");
                                        if let Some(ref m) = metrics {
                                            m.record_protocol_error("backpressure");
                                        }
                                        let mut err_buf = BytesMut::with_capacity(64);
                                        codec::encode_error(
                                            0,
                                            ErrorCode::Backpressure,
                                            "Write buffer overflow",
                                            &mut err_buf,
                                        );
                                        let _ = stream.write_all(&err_buf).await;
                                        return Err(std::io::Error::new(
                                            std::io::ErrorKind::WriteZero,
                                            "Write buffer backpressure limit exceeded",
                                        ));
                                    }

                                    if let Some(ref m) = metrics {
                                        m.record_bytes_out("tcp", write_buf.len());
                                        m.record_frame_out("tcp", 1);
                                    }
                                    stream.write_all(&write_buf).await?;
                                }
                                Err(codec::DecodeError::Incomplete) => break,
                                Err(codec::DecodeError::SequenceOutOfOrder { expected_min, got }) => {
                                    warn!(error = "sequence out of order", "protocol decode error");
                                    if let Some(ref m) = metrics {
                                        m.record_protocol_error("sequence_out_of_order");
                                    }
                                    write_buf.clear();
                                    codec::encode_error(
                                        got,
                                        ErrorCode::SequenceOutOfOrder,
                                        &format!("Expected seq >= {expected_min}, got {got}"),
                                        &mut write_buf,
                                    );
                                    if let Some(ref m) = metrics {
                                        m.record_bytes_out("tcp", write_buf.len());
                                        m.record_frame_out("tcp", 1);
                                    }
                                    stream.write_all(&write_buf).await?;
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        "Sequence out of order",
                                    ));
                                }
                                Err(codec::DecodeError::HandshakeRequired) => {
                                    warn!(error = "handshake required", "protocol decode error");
                                    if let Some(ref m) = metrics {
                                        m.record_protocol_error("handshake_required");
                                    }
                                    write_buf.clear();
                                    codec::encode_error(
                                        0,
                                        ErrorCode::HandshakeRequired,
                                        "Send a Handshake message first",
                                        &mut write_buf,
                                    );
                                    if let Some(ref m) = metrics {
                                        m.record_bytes_out("tcp", write_buf.len());
                                        m.record_frame_out("tcp", 1);
                                    }
                                    stream.write_all(&write_buf).await?;
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        "Handshake required",
                                    ));
                                }
                                Err(codec::DecodeError::UnsupportedVersion(v)) => {
                                    warn!(error = %v, "protocol decode error: unsupported version");
                                    if let Some(ref m) = metrics {
                                        m.record_protocol_error("unsupported_version");
                                    }
                                    write_buf.clear();
                                    codec::encode_error(
                                        0,
                                        ErrorCode::UnsupportedVersion,
                                        &format!("Unsupported version: {v}"),
                                        &mut write_buf,
                                    );
                                    if let Some(ref m) = metrics {
                                        m.record_bytes_out("tcp", write_buf.len());
                                        m.record_frame_out("tcp", 1);
                                    }
                                    stream.write_all(&write_buf).await?;
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!("Unsupported protocol version: {v}"),
                                    ));
                                }
                                Err(e) => {
                                    warn!(error = %e, "protocol decode error");
                                    if let Some(ref m) = metrics {
                                        m.record_protocol_error("decode_error");
                                    }
                                    write_buf.clear();
                                    codec::encode_error(
                                        0,
                                        ErrorCode::InvalidPayload,
                                        &format!("{e}"),
                                        &mut write_buf,
                                    );
                                    if let Some(ref m) = metrics {
                                        m.record_bytes_out("tcp", write_buf.len());
                                        m.record_frame_out("tcp", 1);
                                    }
                                    let _ = stream.write_all(&write_buf).await;
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!("{e}"),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
