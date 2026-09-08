//! Minimal control protocol for rb-transport coordination.

use std::net::SocketAddr;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use uuid::Uuid;

use crate::adaptive_nat::NatProfile;

/// Max frame length for a JSON message on the control channel. Matches bore's
/// `shared::MAX_FRAME_LENGTH`: the coordination server forwards up to
/// [`crate::shared::MAX_UDP_CANDIDATES`] sanitized candidates in one
/// [`ServerMsg::UdpPunch`], which does not fit in a smaller frame once the
/// addresses are IPv6. It is ENFORCED on the read path so a peer cannot grow the
/// server's buffer without bound by never sending the delimiter.
pub const MAX_FRAME_LENGTH: usize = 8192;
/// A v2 offer carries both the legacy address list and richer candidates. Eight
/// entries keep its maximum emitted JSON frame below [`MAX_FRAME_LENGTH`].
pub const MAX_V2_OFFER_CANDIDATES: usize = 8;
/// Deadline for a handshake frame that the peer owes us immediately (the first
/// Register/Connect, the auth challenge and its answer). Ported from bore's
/// `Delimited::recv_timeout`: without it a peer that opens a control substream
/// and then goes silent pins a server task forever. Long-lived loops
/// (`serve_control`, `drive_control`) deliberately use the untimed form.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Candidate metadata added in UDP offer v2. `addrs` remains in the message for
/// old peers; new peers use this to retain priority and provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UdpCandidateKind {
    Host,
    Reflexive,
    Mapped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpCandidate {
    pub addr: SocketAddr,
    pub kind: UdpCandidateKind,
    pub priority: u16,
}

/// Client → Server: protocol messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    /// Register as the source/provider for a channel.
    Register { channel: String },
    /// Connect as the destination/consumer for a channel.
    Connect { channel: String },
    /// Authenticate with the server using the provided HMAC tag.
    ///
    /// This must remain a struct variant: internally tagged Serde enums cannot
    /// serialize a newtype variant whose payload is a scalar.
    Authenticate { tag: String },
    /// Heartbeat to keep the connection alive.
    Heartbeat,
    /// Offer UDP candidate addresses for hole-punching to the server.
    UdpCandidateOffer {
        /// Legacy v1 address list. Kept for old peer compatibility.
        addrs: Vec<SocketAddr>,
        #[serde(default)]
        candidates: Vec<UdpCandidate>,
        #[serde(default)]
        generation: u32,
        #[serde(default)]
        nat_profile: NatProfile,
    },
}

/// Server → Client: protocol messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMsg {
    /// Authentication challenge (UUID); client must respond with Authenticate.
    /// See [`ClientMsg::Authenticate`] for why this is a struct variant.
    Challenge { challenge: Uuid },
    /// Success (registration/connection/auth accepted).
    Ok,
    /// Heartbeat echo.
    Heartbeat,
    /// Error message; connection will close.
    Error { reason: String },
    /// Forward the peer's UDP candidate addresses for hole-punching.
    UdpPunch {
        peer_addrs: Vec<SocketAddr>,
        #[serde(default)]
        peer_candidates: Vec<UdpCandidate>,
        #[serde(default)]
        peer_profile: NatProfile,
    },
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

    /// Send one null-delimited JSON frame, refusing to emit an over-long one.
    async fn send<F: Serialize>(&mut self, msg: &F) -> anyhow::Result<()> {
        let json = serde_json::to_vec(msg)?;
        anyhow::ensure!(
            json.len() < MAX_FRAME_LENGTH,
            "control frame of {} bytes exceeds MAX_FRAME_LENGTH ({MAX_FRAME_LENGTH})",
            json.len()
        );
        self.inner.write_all(&json).await?;
        self.inner.write_all(b"\0").await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Receive one null-delimited JSON frame.
    ///
    /// The accumulated buffer is bounded by [`MAX_FRAME_LENGTH`]: a peer that
    /// streams bytes without ever sending the delimiter is refused instead of
    /// being allowed to allocate without bound inside the server.
    async fn recv<F: DeserializeOwned>(&mut self) -> anyhow::Result<Option<F>> {
        loop {
            if let Some(pos) = self.read_buf.iter().position(|&b| b == b'\0') {
                let frame = self.read_buf.drain(..=pos).collect::<Vec<_>>();
                let json = &frame[..frame.len() - 1];
                return Ok(Some(serde_json::from_slice(json)?));
            }
            anyhow::ensure!(
                self.read_buf.len() < MAX_FRAME_LENGTH,
                "control frame exceeds MAX_FRAME_LENGTH ({MAX_FRAME_LENGTH}) with no delimiter"
            );
            let mut buf = [0u8; 1024];
            match self.inner.read(&mut buf).await? {
                0 => return Ok(None),
                n => self.read_buf.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// Send a client message.
    pub async fn send_client(&mut self, msg: ClientMsg) -> anyhow::Result<()> {
        self.send(&msg).await
    }

    /// Receive a client message.
    pub async fn recv_client(&mut self) -> anyhow::Result<Option<ClientMsg>> {
        self.recv().await
    }

    /// Receive a client message the peer owes us now (see [`HANDSHAKE_TIMEOUT`]).
    pub async fn recv_client_timeout(&mut self) -> anyhow::Result<Option<ClientMsg>> {
        timeout(HANDSHAKE_TIMEOUT, self.recv_client())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for client handshake frame"))?
    }

    /// Send a server message.
    pub async fn send_server(&mut self, msg: ServerMsg) -> anyhow::Result<()> {
        self.send(&msg).await
    }

    /// Receive a server message.
    pub async fn recv_server(&mut self) -> anyhow::Result<Option<ServerMsg>> {
        self.recv().await
    }

    /// Receive a server message the peer owes us now (see [`HANDSHAKE_TIMEOUT`]).
    pub async fn recv_server_timeout(&mut self) -> anyhow::Result<Option<ServerMsg>> {
        timeout(HANDSHAKE_TIMEOUT, self.recv_server())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for server handshake frame"))?
    }
}
