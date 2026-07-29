//! PairedChannel implementation for rb-transport.

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

use rb_core::channel::DataChannel;
use rb_core::error::{BackupError, Phase, Result};

use crate::mux;
use crate::proto::{ClientMsg, Delimited};

#[cfg(feature = "udp")]
use crate::direct::DirectConn;

/// Timeout for direct connection attempts.
#[cfg(feature = "udp")]
const DIRECT_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the client sends a control-plane heartbeat so the coordination
/// server's recv-deadline reaper does not drop the channel (the server reaps
/// after `SECRET_CTRL_TIMEOUT` = 60 s of silence). A yamux substream hides a
/// half-open peer, so this app-level keepalive is what keeps the registry entry
/// live. Independent of the relay/direct split — it rides the relay control
/// substream, which always exists.
const CTRL_CLIENT_HEARTBEAT: Duration = Duration::from_secs(20);

/// A paired byte channel between source and destination.
pub struct PairedChannel {
    inner: std::sync::Arc<Mutex<PairedChannelInner>>,
    carriers: usize,
    /// Drives the control-plane keepalive (client→server heartbeats) and drains
    /// inbound server frames. Owns the control substream — nothing else uses it
    /// after setup. Aborted when the channel drops.
    _heartbeat: AbortOnDrop,
}

enum PairedChannelInner {
    Source {
        acceptor: mux::Acceptor,
        #[cfg(feature = "udp")]
        direct: Option<DirectConn>,
        #[cfg(feature = "udp")]
        direct_token: Option<[u8; crate::direct::TOKEN_LEN]>,
        #[cfg(feature = "udp")]
        direct_streams: usize,
    },
    Destination {
        opener: mux::Opener,
        #[cfg(feature = "udp")]
        direct: Option<DirectConn>,
        #[cfg(feature = "udp")]
        direct_token: Option<[u8; crate::direct::TOKEN_LEN]>,
        #[cfg(feature = "udp")]
        direct_streams: usize,
    },
}

impl PairedChannel {
    /// Create a new source-side PairedChannel (provider).
    pub fn source(acceptor: mux::Acceptor, control: Delimited<mux::Stream>) -> Self {
        Self::source_with_carriers(acceptor, control, 1)
    }

    /// Create a source channel with a negotiated number of item-pinned data
    /// carriers. Carrier zero is never the control stream when this is >1.
    pub fn source_with_carriers(
        acceptor: mux::Acceptor,
        control: Delimited<mux::Stream>,
        carriers: u32,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(PairedChannelInner::Source {
                acceptor,
                #[cfg(feature = "udp")]
                direct: None,
                #[cfg(feature = "udp")]
                direct_token: None,
                #[cfg(feature = "udp")]
                direct_streams: 0,
            })),
            carriers: carriers.clamp(1, 32) as usize,
            _heartbeat: AbortOnDrop(tokio::spawn(drive_control(control))),
        }
    }

    /// Create a new destination-side PairedChannel (consumer).
    pub fn destination(opener: mux::Opener, control: Delimited<mux::Stream>) -> Self {
        Self::destination_with_carriers(opener, control, 1)
    }

    /// Create a destination channel with a negotiated number of item-pinned
    /// data carriers. The destination opens each data substream after PlanAck.
    pub fn destination_with_carriers(
        opener: mux::Opener,
        control: Delimited<mux::Stream>,
        carriers: u32,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(PairedChannelInner::Destination {
                opener,
                #[cfg(feature = "udp")]
                direct: None,
                #[cfg(feature = "udp")]
                direct_token: None,
                #[cfg(feature = "udp")]
                direct_streams: 0,
            })),
            carriers: carriers.clamp(1, 32) as usize,
            _heartbeat: AbortOnDrop(tokio::spawn(drive_control(control))),
        }
    }

    /// Set the direct QUIC connection (Phase 1.3).
    #[cfg(feature = "udp")]
    pub async fn set_direct(&self, direct: DirectConn) {
        self.set_direct_with_token(direct, [0; crate::direct::TOKEN_LEN])
            .await;
    }

    /// Install a direct path plus its authentication token for sibling carrier
    /// connections. The compatibility setter above remains single-stream only.
    #[cfg(feature = "udp")]
    pub async fn set_direct_with_token(
        &self,
        direct: DirectConn,
        token: [u8; crate::direct::TOKEN_LEN],
    ) {
        let mut inner = self.inner.lock().await;
        match &mut *inner {
            PairedChannelInner::Source {
                direct: d,
                direct_token,
                direct_streams,
                ..
            } => {
                *d = Some(direct);
                *direct_token = Some(token);
                *direct_streams = 0;
            }
            PairedChannelInner::Destination {
                direct: d,
                direct_token,
                direct_streams,
                ..
            } => {
                *d = Some(direct);
                *direct_token = Some(token);
                *direct_streams = 0;
            }
        }
    }

    /// Whether a direct QUIC connection is currently active (vs. relay-only).
    #[cfg(feature = "udp")]
    pub async fn is_direct(&self) -> bool {
        let inner = self.inner.lock().await;
        match &*inner {
            PairedChannelInner::Source { direct, .. } => direct.is_some(),
            PairedChannelInner::Destination { direct, .. } => direct.is_some(),
        }
    }
}

