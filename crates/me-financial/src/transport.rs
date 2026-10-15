//! Additive financial messages on the v2 frame header; all amounts live in JSON contracts.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::{
    self,
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
    RootCertStore,
};

pub const HANDSHAKE: u8 = 0x05;
pub const HANDSHAKE_ACK: u8 = 0x1a;
pub const FINANCIAL_COMMAND: u8 = 0x30;
pub const FINANCIAL_DECISIONS: u8 = 0x31;
pub const FINANCIAL_IDENTITY: u8 = 0x32;
pub const FINANCIAL_MAINTENANCE: u8 = 0x33;
pub const FINANCIAL_REPLY: u8 = 0x38;
pub const FINANCIAL_CAPABILITIES: u64 = (1 << 5) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11);
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub certificate: String,
    pub private_key: String,
    pub ca: String,
}
impl TlsFiles {
    fn roots(&self) -> io::Result<RootCertStore> {
        let mut roots = RootCertStore::empty();
        for cert in certificates(&self.ca)? {
            roots.add(cert).map_err(error)?;
        }
        if roots.is_empty() {
            return Err(error("empty financial CA"));
        }
        Ok(roots)
    }
    pub fn server(&self) -> io::Result<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(self.roots()?), provider.clone())
                .build()
                .map_err(error)?;
        rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(error)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                certificates(&self.certificate)?,
                PrivateKeyDer::from_pem_file(&self.private_key).map_err(error)?,
            )
            .map_err(error)
    }
    pub fn client(&self) -> io::Result<rustls::ClientConfig> {
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(error)?
        .with_root_certificates(self.roots()?)
        .with_client_auth_cert(
            certificates(&self.certificate)?,
            PrivateKeyDer::from_pem_file(&self.private_key).map_err(error)?,
        )
        .map_err(error)
    }
}
fn certificates(path: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .map_err(error)?
        .map(|r| r.map_err(error))
        .collect()
}
pub fn peer_identity(chain: Option<&[CertificateDer<'_>]>) -> io::Result<String> {
    let cert = chain
        .and_then(|c| c.first())
        .ok_or_else(|| error("missing peer certificate"))?;
    Ok(format!("{:x}", Sha256::digest(cert.as_ref())))
}
pub fn validate_fingerprint(value: &str) -> io::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error(
            "certificate fingerprint must be 64 lowercase hex characters",
        ));
    }
    Ok(())
}
pub struct Frame {
    pub message_type: u8,
    pub sequence: u64,
    pub request_id: u64,
    pub payload: Vec<u8>,
}
impl Frame {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(error("financial frame too large"));
        }
        let mut bytes = Vec::with_capacity(22 + self.payload.len());
        bytes.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        bytes.push(2);
        bytes.push(self.message_type);
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.extend_from_slice(&self.request_id.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }
    pub async fn read(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Self> {
        let mut header = [0; 22];
        stream.read_exact(&mut header).await?;
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        if header[4] != 2 || len > MAX_PAYLOAD {
            return Err(error("unsupported financial frame version/length"));
        }
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload).await?;
        Ok(Self {
            message_type: header[5],
            sequence: u64::from_le_bytes(header[6..14].try_into().unwrap()),
            request_id: u64::from_le_bytes(header[14..22].try_into().unwrap()),
            payload,
        })
    }
    pub async fn write(&self, stream: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        stream.write_all(&self.encode()?).await
    }
    pub fn json(message_type: u8, request_id: u64, value: &impl Serialize) -> io::Result<Self> {
        Ok(Self {
            message_type,
            sequence: 0,
            request_id,
            payload: serde_json::to_vec(value).map_err(error)?,
        })
    }
}
pub fn hello() -> Frame {
    Frame {
        message_type: HANDSHAKE,
        sequence: 0,
        request_id: 0,
        payload: capability_payload(),
    }
}
pub fn accept_hello(frame: &Frame) -> io::Result<Frame> {
    if frame.message_type != HANDSHAKE
        || frame.sequence != 0
        || frame.request_id != 0
        || !has_capabilities(&frame.payload)
    {
        return Err(error("financial capabilities required"));
    }
    Ok(Frame {
        message_type: HANDSHAKE_ACK,
        sequence: 0,
        request_id: 0,
        payload: capability_payload(),
    })
}
pub fn verify_hello(frame: &Frame) -> io::Result<()> {
    if frame.message_type != HANDSHAKE_ACK
        || frame.sequence != 0
        || frame.request_id != 0
        || !has_capabilities(&frame.payload)
    {
        return Err(error("server lacks mandatory financial capabilities"));
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}
pub fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn capability_payload() -> Vec<u8> {
    let mut payload = vec![2];
    payload.extend_from_slice(&1u16.to_le_bytes());
    payload.extend_from_slice(&8u16.to_le_bytes());
    payload.extend_from_slice(&FINANCIAL_CAPABILITIES.to_le_bytes());
    payload
}
fn has_capabilities(payload: &[u8]) -> bool {
    if payload.first() != Some(&2) {
        return false;
    }
    let mut offset = 1;
    let mut caps = None;
    while offset < payload.len() {
        if payload.len() - offset < 4 {
            return false;
        }
        let tag = u16::from_le_bytes(payload[offset..offset + 2].try_into().unwrap());
        let len = u16::from_le_bytes(payload[offset + 2..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if payload.len() - offset < len {
            return false;
        }
        if tag == 1 {
            if len != 8 || caps.is_some() {
                return false;
            }
            caps = Some(u64::from_le_bytes(
                payload[offset..offset + len].try_into().unwrap(),
            ));
        }
        offset += len;
    }
    caps.is_some_and(|mask| mask & FINANCIAL_CAPABILITIES == FINANCIAL_CAPABILITIES)
}
