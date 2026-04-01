use std::sync::Arc;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use me_engine::router::ShardedRouterV2;

use crate::codec;

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
    pub async fn listen(&self, path: &str) -> std::io::Result<()> {
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(path);

        let listener = UnixListener::bind(path)?;

        loop {
            let (stream, _) = listener.accept().await?;
            let router = Arc::clone(&self.router);

            tokio::spawn(async move {
                if let Err(e) = handle_uds_connection(stream, router).await {
                    eprintln!("UDS connection error: {e}");
                }
            });
        }
    }
}

async fn handle_uds_connection(
    mut stream: UnixStream,
    router: Arc<ShardedRouterV2>,
) -> std::io::Result<()> {
    let mut read_buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);

    loop {
        let n = stream.read_buf(&mut read_buf).await?;
        if n == 0 {
            return Ok(());
        }

        loop {
            match codec::decode_frame(&read_buf) {
                Ok((cmd, seq, consumed)) => {
                    read_buf.advance(consumed);

                    let events = router.process(cmd);

                    write_buf.clear();
                    codec::encode_events(&events, seq, &mut write_buf);

                    stream.write_all(&write_buf).await?;
                }
                Err(codec::DecodeError::Incomplete) => break,
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{e}"),
                    ));
                }
            }
        }
    }
}
