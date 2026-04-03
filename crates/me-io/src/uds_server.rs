use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, info, info_span, warn, Instrument};

use me_engine::router::ShardedRouterV2;
use me_metrics::EngineMetrics;

use crate::codec::{self, ConnectionCodecState, DecodedFrame};
use crate::protocol::{
    ConnectionState, ErrorCode, MAX_WRITE_BUFFER_SIZE, PROTOCOL_VERSION,
    CAP_BATCH_ORDERS, CAP_HEARTBEAT, CAP_ORDER_FLAGS, CAP_ORDER_QUERY, CAP_SESSION_RECOVERY,
};
use crate::session_store::SessionStore;

/// Unix Domain Socket server.
///
/// Identical protocol to TCP but ~30% lower latency for co-located processes
/// since it bypasses the TCP/IP stack.
pub struct UdsServer {
    router: Arc<ShardedRouterV2>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
}

impl UdsServer {
    pub fn new(router: Arc<ShardedRouterV2>) -> Self {
        Self::new_with_config(
            router,
            None,
            Arc::new(SessionStore::new(1000, Duration::from_secs(60))),
            5_000,
            15_000,
        )
    }

    pub fn new_with_metrics(
        router: Arc<ShardedRouterV2>,
        metrics: Option<Arc<EngineMetrics>>,
    ) -> Self {
        Self::new_with_config(
            router,
            metrics,
            Arc::new(SessionStore::new(1000, Duration::from_secs(60))),
            5_000,
            15_000,
        )
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
        }
    }

    /// Start listening on the given Unix socket path.
    pub async fn listen(
        &self,
        path: &str,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        info!(path = path, "UDS server listening");

        let caps = CAP_SESSION_RECOVERY | CAP_HEARTBEAT | CAP_BATCH_ORDERS | CAP_ORDER_FLAGS | CAP_ORDER_QUERY;

        loop {
            tokio::select! {
                biased;

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("UDS server shutting down");
                        let _ = std::fs::remove_file(path);
                        break;
                    }
                }

                result = listener.accept() => {
                    let (stream, _) = result?;
                    let router = Arc::clone(&self.router);
                    let metrics = self.metrics.clone();
                    let conn_shutdown = shutdown_rx.clone();
                    let session_store = Arc::clone(&self.session_store);
                    let hb_interval = self.heartbeat_interval_ms;
                    let hb_timeout = self.heartbeat_timeout_ms;

                    let span = info_span!("uds_conn");
                    tokio::spawn(async move {
                        if let Err(e) = handle_uds_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps).await {
                            warn!(error = %e, "connection error");
                        }
                    }.instrument(span));
                }
            }
        }

        Ok(())
    }
}

async fn handle_uds_connection(
    mut stream: UnixStream,
    router: Arc<ShardedRouterV2>,
    mut shutdown_rx: watch::Receiver<bool>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
) -> std::io::Result<()> {
    if let Some(ref m) = metrics {
        m.record_connection_open("uds");
    }
    info!("client connected");

    let result = handle_uds_connection_inner(
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
        m.record_connection_close("uds");
    }
    info!("client disconnected");

    result
}

async fn handle_uds_connection_inner(
    stream: &mut UnixStream,
    router: Arc<ShardedRouterV2>,
    shutdown_rx: &mut watch::Receiver<bool>,
    metrics: &Option<Arc<EngineMetrics>>,
    session_store: &Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
) -> std::io::Result<()> {
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
                    Ok(Ok(0)) => return Ok(()),
                    Ok(Ok(n)) => {
                        if let Some(ref m) = metrics {
                            m.record_bytes_in("uds", n);
                        }

                        loop {
                            match codec::decode_frame(&read_buf, &mut conn) {
                                Ok((frame, consumed)) => {
                                    read_buf.advance(consumed);

                                    if let Some(ref m) = metrics {
                                        m.record_frame_in("uds");
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
                                                    m.record_bytes_out("uds", write_buf.len());
                                                    m.record_frame_out("uds", 1);
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
                                        m.record_bytes_out("uds", write_buf.len());
                                        m.record_frame_out("uds", 1);
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
                                        m.record_bytes_out("uds", write_buf.len());
                                        m.record_frame_out("uds", 1);
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
                                        m.record_bytes_out("uds", write_buf.len());
                                        m.record_frame_out("uds", 1);
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
                                        m.record_bytes_out("uds", write_buf.len());
                                        m.record_frame_out("uds", 1);
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
                                        m.record_bytes_out("uds", write_buf.len());
                                        m.record_frame_out("uds", 1);
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
