use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, info, info_span, warn, Instrument};

use me_clock::Clock;
use me_engine::router::Router;
use me_metrics::EngineMetrics;

use crate::codec::{self, ConnectionCodecState, DecodedFrame};
use crate::protocol::{
    ConnectionState, ErrorCode, CAP_BATCH_ORDERS, CAP_EXACTLY_ONCE, CAP_HEARTBEAT,
    CAP_ORDER_FLAGS, CAP_ORDER_QUERY, CAP_SESSION_RECOVERY, CAP_SNAPSHOT, MAX_WRITE_BUFFER_SIZE,
    PROTOCOL_VERSION,
};
use crate::session_store::SessionStore;

/// Unix Domain Socket server.
///
/// Identical protocol to TCP but ~30% lower latency for co-located processes
/// since it bypasses the TCP/IP stack.
pub struct UdsServer {
    router: Arc<Router>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    clock: Arc<dyn Clock>,
}

impl UdsServer {
    pub fn new(router: Arc<Router>) -> Self {
        Self::new_with_config(
            router,
            None,
            Arc::new(SessionStore::new(1000, Duration::from_secs(60))),
            5_000,
            15_000,
            Arc::new(me_clock::SystemClock),
        )
    }

    pub fn new_with_metrics(
        router: Arc<Router>,
        metrics: Option<Arc<EngineMetrics>>,
    ) -> Self {
        Self::new_with_config(
            router,
            metrics,
            Arc::new(SessionStore::new(1000, Duration::from_secs(60))),
            5_000,
            15_000,
            Arc::new(me_clock::SystemClock),
        )
    }

