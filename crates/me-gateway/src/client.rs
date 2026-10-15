use crate::config::Matcher;
use me_financial::transport::*;
use serde::de::DeserializeOwned;
use std::{io, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_rustls::{client::TlsStream, rustls::pki_types::ServerName, TlsConnector};

pub struct Client {
    config: Matcher,
    stream: Option<TlsStream<TcpStream>>,
    next_control: u64,
}
impl Client {
    pub fn new(config: Matcher) -> Self {
        Self {
            config,
            stream: None,
            next_control: 1u64 << 63,
        }
    }
    async fn connect(&mut self) -> io::Result<()> {
        let tls = TlsConnector::from(Arc::new(self.config.tls.client()?));
        let stream = TcpStream::connect(&self.config.address).await?;
        stream.set_nodelay(true)?;
        let mut stream = tls
            .connect(
                ServerName::try_from(self.config.server_name.clone()).map_err(error)?,
                stream,
            )
            .await?;
        if peer_identity(stream.get_ref().1.peer_certificates())?
            != self.config.certificate_fingerprint
        {
            return Err(error("matcher certificate identity changed"));
        }
        hello().write(&mut stream).await?;
        verify_hello(&Frame::read(&mut stream).await?)?;
        self.stream = Some(stream);
        Ok(())
    }
    /// A connection owns one reader. The worker resends persisted mutation frames unchanged.
    pub async fn exchange<T: DeserializeOwned>(
        &mut self,
        bytes: &[u8],
        request_id: u64,
    ) -> io::Result<T> {
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            if self.stream.is_none() {
                self.connect().await?;
            }
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(bytes).await?;
            let reply = Frame::read(stream).await?;
            if reply.message_type != FINANCIAL_REPLY
                || reply.request_id != request_id
                || reply.sequence != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "financial response correlation mismatch",
                ));
            }
            let reply: Reply = serde_json::from_slice(&reply.payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if let Some(reason) = reply.error {
                return Err(io::Error::new(
                    if reply.retryable {
                        io::ErrorKind::WouldBlock
                    } else {
                        io::ErrorKind::InvalidData
                    },
                    reason,
                ));
            }
            serde_json::from_value(
                reply
                    .result
                    .ok_or_else(|| error("missing financial result"))?,
            )
            .map_err(error)
        })
        .await;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                self.stream = None;
                Err(e)
            }
            Err(e) => {
                self.stream = None;
                Err(error(e))
            }
        }
    }
    pub async fn query<T: DeserializeOwned>(
        &mut self,
        message: u8,
        value: &impl serde::Serialize,
    ) -> io::Result<T> {
        let request_id = self.next_control;
        self.next_control = self
            .next_control
            .checked_add(1)
            .ok_or_else(|| error("control request counter exhausted"))?;
        let bytes = Frame::json(message, request_id, value)?.encode()?;
        self.exchange(&bytes, request_id).await
    }
}
