use std::sync::Arc;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use me_engine::router::ShardedRouterV2;

use crate::codec::{self, ConnectionCodecState, DecodedFrame};
use crate::protocol::{ConnectionState, ErrorCode, MAX_WRITE_BUFFER_SIZE, PROTOCOL_VERSION};

/// Unix Domain Socket server.
///
/// Identical protocol to TCP but ~30% lower latency for co-located processes
/// since it bypasses the TCP/IP stack.
pub struct UdsServer {
    router: Arc<ShardedRouterV2>,
}

impl UdsServer {
    pub fn new(router: Arc<ShardedRouterV2>) -> Self {
        Self { router }
    }

    /// Start listening on the given Unix socket path.
    ///
    /// `shutdown_rx` is a watch channel that signals when the server should stop
    /// accepting new connections and drain existing ones.
    pub async fn listen(
        &self,
        path: &str,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(path);

        let listener = UnixListener::bind(path)?;

        loop {
            tokio::select! {
                biased;

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        eprintln!("UDS server shutting down");
                        // Clean up socket file
                        let _ = std::fs::remove_file(path);
                        break;
                    }
                }

                result = listener.accept() => {
                    let (stream, _) = result?;
                    let router = Arc::clone(&self.router);
                    let conn_shutdown = shutdown_rx.clone();

                    tokio::spawn(async move {
                        if let Err(e) = handle_uds_connection(stream, router, conn_shutdown).await {
                            eprintln!("UDS connection error: {e}");
                        }
                    });
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
) -> std::io::Result<()> {
    let mut read_buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);
    let mut conn = ConnectionCodecState::new();

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
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
                    return Ok(());
                }

                loop {
                    match codec::decode_frame(&read_buf, &mut conn) {
                        Ok((frame, consumed)) => {
                            read_buf.advance(consumed);

                            write_buf.clear();
                            match frame {
                                DecodedFrame::Handshake { client_version, seq } => {
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
                                        return Ok(());
                                    }
                                    conn.state = ConnectionState::Active;
                                    codec::encode_handshake_ack(seq, &mut write_buf);
                                }
                                DecodedFrame::Ping(seq) => {
                                    codec::encode_pong(seq, &mut write_buf);
                                }
                                DecodedFrame::Command(cmd, seq) => {
                                    let events = router.process(cmd);
                                    codec::encode_events(&events, seq, &mut write_buf);
                                }
                            }

                            if write_buf.len() > MAX_WRITE_BUFFER_SIZE {
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

                            stream.write_all(&write_buf).await?;
                        }
                        Err(codec::DecodeError::Incomplete) => break,
                        Err(codec::DecodeError::SequenceOutOfOrder { expected_min, got }) => {
                            write_buf.clear();
                            codec::encode_error(
                                got,
                                ErrorCode::SequenceOutOfOrder,
                                &format!("Expected seq >= {expected_min}, got {got}"),
                                &mut write_buf,
                            );
                            stream.write_all(&write_buf).await?;
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Sequence out of order",
                            ));
                        }
                        Err(codec::DecodeError::HandshakeRequired) => {
                            write_buf.clear();
                            codec::encode_error(
                                0,
                                ErrorCode::HandshakeRequired,
                                "Send a Handshake message first",
                                &mut write_buf,
                            );
                            stream.write_all(&write_buf).await?;
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Handshake required",
                            ));
                        }
                        Err(codec::DecodeError::UnsupportedVersion(v)) => {
                            write_buf.clear();
                            codec::encode_error(
                                0,
                                ErrorCode::UnsupportedVersion,
                                &format!("Unsupported version: {v}"),
                                &mut write_buf,
                            );
                            stream.write_all(&write_buf).await?;
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Unsupported protocol version: {v}"),
                            ));
                        }
                        Err(e) => {
                            write_buf.clear();
                            codec::encode_error(
                                0,
                                ErrorCode::InvalidPayload,
                                &format!("{e}"),
                                &mut write_buf,
                            );
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
