use std::sync::Arc;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, info, info_span, warn, Instrument};

use me_engine::router::ShardedRouterV2;
use me_metrics::EngineMetrics;

use crate::codec::{self, ConnectionCodecState, DecodedFrame};
use crate::protocol::{ConnectionState, ErrorCode, MAX_WRITE_BUFFER_SIZE, PROTOCOL_VERSION};

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
}

impl TcpServer {
    pub fn new(router: Arc<ShardedRouterV2>) -> Self {
        Self {
            router,
            metrics: None,
        }
    }

    pub fn new_with_metrics(
        router: Arc<ShardedRouterV2>,
        metrics: Option<Arc<EngineMetrics>>,
    ) -> Self {
        Self { router, metrics }
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

                    let span = info_span!("tcp_conn", peer = %peer);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, router, conn_shutdown, metrics).await {
                            warn!(error = %e, "connection error");
                        }
                    }.instrument(span));
                }
            }
        }

        Ok(())
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    router: Arc<ShardedRouterV2>,
    mut shutdown_rx: watch::Receiver<bool>,
    metrics: Option<Arc<EngineMetrics>>,
) -> std::io::Result<()> {
    if let Some(ref m) = metrics {
        m.record_connection_open("tcp");
    }
    info!("client connected");

    let result = handle_connection_inner(&mut stream, router, &mut shutdown_rx, &metrics).await;

    if let Some(ref m) = metrics {
        m.record_connection_close("tcp");
    }
    info!("client disconnected");

    result
}

async fn handle_connection_inner(
    stream: &mut TcpStream,
    router: Arc<ShardedRouterV2>,
    shutdown_rx: &mut watch::Receiver<bool>,
    metrics: &Option<Arc<EngineMetrics>>,
) -> std::io::Result<()> {
    let mut read_buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);
    let mut conn = ConnectionCodecState::new();

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    // Send a graceful shutdown error frame.
                    let _ = ConnectionState::Closing; // state tracked for future metrics
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

            result = stream.read_buf(&mut read_buf) => {
                let n = result?;
                if n == 0 {
                    return Ok(()); // Connection closed by client
                }

                if let Some(ref m) = metrics {
                    m.record_bytes_in("tcp", n);
                }

                // Try to decode and process frames from the buffer.
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
                                    conn.state = ConnectionState::Active;
                                    codec::encode_handshake_ack(seq, &mut write_buf);
                                }
                                DecodedFrame::Ping(seq) => {
                                    codec::encode_pong(seq, &mut write_buf);
                                }
                                DecodedFrame::Command(cmd, seq) => {
                                    // Process command synchronously via router
                                    let events = router.process(cmd);
                                    codec::encode_events(&events, seq, &mut write_buf);
                                }
                            }

                            // Backpressure check: if write buffer is too large,
                            // send an error and drop the connection rather than
                            // blocking the engine thread.
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
