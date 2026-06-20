//! Automatic reconnection with capped exponential backoff.
//! Vendored from bore with zero modifications.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio::time::sleep;
use tracing::{info, warn};

const DEFAULT_INITIAL_BACKOFF_SECS: u64 = 1;
const DEFAULT_MAX_BACKOFF_SECS: u64 = 32;

#[derive(Debug)]
pub struct Backoff {
    next_secs: u64,
    max_secs: u64,
    initial_secs: u64,
}

impl Backoff {
    pub fn new() -> Self {
        Self::new_with(DEFAULT_INITIAL_BACKOFF_SECS, DEFAULT_MAX_BACKOFF_SECS)
    }

    pub fn new_with(initial_secs: u64, max_secs: u64) -> Self {
        let next_secs = initial_secs.min(max_secs);
        Self {
            next_secs,
            max_secs,
            initial_secs,
        }
    }

    pub fn peek(&self) -> Duration {
        Duration::from_secs(self.next_secs)
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = Duration::from_secs(self.next_secs);
        self.next_secs = self.next_secs.saturating_mul(2).min(self.max_secs);
        delay
    }

    pub fn reset(&mut self) {
        self.next_secs = self.initial_secs;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run<Connect, ConnectFut, Handle, Serve, ServeFut>(
    auto_reconnect: bool,
    mut connect: Connect,
    mut serve: Serve,
) -> Result<()>
where
    Connect: FnMut() -> ConnectFut,
    ConnectFut: Future<Output = Result<Handle>>,
    Serve: FnMut(Handle) -> ServeFut,
    ServeFut: Future<Output = Result<()>>,
{
    if !auto_reconnect {
        let handle = connect().await?;
        return serve(handle).await;
    }

    let mut backoff = Backoff::new();
    loop {
        match connect().await {
            Ok(handle) => {
                info!("connected");
                backoff.reset();
                match serve(handle).await {
                    Ok(()) => info!("connection closed; reconnecting"),
                    Err(err) => warn!(%err, "connection closed with error; reconnecting"),
                }
            }
            Err(err) => warn!(%err, "failed to connect; retrying"),
        }
        let delay = backoff.next_delay();
        info!(seconds = delay.as_secs(), "reconnecting after backoff");
        sleep(delay).await;
    }
}