#[async_trait]
impl DataChannel for PairedChannel {
    async fn open_stream(&self) -> Result<Box<dyn rb_core::channel::DuplexStream>> {
        let mut inner = self.inner.lock().await;
        match &mut *inner {
            PairedChannelInner::Destination {
                opener,
                #[cfg(feature = "udp")]
                direct,
                #[cfg(feature = "udp")]
                direct_token,
                #[cfg(feature = "udp")]
                direct_streams,
                ..
            } => {
                #[cfg(feature = "udp")]
                if let (Some(dc), Some(token)) = (direct, direct_token) {
                    let result: anyhow::Result<_> = async {
                        if *direct_streams == 0 {
                            timeout(DIRECT_SETUP_TIMEOUT, dc.open_stream())
                                .await
                                .map_err(|_| anyhow::anyhow!("direct stream open timed out"))?
                        } else {
                            let sibling = timeout(DIRECT_SETUP_TIMEOUT, dc.open_sibling(*token))
                                .await
                                .map_err(|_| anyhow::anyhow!("direct sibling open timed out"))??;
                            timeout(DIRECT_SETUP_TIMEOUT, sibling.open_stream())
                                .await
                                .map_err(|_| anyhow::anyhow!("direct sibling stream timed out"))?
                        }
                    }
                    .await;
                    match result {
                        Ok(mut qt) => {
                            // QUIC does not expose a newly opened bidi stream to
                            // the peer until the opener writes data.  Without
                            // this marker the destination waited for Plan while
                            // the source waited in accept_bi: a symmetric
                            // ten-second deadlock followed by split fallback.
                            if let Err(e) = qt.write_all(&[mux::STREAM_READY]).await {
                                tracing::warn!(
                                    "direct stream ready write failed, falling back to relay: {e}"
                                );
                            } else if let Err(e) = qt.flush().await {
                                tracing::warn!(
                                    "direct stream ready flush failed, falling back to relay: {e}"
                                );
                            } else {
                                *direct_streams += 1;
                                return Ok(Box::new(qt));
                            }
                        }
                        Err(e) => {
                            tracing::warn!("direct stream open failed, falling back to relay: {e}");
                        }
                    }
                }

                // Fallback to relay
                let stream = opener
                    .open()
                    .await
                    .map_err(|e| BackupError::phase(Phase::Connect, format!("open stream: {e}")))?;
                let mut s = stream;
                s.write_all(&[mux::STREAM_READY])
                    .await
                    .map_err(|e| BackupError::phase(Phase::Connect, format!("write ready: {e}")))?;
                Ok(Box::new(s))
            }
            PairedChannelInner::Source { .. } => Err(BackupError::phase(
                Phase::Connect,
                "source cannot open streams",
            )),
        }
    }

