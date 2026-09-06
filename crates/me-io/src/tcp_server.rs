use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
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

#[cfg(feature = "tls")]
use tokio_rustls::TlsAcceptor;

/// Default cap on concurrent client connections. Bounds memory/session
/// amplification from a connection-flood (each connection holds a read buffer
/// and a session entry). Override with [`TcpServer::with_max_connections`].
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

/// TCP server for the binary protocol.
///
/// Architecture:
/// - tokio for async accept + read/write (I/O multiplexing only)
/// - Matching logic is synchronous via Router (crossbeam channels)
/// - Each TCP connection is handled in its own tokio task
/// - Graceful shutdown via a `watch` channel (SIGTERM/SIGINT)
pub struct TcpServer {
    router: Arc<Router>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    clock: Arc<dyn Clock>,
    max_connections: usize,
    drain_timeout: Duration,
    readiness: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(feature = "tls")]
    tls_acceptor: Option<TlsAcceptor>,
}

impl TcpServer {
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
            max_connections: DEFAULT_MAX_CONNECTIONS,
            drain_timeout: Duration::from_secs(30),
            readiness: None,
            #[cfg(feature = "tls")]
            tls_acceptor: None,
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
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        let listener = TcpListener::bind(addr).await?;
        let _readiness_guard = ReadinessGuard(self.readiness.clone());
        if let Some(flag) = &self.readiness {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        info!(addr = addr, "TCP server listening");

        #[cfg(feature = "tls")]
        let tls_active = self.tls_acceptor.is_some();
        #[cfg(not(feature = "tls"))]
        let tls_active = false;

        let caps = build_capabilities(tls_active);

        // Bound concurrent connections. A permit is acquired per accepted
        // connection and released when its task ends.
        let conn_limiter = Arc::new(tokio::sync::Semaphore::new(self.max_connections));

        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;

                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("TCP server shutting down");
                        break;
                    }
                }

                Some(_) = connections.join_next(), if !connections.is_empty() => {}

                result = listener.accept() => {
                    let (stream, peer) = result?;

                    // Reject (close) the connection if we are at capacity,
                    // rather than unboundedly spawning tasks/sessions.
                    let permit = match Arc::clone(&conn_limiter).try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            warn!(peer = %peer, max = self.max_connections, "connection limit reached; rejecting");
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

                    #[cfg(feature = "tls")]
                    let tls_acceptor = self.tls_acceptor.clone();

                    let span = info_span!("tcp_conn", peer = %peer);
                    connections.spawn(async move {
                        // Hold the permit for the lifetime of the connection.
                        let _permit = permit;
                        #[cfg(feature = "tls")]
                        let result = if let Some(acceptor) = tls_acceptor {
                            match tokio::time::timeout(Duration::from_millis(hb_timeout), acceptor.accept(stream)).await {
                                Ok(Ok(tls_stream)) => {
                                    handle_connection(tls_stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps, clock).await
                                }
                                Ok(Err(e)) => {
                                    warn!(error = %e, "TLS handshake failed");
                                    Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e.to_string()))
                                }
                                Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "TLS handshake timeout")),
                            }
                        } else {
                            handle_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps, clock).await
                        };

                        #[cfg(not(feature = "tls"))]
                        let result = handle_connection(stream, router, conn_shutdown, metrics, session_store, hb_interval, hb_timeout, caps, clock).await;

                        if let Err(e) = result {
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

fn build_capabilities(tls_active: bool) -> u64 {
    // Advertise all v2 capabilities the server implements. CAP_EXACTLY_ONCE
    // and CAP_SNAPSHOT became load-bearing in protocol v2 (request_id dedup
    // + GetOpenOrders resync) — clients inspect these bits to know whether
    // retries and snapshots are safe on this server.
    let mut caps = CAP_SESSION_RECOVERY
        | CAP_HEARTBEAT
        | CAP_BATCH_ORDERS
        | CAP_ORDER_FLAGS
        | CAP_ORDER_QUERY
        | CAP_EXACTLY_ONCE
        | CAP_SNAPSHOT;
    if tls_active {
        caps |= crate::protocol::CAP_TLS;
    }
    caps
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection<S>(
    mut stream: S,
    router: Arc<Router>,
    mut shutdown_rx: watch::Receiver<bool>,
    metrics: Option<Arc<EngineMetrics>>,
    session_store: Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
    clock: Arc<dyn Clock>,
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
        &clock,
    )
    .await;

    if let Some(ref m) = metrics {
        m.record_connection_close("tcp");
    }
    info!("client disconnected");

    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection_inner<S>(
    stream: &mut S,
    router: Arc<Router>,
    shutdown_rx: &mut watch::Receiver<bool>,
    metrics: &Option<Arc<EngineMetrics>>,
    session_store: &Arc<SessionStore>,
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u64,
    capabilities: u64,
    clock: &dyn Clock,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
                        if let Err(e) = write_bounded(stream, &write_buf, timeout, metrics.as_deref()).await {
                            tracing::debug!(error = %e, "failed to send HeartbeatTimeout frame");
                        }
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Heartbeat timeout",
                        ));
                    }
                    Ok(Err(e)) => return Err(e),
                    Ok(Ok(0)) => return Ok(()), // Connection closed by client
                    Ok(Ok(n)) => {
                        read_deadline = tokio::time::Instant::now() + timeout;
                        if let Some(ref m) = metrics {
                            m.record_bytes_in("tcp", n);
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
                                        m.record_frame_in("tcp");
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
                                                    // session's replay buffer state. Without this,
                                                    // the fresh ConnectionCodecState's server_frame_seq=1
                                                    // would collide with seqs already in the buffer.
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
                                                    // Frames client needs were evicted — client must resync.
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
                                            // Piggyback ACK from client: purge replay buffer.
                                            // Guard against pre-handshake Pings applying ACKs to
                                            // the zero-sentinel session_id.
                                            if ack_seq > 0 && conn.state == ConnectionState::Active {
                                                session_store.apply_ack(&conn.session_id, ack_seq);
                                            }
                                            codec::encode_pong(seq, conn.last_client_seq, &mut write_buf);
                                        }
                                        DecodedFrame::Command(mut cmd, _seq, request_id) => {
                                            if let Some(cached) = session_store.dedup_lookup(&conn.session_id, request_id) {
                                                // Retry: return cached response, skip re-execution.
                                                write_buf.extend_from_slice(&cached);
                                                dedup_cache_hit = true;
                                            } else {
                                                // v2: inject session_id into PlaceOrder so OpenOrders
                                                // snapshot can later find this client's resting orders.
                                                if let me_engine::command::Command::PlaceOrder(ref mut ord) = cmd {
                                                    ord.client_session_id = conn.session_id;
                                                }
                                                // Enforce order ownership on cancel/amend: a client
                                                // may only mutate orders its own session placed.
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
                                            // No response; write_buf stays empty.
                                        }
                                        DecodedFrame::GetOpenOrders { seq: _, request_id } => {
                                            // Phase 4: return snapshot of session's open orders.
                                            //
                                            // Dedup applies to GetOpenOrders for exactly-once
                                            // semantics: a client retry after a network blip
                                            // MUST return the cached snapshot, NOT re-fan-out
                                            // to every shard (expensive) AND MUST NOT emit a
                                            // second OpenOrdersBegin/End sequence mid-stream.
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

                                                // Flatten Result<Result<Vec, String>, JoinError> into
                                                // a single error path for both spawn panics and shard
                                                // failures. Neither should produce a snapshot.
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
                                                    // Return explicit error. NOT cached in dedup
                                                    // (new_dedup_request_id stays None) so retry
                                                    // executes fresh.
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

                                    // ── v2 outbound seq assignment & replay push ────────────
                                    //
                                    // CASE 1 — dedup cache hit: the cached bytes already carry
                                    // their original server_seq values from the first execution,
                                    // AND those frames are still present in the replay buffer.
                                    // Just send them over the wire; do NOT push duplicates.
                                    //
                                    // CASE 2 — freshly-encoded response: rewrite each frame's
                                    // header.seq with a monotonic server_frame_seq, push each
                                    // individual frame into the replay buffer, advance the
                                    // connection counter by the frame count, then cache the
                                    // (now-finalized) bytes in the dedup table for retries.
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

                                        // Cache finalized bytes (with assigned server_seqs) for
                                        // dedup — retries will re-send these exact bytes so the
                                        // client sees the ORIGINAL server_seqs again, matching
                                        // the replay-buffer entries.
                                        if let Some(req_id) = new_dedup_request_id {
                                            let frame_bytes = Bytes::copy_from_slice(&write_buf);
                                            session_store.dedup_insert(&conn.session_id, req_id, frame_bytes);
                                        }

                                        // Emit FlowControlWarning if buffer approaches limits.
                                        if let Some((pct, is_hard)) =
                                            session_store.check_flow_control_warning(&conn.session_id)
                                        {
                                            let mut warn_buf = BytesMut::with_capacity(32);
                                            codec::encode_flow_control_warning(
                                                0, // placeholder, rewritten by push_frames
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
                                                warn!(
                                                    buffer_pct = pct,
                                                    "slow consumer: hard limit reached"
                                                );
                                            } else {
                                                debug!(
                                                    buffer_pct = pct,
                                                    "slow consumer: soft limit reached"
                                                );
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
        metrics.record_bytes_out("tcp", bytes.len());
        metrics.record_frame_out("tcp", frames);
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
