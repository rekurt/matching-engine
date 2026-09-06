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
    ConnectionState, ErrorCode, CAP_BATCH_ORDERS, CAP_EXACTLY_ONCE, CAP_HEARTBEAT, CAP_ORDER_FLAGS,
    CAP_ORDER_QUERY, CAP_SESSION_RECOVERY, CAP_SNAPSHOT, MAX_WRITE_BUFFER_SIZE, PROTOCOL_VERSION,
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
    max_connections: usize,
    drain_timeout: Duration,
    readiness: Option<Arc<std::sync::atomic::AtomicBool>>,
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

    pub fn new_with_metrics(router: Arc<Router>, metrics: Option<Arc<EngineMetrics>>) -> Self {
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
            max_connections: crate::tcp_server::DEFAULT_MAX_CONNECTIONS,
            drain_timeout: Duration::from_secs(30),
            readiness: None,
        }
    }

    /// Override the maximum number of concurrent connections.
    pub fn with_readiness(mut self, readiness: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.readiness = Some(readiness);
        self
    }

    pub fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max.max(1);
        self
    }

    /// Start listening on the given Unix socket path.
    pub async fn listen(
        &self,
        path: &str,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "UDS path is not a socket",
                ));
            }
            match UnixStream::connect(path).await {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        "UDS listener already active",
                    ))
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path)?;
                }
                Err(e) => return Err(e),
            }
        }
        let listener = UnixListener::bind(path)?;
        let _readiness_guard = ReadinessGuard(self.readiness.clone());
        if let Some(flag) = &self.readiness {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        info!(path = path, "UDS server listening");

        // Advertise all v2 capabilities — mirrors tcp_server::build_capabilities.
        let caps = CAP_SESSION_RECOVERY
            | CAP_HEARTBEAT
            | CAP_BATCH_ORDERS
            | CAP_ORDER_FLAGS
            | CAP_ORDER_QUERY
            | CAP_EXACTLY_ONCE
            | CAP_SNAPSHOT;

        let conn_limiter = Arc::new(tokio::sync::Semaphore::new(self.max_connections));

        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;

                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("UDS server shutting down");
                        if let Err(e) = std::fs::remove_file(path) {
                            tracing::debug!(path, error = %e, "UDS shutdown remove_file failed");
                        }
                        break;
                    }
                }

                Some(_) = connections.join_next(), if !connections.is_empty() => {}

                result = listener.accept() => {
                    let (stream, _) = result?;
                    let permit = match Arc::clone(&conn_limiter).try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            warn!(max = self.max_connections, "UDS connection limit reached; rejecting");
                            drop(stream);
                            continue;
                        }
                    };
                    let router = Arc::clone(&self.router);
                    let metrics = self.metrics.clone();
                    let conn_shutdown = shutdown_rx.clone();
                    let session_store = Arc::clone(&self.session_store);
                    let hb_interval = self.heartbeat_interval_ms;
                    let hb_timeout = self.heartbeat_timeout_ms;
                    let clock = Arc::clone(&self.clock);

                    let span = info_span!("uds_conn");
                    connections.spawn(async move {
                        let _permit = permit;
                        if let Err(e) = handle_uds_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps, clock).await {
                            warn!(error = %e, "connection error");
                        }
                    }.instrument(span));
                }
            }
        }

        drop(_readiness_guard);
        if tokio::time::timeout(self.drain_timeout, async {
            while connections.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
        self.session_store.checkpoint()?;
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
    let mut _session_lease = None;
    let timeout = Duration::from_millis(heartbeat_timeout_ms);
    let mut read_deadline = tokio::time::Instant::now() + timeout;
    let mut outbound_tick = tokio::time::interval(Duration::from_millis(10));

    loop {
        tokio::select! {
            biased;

            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
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
                    if let Err(e) = write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await {
                        tracing::debug!(error = %e, "failed to send final error frame (client likely disconnected)");
                    }
                    return Ok(());
                }
            }

            _ = outbound_tick.tick(), if conn.state == ConnectionState::Active => {
                let next = session_store.pending(&conn.session_id, conn.server_frame_seq.peek(), &mut write_buf)?;
                write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                conn.server_frame_seq.reset_to(next);
            }

            read_result = tokio::time::timeout_at(read_deadline, stream.read_buf(&mut read_buf)) => {
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
                        if let Err(e) = write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await {
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
                        read_deadline = tokio::time::Instant::now() + timeout;
                        if let Some(ref m) = metrics {
                            m.record_bytes_in("uds", n);
                        }

                        loop {
                            match codec::decode_frame(&read_buf, &mut conn, clock) {
                                Ok((frame, consumed)) => {
                                    if conn.state == ConnectionState::Active && matches!(frame, DecodedFrame::Handshake { .. } | DecodedFrame::ResumeSession { .. }) {
                                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "connection already has an active session"));
                                    }
                                    let symbols = match &frame {
                                        DecodedFrame::Command(command, ..) => vec![command.symbol().0],
                                        DecodedFrame::Batch(commands, ..) => commands.iter().map(|command| command.symbol().0).collect(),
                                        DecodedFrame::GetOpenOrders { .. } => (0..64).collect(),
                                        _ => Vec::new(),
                                    };
                                    let execution_guard = session_store.execution_guards(symbols).await;
                                    session_store.ensure_healthy()?;
                                    let request = match &frame {
                                        DecodedFrame::Command(_, _, request) | DecodedFrame::Batch(_, _, request) => Some(*request),
                                        DecodedFrame::QueryOrder { request_id, .. } | DecodedFrame::GetOpenOrders { request_id, .. } => Some(*request_id),
                                        _ => None,
                                    };
                                    if let Some(request) = request {
                                        if session_store.dedup_lookup(&conn.session_id, request).is_none() {
                                            let commands: Vec<_> = match &frame {
                                                DecodedFrame::Command(command, ..) => vec![command],
                                                DecodedFrame::Batch(commands, ..) => commands.iter().collect(),
                                                _ => Vec::new(),
                                            };
                                            let mut cancel_ids = std::collections::HashSet::new();
                                            let valid = commands.iter().all(|command| match command {
                                                me_engine::command::Command::CancelOrder { order_id, symbol, .. } =>
                                                    cancel_ids.insert((*symbol, *order_id)) && router.query_order_for_session(*order_id, *symbol, conn.session_id).iter().any(|event| matches!(event, me_core::MatchEvent::OrderStatus { .. })),
                                                _ => true,
                                            });
                                            if !valid {
                                                read_buf.advance(consumed);
                                                write_buf.clear();
                                                codec::encode_error(0, ErrorCode::InvalidPayload, "cancel requires an active order owned by this session", &mut write_buf);
                                                drop(execution_guard);
                                                write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                                continue;
                                            }
                                        }
                                        let mut payload = vec![read_buf[5]];
                                        payload.extend_from_slice(&read_buf[crate::protocol::FRAME_HEADER_SIZE..consumed]);
                                        if let Err(error) = session_store.admit_request(conn.session_id, request, payload) {
                                            session_store.ensure_healthy()?;
                                            read_buf.advance(consumed);
                                            write_buf.clear();
                                            codec::encode_error(0, ErrorCode::InvalidPayload, &error.to_string(), &mut write_buf);
                                            drop(execution_guard);
                                            write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                            continue;
                                        }
                                    }
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
                                                    0,
                                                    ErrorCode::UnsupportedVersion,
                                                    &format!(
                                                        "Server requires protocol v{PROTOCOL_VERSION}, client sent v{client_version}"
                                                    ),
                                                    &mut write_buf,
                                                );
                                                write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                                return Ok(());
                                            }
                                            let session_id = session_store.try_create()?;
                                            _session_lease = Some(session_store.claim(session_id)?);
                                            session_store.checkpoint()?;
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
                                            _session_lease = Some(session_store.claim(session_id)?);
                                            let (replay, replay_next) = session_store.delivery_snapshot(&session_id, last_recv_seq)?;
                                            match replay {
                                                ReplayResult::Ok(replay_frames) => {
                                                    let count = replay_frames.len() as u32;
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = session_id;
                                                    // CRITICAL: re-sync outbound seq counter with the
                                                    // session's replay buffer state.
                                                    conn.server_frame_seq
                                                        .reset_to(replay_next);
                                                    codec::encode_session_resumed(count, 0, &mut write_buf);
                                                    session_store.checkpoint()?;
                                                    drop(execution_guard);
                                                    tokio::time::timeout(timeout, async {
                                                        write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                                        for replay_frame in replay_frames {
                                                            write_chunks(stream, &replay_frame, timeout, metrics.as_deref()).await?;
                                                        }
                                                        Ok::<_, std::io::Error>(())
                                                    }).await.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "replay timeout"))??;
                                                    debug!(replayed = count, next_seq = conn.server_frame_seq.peek(), "session resumed");
                                                    continue;
                                                }
                                                ReplayResult::Gap { missing_from, missing_to } => {
                                                    conn.state = ConnectionState::Active;
                                                    conn.session_id = session_id;
                                                    conn.server_frame_seq
                                                        .reset_to(replay_next);
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
                                                    codec::encode_error(0, ErrorCode::InvalidPayload,
                                                        "Unknown session; explicitly open a new session", &mut write_buf);
                                                    write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                                    return Ok(());
                                                }
                                            }
                                        }
                                        DecodedFrame::Ping { seq, ack_seq } => {
                                            if ack_seq > 0 && conn.state == ConnectionState::Active {
                                                session_store.apply_ack(&conn.session_id, ack_seq);
                                            }
                                            codec::encode_pong(seq, conn.last_client_seq, &mut write_buf);
                                        }
                                        DecodedFrame::Command(mut cmd, _seq, request_id) => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                if let me_engine::command::Command::PlaceOrder(ref mut ord) = cmd {
                                                    ord.client_session_id = conn.session_id;
                                                }
                                                // Enforce order ownership on cancel/amend.
                                                let _events = router.process_with_request(cmd, conn.session_id, request_id);
                                                let record = router.request_record(conn.session_id, request_id)
                                                    .map_err(std::io::Error::other)?
                                                    .ok_or_else(|| std::io::Error::other("durable request outcome missing"))?;
                                                session_store.commit_request(record)?;
                                                let next = session_store.pending(&conn.session_id, conn.server_frame_seq.peek(), &mut write_buf)?;
                                                conn.server_frame_seq.reset_to(next);
                                                dedup_cache_hit = true;
                                            }
                                        }
                                        DecodedFrame::Batch(cmds, _seq, request_id) => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                session_store.process_batch(&router, conn.session_id, request_id, cmds)?;
                                                let next = session_store.pending(&conn.session_id, conn.server_frame_seq.peek(), &mut write_buf)?;
                                                conn.server_frame_seq.reset_to(next);
                                                dedup_cache_hit = true;
                                            }
                                        }
                                        DecodedFrame::QueryOrder { order_id, symbol_id, seq, request_id } => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                let symbol = me_core::types::SymbolId(symbol_id);
                                                let events = router.query_order_for_session(order_id, symbol, conn.session_id);
                                                codec::encode_events(&events, seq, request_id, &mut write_buf);
                                                new_dedup_request_id = Some(request_id);
                                            }
                                        }
                                        DecodedFrame::Acknowledge { last_processed_server_seq, seq: _ } => {
                                            session_store.apply_ack(&conn.session_id, last_processed_server_seq);
                                        }
                                        DecodedFrame::GetOpenOrders { seq: _, request_id } => {
                                            // Dedup: retry MUST return cached snapshot, not re-fan-out.
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                drop(execution_guard);
                                                write_chunks(stream, &cached, timeout, metrics.as_deref()).await?;
                                                continue;
                                            } else {
                                                // `get_open_orders_by_session` blocks on sync recv
                                                // from each shard — wrap in spawn_blocking so the
                                                // tokio executor thread is not stalled.
                                                let router_for_snapshot = Arc::clone(&router);
                                                let session_id_copy = conn.session_id;
                                                let snapshot_result = tokio::task::spawn_blocking(move || {
                                                    router_for_snapshot.get_open_orders_by_session(session_id_copy)
                                                })
                                                .await;

                                                let orders = match snapshot_result {
                                                    Ok(Ok(orders)) => Some(orders),
                                                    Ok(Err(shard_err)) => {
                                                        tracing::error!(
                                                            error = %shard_err,
                                                            "GetOpenOrders: shard failure — returning InternalError"
                                                        );
                                                        None
                                                    }
                                                    Err(join_err) => {
                                                        tracing::error!(
                                                            error = ?join_err,
                                                            "GetOpenOrders: spawn_blocking panicked/cancelled — returning InternalError"
                                                        );
                                                        None
                                                    }
                                                };

                                                if let Some(orders) = orders {
                                                    let next = session_store.pending(&conn.session_id, conn.server_frame_seq.peek(), &mut write_buf)?;
                                                    conn.server_frame_seq.reset_to(next);
                                                    let cached = session_store.cache_open_orders(&conn.session_id, request_id, &orders)?;
                                                    if let Some(next) = crate::session_store::next_sequence_in_block(&cached) {
                                                        conn.server_frame_seq.reset_to(next);
                                                    }
                                                    drop(execution_guard);
                                                    tokio::time::timeout(timeout, async {
                                                        write_chunks(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                                        write_chunks(stream, &cached, timeout, metrics.as_deref()).await?;
                                                        Ok::<_, std::io::Error>(())
                                                    }).await.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "snapshot timeout"))??;
                                                    continue;
                                                } else {
                                                    codec::encode_error(
                                                        0,
                                                        ErrorCode::InternalError,
                                                        "GetOpenOrders failed: incomplete snapshot",
                                                        &mut write_buf,
                                                    );
                                                }
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
                                        let _ = frames_pushed;

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
                                            let _ = warn_pushed;
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
                                        if let Err(e) = write_bounded(stream, &err_buf, timeout, metrics.as_deref()).await {
                                            tracing::debug!(error = %e, "failed to send Backpressure frame");
                                        }
                                        return Err(std::io::Error::new(
                                            std::io::ErrorKind::WriteZero,
                                            "Write buffer backpressure limit exceeded",
                                        ));
                                    }
                                    session_store.checkpoint()?;
                                    if !dedup_cache_hit && conn.state == ConnectionState::Active {
                                        let next = session_store.pending(&conn.session_id, conn.server_frame_seq.peek(), &mut write_buf)?;
                                        conn.server_frame_seq.reset_to(next);
                                    }
                                    drop(execution_guard);
                                    write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
                                }
                                Err(codec::DecodeError::Incomplete) => break,
                                Err(codec::DecodeError::SequenceOutOfOrder { expected_min, got }) => {
                                    warn!(error = "sequence out of order", "protocol decode error");
                                    if let Some(ref m) = metrics {
                                        m.record_protocol_error("sequence_out_of_order");
                                    }
                                    write_buf.clear();
                                    codec::encode_error(
                                        0,
                                        ErrorCode::SequenceOutOfOrder,
                                        &format!("Expected seq >= {expected_min}, got {got}"),
                                        &mut write_buf,
                                    );
                                    write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
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
                                    write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
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
                                    write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
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
                                    write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await?;
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
                                    if let Err(we) = write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await {
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

async fn write_chunks<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
    timeout: Duration,
    metrics: Option<&EngineMetrics>,
) -> std::io::Result<()> {
    tokio::time::timeout(timeout, async {
        for chunk in bytes.chunks(MAX_WRITE_BUFFER_SIZE) {
            stream.write_all(chunk).await?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout"))??;
    record_sent(metrics, bytes);
    Ok(())
}

async fn write_bounded<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
    timeout: Duration,
    metrics: Option<&EngineMetrics>,
) -> std::io::Result<()> {
    tokio::time::timeout(timeout, stream.write_all(bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout"))??;
    record_sent(metrics, bytes);
    Ok(())
}

fn record_sent(metrics: Option<&EngineMetrics>, bytes: &[u8]) {
    if let Some(metrics) = metrics {
        let mut offset = 0;
        let mut frames = 0;
        while offset + crate::protocol::FRAME_HEADER_SIZE <= bytes.len() {
            let size = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += crate::protocol::FRAME_HEADER_SIZE + size;
            if offset > bytes.len() {
                break;
            }
            frames += 1;
        }
        metrics.record_bytes_out("uds", bytes.len());
        metrics.record_frame_out("uds", frames);
    }
}

struct ReadinessGuard(Option<Arc<std::sync::atomic::AtomicBool>>);
impl Drop for ReadinessGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}