    async fn accept_stream(&self) -> Result<Box<dyn rb_core::channel::DuplexStream>> {
        let mut inner = self.inner.lock().await;
        match &mut *inner {
            PairedChannelInner::Source {
                acceptor,
                #[cfg(feature = "udp")]
                direct,
                #[cfg(feature = "udp")]
                direct_token,
                #[cfg(feature = "udp")]
                direct_streams,
                ..
            } => {
                #[cfg(feature = "udp")]
                if let (Some(dc), Some(token)) = (direct, direct_token) {
                    let result: anyhow::Result<_> = async {
                        if *direct_streams == 0 {
                            timeout(DIRECT_SETUP_TIMEOUT, dc.accept_stream())
                                .await
                                .map_err(|_| anyhow::anyhow!("direct stream accept timed out"))?
                        } else {
                            let sibling = timeout(DIRECT_SETUP_TIMEOUT, dc.accept_sibling(*token))
                                .await
                                .map_err(|_| {
                                    anyhow::anyhow!("direct sibling accept timed out")
                                })??;
                            timeout(DIRECT_SETUP_TIMEOUT, sibling.accept_stream())
                                .await
                                .map_err(|_| {
                                    anyhow::anyhow!("direct sibling stream accept timed out")
                                })?
                        }
                    }
                    .await;
                    match result {
                        Ok(mut qt) => {
                            let mut marker = [0u8; 1];
                            let ready =
                                timeout(DIRECT_SETUP_TIMEOUT, qt.read_exact(&mut marker)).await;
                            match ready {
                                Ok(Ok(_)) if marker[0] == mux::STREAM_READY => {
                                    *direct_streams += 1;
                                    return Ok(Box::new(qt));
                                }
                                Ok(Ok(_)) => tracing::warn!(
                                    "invalid direct stream ready marker, falling back to relay"
                                ),
                                Ok(Err(e)) => tracing::warn!(
                                    "direct stream ready read failed, falling back to relay: {e}"
                                ),
                                Err(_) => tracing::warn!(
                                    "direct stream ready read timed out, falling back to relay"
                                ),
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "direct stream accept failed, falling back to relay: {e}"
                            );
                        }
                    }
                }

                // Fallback to relay
                let stream = acceptor
                    .accept()
                    .await
                    .ok_or_else(|| BackupError::phase(Phase::Connect, "acceptor closed"))?;
                let mut s = stream;
                let mut marker = [0u8; 1];
                s.read_exact(&mut marker)
                    .await
                    .map_err(|e| BackupError::phase(Phase::Connect, format!("read ready: {e}")))?;
                if marker[0] != mux::STREAM_READY {
                    return Err(BackupError::phase(
                        Phase::Connect,
                        "invalid stream ready marker",
                    ));
                }
                Ok(Box::new(s))
            }
            PairedChannelInner::Destination { .. } => Err(BackupError::phase(
                Phase::Connect,
                "destination cannot accept streams",
            )),
        }
    }

    fn carriers(&self) -> usize {
        self.carriers
    }
}

/// Aborts the wrapped task on drop, so the control-plane keepalive never outlives
/// the channel.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Control-plane keepalive driver. Owns the control substream for the channel's
/// lifetime: emits a client heartbeat every `CTRL_CLIENT_HEARTBEAT` and drains
/// inbound server frames (heartbeats plus any late broker messages). Returns when
/// the control stream errors or closes; the task is aborted when the channel drops.
async fn drive_control(mut control: Delimited<mux::Stream>) {
    let mut tick = tokio::time::interval(CTRL_CLIENT_HEARTBEAT);
    // The first tick fires immediately; skip it so the first heartbeat lands one
    // full interval after setup (setup already proved the link live).
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if control.send_client(ClientMsg::Heartbeat).await.is_err() {
                    return;
                }
            }
            msg = control.recv_server() => {
                match msg {
                    Ok(Some(_)) => {}
                    _ => return,
                }
            }
        }
    }
}
