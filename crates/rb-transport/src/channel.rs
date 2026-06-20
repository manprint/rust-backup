//! PairedChannel implementation for rb-transport.

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use rb_core::channel::DataChannel;
use rb_core::error::{BackupError, Phase, Result};

use crate::mux;
use crate::proto::Delimited;

/// A paired byte channel between source and destination.
pub struct PairedChannel {
    inner: std::sync::Arc<Mutex<PairedChannelInner>>,
}

enum PairedChannelInner {
    Source {
        acceptor: mux::Acceptor,
        #[allow(dead_code)]
        control: Delimited<mux::Stream>,
    },
    Destination {
        opener: mux::Opener,
        #[allow(dead_code)]
        control: Delimited<mux::Stream>,
    },
}

impl PairedChannel {
    /// Create a source/provider channel.
    pub fn source(acceptor: mux::Acceptor, control: Delimited<mux::Stream>) -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(PairedChannelInner::Source {
                acceptor,
                control,
            })),
        }
    }

    /// Create a destination/consumer channel.
    pub fn destination(opener: mux::Opener, control: Delimited<mux::Stream>) -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(PairedChannelInner::Destination {
                opener,
                control,
            })),
        }
    }
}

#[async_trait]
impl DataChannel for PairedChannel {
    async fn open_stream(&self) -> Result<Box<dyn rb_core::channel::DuplexStream>> {
        let inner = self.inner.lock().await;
        match &*inner {
            PairedChannelInner::Destination { opener, .. } => {
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
            PairedChannelInner::Source { acceptor, .. } => {
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
        1
    }
}