    pub fn new_with_config(
        router: Arc<Router>,
        metrics: Option<Arc<EngineMetrics>>,
        session_store: Arc<SessionStore>,
        heartbeat_interval_ms: u32,
        heartbeat_timeout_ms: u64,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            router,
            metrics,
            session_store,
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
            clock,
        }
    }

    /// Start listening on the given Unix socket path.
    pub async fn listen(
        &self,
        path: &str,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        // Pre-bind cleanup: remove any stale socket. NotFound is normal
        // (first start); other errors are logged at debug for traceability
        // — if bind() then fails, the root cause is visible.
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path, error = %e, "UDS pre-bind remove_file failed");
            }
        }
        let listener = UnixListener::bind(path)?;
        info!(path = path, "UDS server listening");

        // Advertise all v2 capabilities — mirrors tcp_server::build_capabilities.
        let caps = CAP_SESSION_RECOVERY
            | CAP_HEARTBEAT
            | CAP_BATCH_ORDERS
            | CAP_ORDER_FLAGS
            | CAP_ORDER_QUERY
            | CAP_EXACTLY_ONCE
            | CAP_SNAPSHOT;

        loop {
            tokio::select! {
                biased;

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("UDS server shutting down");
                        if let Err(e) = std::fs::remove_file(path) {
                            tracing::debug!(path, error = %e, "UDS shutdown remove_file failed");
                        }
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
                    let clock = Arc::clone(&self.clock);

                    let span = info_span!("uds_conn");
                    tokio::spawn(async move {
                        if let Err(e) = handle_uds_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps, clock).await {
                            warn!(error = %e, "connection error");
                        }
                    }.instrument(span));
                }
            }
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_uds_connection(
    mut stream: UnixStream,
    router: Arc<Router>,
    mut shutdown_rx: watch::Receiver<bool>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
    clock: Arc<dyn Clock>,
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
        &clock,
    )
    .await;

    if let Some(ref m) = metrics {
        m.record_connection_close("uds");
    }
    info!("client disconnected");

    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_uds_connection_inner(
    stream: &mut UnixStream,
    router: Arc<Router>,
    shutdown_rx: &mut watch::Receiver<bool>,
    metrics: &Option<Arc<EngineMetrics>>,
    session_store: &Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
    clock: &dyn Clock,
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
                    // Emit a final ServerShuttingDown error frame and exit.
                    // No need to transition conn.state — the connection is
                    // terminating, state is never read again.
                    write_buf.clear();
                    codec::encode_error(
                        0,
                        ErrorCode::ServerShuttingDown,
                        "Server shutting down",
                        &mut write_buf,
                    );
                    if let Err(e) = stream.write_all(&write_buf).await {
                        tracing::debug!(error = %e, "failed to send final error frame (client likely disconnected)");
                    }
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
                        if let Err(e) = stream.write_all(&write_buf).await {
                            tracing::debug!(error = %e, "failed to send HeartbeatTimeout frame");
                        }
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
                            match codec::decode_frame(&read_buf, &mut conn, clock) {
                                Ok((frame, consumed)) => {
                                    read_buf.advance(consumed);

                                    if let Some(ref m) = metrics {
                                        m.record_frame_in("uds");
                                    }

                                    write_buf.clear();
                                    // v2 exactly-once state for this frame:
                                    let mut dedup_cache_hit = false;
                                    let mut new_dedup_request_id: Option<u64> = None;
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
                                            use crate::session_store::ReplayResult;
                                            match session_store.resume(&session_id, last_recv_seq) {
                                                ReplayResult::Ok(replay_frames) => {
                                                    let count = replay_frames.len() as u32;
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = session_id;
                                                    // CRITICAL: re-sync outbound seq counter with the
                                                    // session's replay buffer state.
                                                    conn.server_frame_seq
                                                        .reset_to(session_store.next_outbound_seq(&session_id));
                                                    codec::encode_session_resumed(count, seq, &mut write_buf);
                                                    codec::encode_replay_frames(&replay_frames, &mut write_buf);
                                                    debug!(replayed = count, next_seq = conn.server_frame_seq.peek(), "session resumed");
                                                }
                                                ReplayResult::Gap { missing_from, missing_to } => {
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = session_id;
                                                    conn.server_frame_seq
                                                        .reset_to(session_store.next_outbound_seq(&session_id));
                                                    codec::encode_replay_gap(
                                                        seq,
                                                        missing_from,
                                                        missing_to,
                                                        &mut write_buf,
                                                    );
                                                    warn!(
                                                        missing_from,
                                                        missing_to,
                                                        next_seq = conn.server_frame_seq.peek(),
                                                        "replay gap: client must resync via GetOpenOrders"
                                                    );
                                                }
                                                ReplayResult::SessionNotFound => {
                                                    let new_id = session_store.create();
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = new_id;
                                                    conn.server_frame_seq
                                                        .reset_to(session_store.next_outbound_seq(&new_id));
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
                                        DecodedFrame::Ping { seq, ack_seq } => {
                                            if ack_seq > 0 && conn.state == ConnectionState::Active {
                                                session_store.apply_ack(&conn.session_id, ack_seq);
                                            }
                                            codec::encode_pong(seq, conn.last_client_seq, &mut write_buf);
                                        }
                                        DecodedFrame::Command(mut cmd, seq, request_id) => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                if let me_engine::command::Command::PlaceOrder(ref mut ord) = cmd {
                                                    ord.client_session_id = conn.session_id;
                                                }
                                                let events = router.process(cmd);
                                                codec::encode_events(&events, seq, request_id, &mut write_buf);
                                                new_dedup_request_id = Some(request_id);
                                            }
                                        }
                                        DecodedFrame::Batch(cmds, seq, request_id) => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                for mut cmd in cmds {
                                                    if let me_engine::command::Command::PlaceOrder(ref mut ord) = cmd {
                                                        ord.client_session_id = conn.session_id;
                                                    }
                                                    let events = router.process(cmd);
                                                    codec::encode_events(&events, seq, request_id, &mut write_buf);
                                                }
                                                new_dedup_request_id = Some(request_id);
                                            }
                                        }
                                        DecodedFrame::QueryOrder { order_id, symbol_id, seq, request_id } => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                let symbol = me_core::types::SymbolId(symbol_id);
                                                let events = router.query_order(order_id, symbol);
                                                codec::encode_events(&events, seq, request_id, &mut write_buf);
                                                new_dedup_request_id = Some(request_id);
                                            }
                                        }
                                        DecodedFrame::Acknowledge { last_processed_server_seq, seq: _ } => {
                                            session_store.apply_ack(&conn.session_id, last_processed_server_seq);
                                        }
                                        DecodedFrame::GetOpenOrders { seq, request_id } => {
                                            // Dedup: retry MUST return cached snapshot, not re-fan-out.
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                // `get_open_orders_by_session` blocks on sync recv
                                                // from each shard — wrap in spawn_blocking so the
                                                // tokio executor thread is not stalled.
                                                let router_for_snapshot = Arc::clone(&router);
                                                let session_id_copy = conn.session_id;
                                                let orders = tokio::task::spawn_blocking(move || {
                                                    router_for_snapshot.get_open_orders_by_session(session_id_copy)
                                                })
                                                .await
                                                .unwrap_or_else(|e| {
                                                    tracing::error!(
                                                        error = ?e,
                                                        "GetOpenOrders: spawn_blocking task panicked/cancelled — returning empty snapshot"
                                                    );
                                                    Vec::new()
                                                });
                                                let snapshot_seq = conn.server_frame_seq.peek();
                                                codec::encode_open_orders_begin(
                                                    seq,
                                                    request_id,
                                                    orders.len() as u32,
                                                    snapshot_seq,
                                                    &mut write_buf,
                                                );
                                                for ord in &orders {
                                                    codec::encode_open_order(
                                                        seq,
                                                        request_id,
                                                        ord,
                                                        &mut write_buf,
                                                    );
                                                }
                                                codec::encode_open_orders_end(
                                                    seq,
                                                    request_id,
                                                    snapshot_seq,
                                                    &mut write_buf,
                                                );
                                                new_dedup_request_id = Some(request_id);
                                            }
                                        }
                                    }

                                    // v2 outbound seq assignment & replay push (see tcp_server.rs
                                    // for detailed semantics). Cache hits skip — original frames
                                    // are already in the replay buffer with their original seqs.
                                    if !write_buf.is_empty()
                                        && !dedup_cache_hit
                                        && conn.state == ConnectionState::Active
                                    {
                                        let frames_pushed = session_store.push_frames_with_seq_rewrite(
                                            &conn.session_id,
                                            conn.server_frame_seq.peek(),
                                            &mut write_buf,
                                        );
                                        conn.server_frame_seq.advance_by(frames_pushed);

                                        if let Some(req_id) = new_dedup_request_id {
                                            let frame_bytes = Bytes::copy_from_slice(&write_buf);
                                            session_store.dedup_insert(&conn.session_id, req_id, frame_bytes);
                                        }

                                        if let Some((pct, is_hard)) =
                                            session_store.check_flow_control_warning(&conn.session_id)
                                        {
                                            let mut warn_buf = BytesMut::with_capacity(32);
                                            codec::encode_flow_control_warning(
                                                0,
                                                pct,
                                                is_hard,
                                                &mut warn_buf,
                                            );
                                            let warn_pushed = session_store
                                                .push_frames_with_seq_rewrite(
                                                    &conn.session_id,
                                                    conn.server_frame_seq.peek(),
                                                    &mut warn_buf,
                                                );
                                            conn.server_frame_seq.advance_by(warn_pushed);
                                            write_buf.extend_from_slice(&warn_buf);
                                            if is_hard {
                                                warn!(buffer_pct = pct, "slow consumer: hard limit");
                                            }
                                        }
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
                                        if let Err(e) = stream.write_all(&err_buf).await {
                                            tracing::debug!(error = %e, "failed to send Backpressure frame");
                                        }
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
                                Err(codec::DecodeError::InvalidRequestId) => {
                                    warn!(error = "invalid request_id (zero)", "protocol decode error");
                                    if let Some(ref m) = metrics {
                                        m.record_protocol_error("invalid_request_id");
                                    }
                                    write_buf.clear();
                                    codec::encode_error(
                                        0,
                                        ErrorCode::InvalidRequestId,
                                        "Command messages require non-zero request_id (v2)",
                                        &mut write_buf,
                                    );
                                    if let Some(ref m) = metrics {
                                        m.record_bytes_out("uds", write_buf.len());
                                        m.record_frame_out("uds", 1);
                                    }
                                    stream.write_all(&write_buf).await?;
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        "Invalid request_id",
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
                                    if let Err(we) = stream.write_all(&write_buf).await {
                                        tracing::debug!(write_error = %we, decode_error = %e, "failed to send InvalidPayload frame");
                                    }
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
