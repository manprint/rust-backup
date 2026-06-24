//! Minimal control protocol for rb-transport coordination.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

/// Max frame length for a JSON message on the control channel.
pub const MAX_FRAME_LENGTH: usize = 1024;

/// Client → Server: protocol messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    /// Register as the source/provider for a channel.
    Register { channel: String },
    /// Connect as the destination/consumer for a channel.
    Connect { channel: String },
    /// Authenticate with the server using the provided HMAC tag.
    Authenticate(String),
    /// Heartbeat to keep the connection alive.
    Heartbeat,
    /// Offer UDP candidate addresses for hole-punching to the server.
    UdpCandidateOffer { addrs: Vec<SocketAddr> },
}

/// Server → Client: protocol messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMsg {
    /// Authentication challenge (UUID); client must respond with Authenticate.
    Challenge(Uuid),
    /// Success (registration/connection/auth accepted).
    Ok,
    /// Heartbeat echo.
    Heartbeat,
    /// Error message; connection will close.
    Error(String),
    /// Forward the peer's UDP candidate addresses for hole-punching.
    UdpPunch { peer_addrs: Vec<SocketAddr> },
    /// Direct UDP path unavailable; proceed with relay fallback.
    UdpUnavailable,
}

/// Null-delimited JSON codec for control messages.
pub struct Delimited<T> {
    inner: T,
    read_buf: Vec<u8>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Delimited<T> {
    /// Wrap a stream with null-delimited JSON framing.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            read_buf: Vec::new(),
        }
    }

    /// Send a client message.
    pub async fn send_client(&mut self, msg: ClientMsg) -> anyhow::Result<()> {
        let json = serde_json::to_vec(&msg)?;
        self.inner.write_all(&json).await?;
        self.inner.write_all(b"\0").await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Receive a client message.
    pub async fn recv_client(&mut self) -> anyhow::Result<Option<ClientMsg>> {
        loop {
            if let Some(pos) = self.read_buf.iter().position(|&b| b == b'\0') {
                let frame = self.read_buf.drain(..=pos).collect::<Vec<_>>();
                let json = &frame[..frame.len() - 1];
                return Ok(Some(serde_json::from_slice(json)?));
            }
            let mut buf = [0u8; 1024];
            match self.inner.read(&mut buf).await? {
                0 => return Ok(None),
                n => self.read_buf.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// Send a server message.
    pub async fn send_server(&mut self, msg: ServerMsg) -> anyhow::Result<()> {
        let json = serde_json::to_vec(&msg)?;
        self.inner.write_all(&json).await?;
        self.inner.write_all(b"\0").await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Receive a server message.
    pub async fn recv_server(&mut self) -> anyhow::Result<Option<ServerMsg>> {
        loop {
            if let Some(pos) = self.read_buf.iter().position(|&b| b == b'\0') {
                let frame = self.read_buf.drain(..=pos).collect::<Vec<_>>();
                let json = &frame[..frame.len() - 1];
                return Ok(Some(serde_json::from_slice(json)?));
            }
            let mut buf = [0u8; 1024];
            match self.inner.read(&mut buf).await? {
                0 => return Ok(None),
                n => self.read_buf.extend_from_slice(&buf[..n]),
            }
        }
    }
}
