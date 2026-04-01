use std::sync::Arc;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use me_engine::router::ShardedRouterV2;

use crate::codec;

/// TCP server for the binary protocol.
///
/// Architecture:
/// - tokio for async accept + read/write (I/O multiplexing only)
/// - Matching logic is synchronous via ShardedRouterV2 (crossbeam channels)
/// - Each TCP connection is handled in its own tokio task
pub struct TcpServer {
    router: Arc<ShardedRouterV2>,
}

impl TcpServer {
    pub fn new(router: Arc<ShardedRouterV2>) -> Self {
        Self { router }
    }

    /// Start listening on the given address.
    pub async fn listen(&self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;

        loop {
            let (stream, peer) = listener.accept().await?;
            let router = Arc::clone(&self.router);

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, router).await {
                    eprintln!("Connection from {peer} error: {e}");
                }
            });
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    router: Arc<ShardedRouterV2>,
) -> std::io::Result<()> {
    let mut read_buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);

    loop {
        // Read data from client
        let n = stream.read_buf(&mut read_buf).await?;
        if n == 0 {
            return Ok(()); // Connection closed
        }

        // Try to decode frames from the buffer
        loop {
            match codec::decode_frame(&read_buf) {
                Ok((cmd, seq, consumed)) => {
                    read_buf.advance(consumed);

                    // Process command synchronously via router
                    let events = router.process(cmd);

                    // Encode response
                    write_buf.clear();
                    codec::encode_events(&events, seq, &mut write_buf);

                    // Send response
                    stream.write_all(&write_buf).await?;
                }
                Err(codec::DecodeError::Incomplete) => break, // Need more data
                Err(e) => {
                    eprintln!("Decode error: {e}");
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{e}"),
                    ));
                }
            }
        }
    }
}

use bytes::Buf;
