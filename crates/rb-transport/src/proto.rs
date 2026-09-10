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
            // Read no more than the frame bound still allows. Reading a full
            // 1 KiB into a buffer already at `MAX_FRAME_LENGTH - 1` let the
            // accumulated frame overshoot the bound this doc comment promises by
            // up to 1023 bytes before the next iteration noticed.
            let mut buf = [0u8; 1024];
            let allowance = (MAX_FRAME_LENGTH - self.read_buf.len()).min(buf.len());
            match self.inner.read(&mut buf[..allowance]).await? {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A peer that hands over `STEP` bytes per read and never a delimiter. The
    /// step is deliberately not a divisor of [`MAX_FRAME_LENGTH`], so the buffer
    /// lands just under the bound and the next read is the one that could
    /// overshoot it.
    struct Trickle;

    const STEP: usize = 700;

    impl AsyncRead for Trickle {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let n = STEP.min(buf.remaining());
            buf.put_slice(&vec![b'x'; n]);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for Trickle {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The refusal is not enough on its own: the buffer must never hold more
    /// than the bound it is refused against, whatever chunk sizes the peer uses.
    #[tokio::test]
    async fn the_frame_buffer_never_exceeds_the_bound() {
        let mut framed = Delimited::new(Trickle);
        let error = framed
            .recv_client()
            .await
            .expect_err("a frame with no delimiter must be refused");
        assert!(
            error.to_string().contains("MAX_FRAME_LENGTH"),
            "unexpected error: {error}"
        );
        assert!(
            framed.read_buf.len() <= MAX_FRAME_LENGTH,
            "the buffer grew to {} bytes, past MAX_FRAME_LENGTH ({MAX_FRAME_LENGTH})",
            framed.read_buf.len()
        );
    }
}
