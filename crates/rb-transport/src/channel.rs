//! PairedChannel implementation for rb-transport.

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Mutex};
use tokio::time::{timeout, Duration};
use tracing::warn;

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
pub(crate) const CTRL_CLIENT_HEARTBEAT: Duration = Duration::from_secs(20);

/// A paired byte channel between source and destination.
pub struct PairedChannel {
    inner: std::sync::Arc<Mutex<PairedChannelInner>>,
    carriers: usize,
    /// Drives the control-plane keepalive (client→server heartbeats) and drains
    /// inbound server frames. Owns the control substream — nothing else uses it
    /// after setup. Aborted when the channel drops.
    _heartbeat: AbortOnDrop,
    /// Set by the keepalive driver when the control stream ends, with why. A
    /// source waiting for the destination's stream must not outlive its
    /// registration: once the coordinator has dropped it, no destination can
    /// ever reach it.
    control_lost: watch::Receiver<Option<String>>,
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
        let (lost_tx, control_lost) = watch::channel(None);
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
            _heartbeat: AbortOnDrop(tokio::spawn(drive_control(control, lost_tx))),
            control_lost,
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
        let (lost_tx, control_lost) = watch::channel(None);
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
            _heartbeat: AbortOnDrop(tokio::spawn(drive_control(control, lost_tx))),
            control_lost,
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
                                tracing::info!("stream to the source opened on the direct path");
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
                tracing::info!("stream to the source opened through the coordinator relay");
                Ok(Box::new(s))
            }
            PairedChannelInner::Source { .. } => Err(BackupError::phase(
                Phase::Connect,
                "source cannot open streams",
            )),
        }
    }

    async fn accept_stream(&self) -> Result<Box<dyn rb_core::channel::DuplexStream>> {
        let mut lost = self.control_lost.clone();
        if let Some(reason) = lost.borrow().clone() {
            return Err(registration_lost(&reason));
        }
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
                                    tracing::info!(
                                        "stream from the destination accepted on the direct path"
                                    );
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
                let stream = tokio::select! {
                    stream = acceptor.accept() => stream.ok_or_else(|| {
                        BackupError::phase(
                            Phase::Connect,
                            "the connection to the coordination server closed while waiting \
                             for the destination",
                        )
                    })?,
                    reason = wait_control_lost(&mut lost) => {
                        return Err(registration_lost(&reason));
                    }
                };
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
                tracing::info!(
                    "stream from the destination accepted through the coordinator relay"
                );
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
/// Wait until the keepalive driver reports the control stream gone.
async fn wait_control_lost(lost: &mut watch::Receiver<Option<String>>) -> String {
    loop {
        if let Some(reason) = lost.borrow_and_update().clone() {
            return reason;
        }
        if lost.changed().await.is_err() {
            // The driver was dropped without a verdict: the channel itself is
            // going away, so nothing is lost that the caller can act on.
            std::future::pending::<()>().await;
        }
    }
}

fn registration_lost(reason: &str) -> BackupError {
    BackupError::phase(
        Phase::Connect,
        format!(
            "the coordination server no longer has this source registered ({reason}); it drops \
             a peer whose heartbeats stop for 60 s (a network cut, a blocked process) or it \
             restarted, so no destination can reach this source any more — start the source \
             again"
        ),
    )
}

async fn drive_control(mut control: Delimited<mux::Stream>, lost: watch::Sender<Option<String>>) {
    let reason = drive_control_until_lost(&mut control).await;
    warn!("control stream to the coordination server lost: {reason}");
    let _ = lost.send(Some(reason));
}

async fn drive_control_until_lost(control: &mut Delimited<mux::Stream>) -> String {
    let mut tick = tokio::time::interval(CTRL_CLIENT_HEARTBEAT);
    // The first tick fires immediately; skip it so the first heartbeat lands one
    // full interval after setup (setup already proved the link live).
    tick.tick().await;
    loop {
        // Every exit here ends the keepalive, so every exit says why. Without a
        // line the only trace a dying control plane left was the coordination
        // server's reap 60 s later, which names the symptom and never the side
        // that failed first (I-OBSERV).
        tokio::select! {
            _ = tick.tick() => {
                if let Err(error) = control.send_client(ClientMsg::Heartbeat).await {
                    return format!("heartbeat send failed: {error}");
                }
            }
            msg = control.recv_server() => {
                match msg {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        return "the coordination server closed the control stream".to_string();
                    }
                    Err(error) => {
                        return format!("control stream read failed: {error}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rb_core::channel::DataChannel;

    /// The coordinator reaps a registration whose heartbeats stopped. The
    /// source waiting for the destination's stream must then fail with that
    /// reason instead of waiting for a destination that can no longer arrive.
    #[tokio::test]
    async fn a_source_whose_control_stream_closes_stops_waiting() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (opener, acceptor) = mux::client(client_io);
        let (_server_opener, mut server_acceptor) = mux::server(server_io);
        let mut control = Delimited::new(opener.open().await.expect("open control"));
        // The first write flushes the lazy SYN so the server side sees it.
        control
            .send_client(ClientMsg::Heartbeat)
            .await
            .expect("first frame");
        let server_control = server_acceptor.accept().await.expect("server control");
        let channel = PairedChannel::source(acceptor, control);

        drop(server_control);
        let outcome = timeout(Duration::from_secs(5), channel.accept_stream())
            .await
            .expect("a lost registration must end the wait");
        let error = match outcome {
            Ok(_) => panic!("no destination stream can exist"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("no longer has this source registered"),
            "{error}"
        );
    }
}
