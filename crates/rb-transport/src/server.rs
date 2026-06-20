//! Coordination server for rb-transport.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::interval;
use tracing::{error, info, info_span, warn, Instrument};

use rb_core::config::ServerConfig;

use crate::auth::Authenticator;
use crate::mux;
use crate::pool::{CarrierPool, PendingCarriers};
use crate::proto::{ClientMsg, Delimited, ServerMsg};
use crate::shared::{proxy_buffer_size, tune_tcp};

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

pub type Registry = Arc<DashMap<String, Arc<CarrierPool>>>;

pub async fn run_server(cfg: &ServerConfig) -> Result<()> {
    let addr = format!("{}:{}", cfg.bind_addr, cfg.control_port);
    let listener = TcpListener::bind(&addr)
        .await
        .context(format!("bind {addr}"))?;
    info!(addr, "rb-transport server listening");

    let registry = Arc::new(DashMap::new());
    let max_conns = Arc::new(Semaphore::new(cfg.max_conns));
    let pending_carriers = Arc::new(DashMap::new());
    let auth = cfg.secret.as_ref().map(|s| Authenticator::new(s));

    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                tune_tcp(&socket);
                let registry = Arc::clone(&registry);
                let max_conns = Arc::clone(&max_conns);
                let pending_carriers = Arc::clone(&pending_carriers);
                let auth_opt = auth.clone();
                tokio::spawn(
                    handle_conn(
                        socket,
                        peer,
                        registry,
                        max_conns,
                        pending_carriers,
                        auth_opt,
                    )
                    .instrument(info_span!("client", %peer)),
                );
            }
            Err(e) => {
                error!(%e, "accept error");
            }
        }
    }
}

async fn handle_conn(
    socket: TcpStream,
    peer: SocketAddr,
    registry: Registry,
    max_conns: Arc<Semaphore>,
    pending_carriers: PendingCarriers,
    auth: Option<Authenticator>,
) -> Result<()> {
    // The CLIENT opens the control substream and sends Register/Connect FIRST —
    // that first write flushes the lazy yamux SYN so our accept fires. Auth (if
    // any) happens AFTER reading the first message (bore's Hello-before-auth rule).
    let (opener, mut acceptor) = mux::server(socket);
    let ctrl_stream = match acceptor.accept().await {
        Some(s) => s,
        None => {
            warn!("client disconnected before control stream");
            return Ok(());
        }
    };
    let mut control = Delimited::new(ctrl_stream);

    let first = control.recv_client().await?;

    if let Some(authenticator) = auth {
        authenticator.server_handshake(&mut control).await?;
    }

    match first {
        Some(ClientMsg::Register { channel }) => {
            serve_provider(control, opener, registry, channel, peer, pending_carriers).await
        }
        Some(ClientMsg::Connect { channel }) => {
            serve_consumer(control, acceptor, registry, channel, peer, max_conns).await
        }
        Some(msg) => {
            warn!(?msg, "unexpected message before register/connect");
            Ok(())
        }
        None => {
            warn!("client disconnected before register/connect");
            Ok(())
        }
    }
}

async fn serve_provider(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    registry: Registry,
    id: String,
    _peer: SocketAddr,
    _pending_carriers: PendingCarriers,
) -> Result<()> {
    if registry.contains_key(&id) {
        warn!(%id, "channel id already in use");
        control
            .send_server(ServerMsg::Error("channel already in use".into()))
            .await?;
        return Ok(());
    }

    let pool = Arc::new(CarrierPool::new(opener));
    registry.insert(id.clone(), Arc::clone(&pool));
    let _guard = DropGuard {
        registry: Arc::clone(&registry),
        id: id.clone(),
    };

    control.send_server(ServerMsg::Ok).await?;
    info!(%id, "provider registered");

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send_server(ServerMsg::Heartbeat).await.is_err() {
                    return Ok(());
                }
            }
            msg = control.recv_client() => {
                match msg? {
                    Some(ClientMsg::Heartbeat) => {}
                    Some(_) => warn!(%id, "unexpected message from provider"),
                    None => return Ok(()),
                }
            }
        }
    }
}

async fn serve_consumer(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    registry: Registry,
    id: String,
    _peer: SocketAddr,
    max_conns: Arc<Semaphore>,
) -> Result<()> {
    control.send_server(ServerMsg::Ok).await?;
    info!(%id, "consumer connected");

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send_server(ServerMsg::Heartbeat).await.is_err() {
                    return Ok(());
                }
            }
            msg = control.recv_client() => {
                match msg? {
                    Some(ClientMsg::Heartbeat) => {}
                    Some(_) => warn!(%id, "unexpected message from consumer"),
                    None => return Ok(()),
                }
            }
            inbound = acceptor.accept() => {
                let Some(consumer_stream) = inbound else {
                    return Ok(());
                };
                let permit = match Arc::clone(&max_conns).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        warn!(%id, "too many active connections, dropping");
                        continue;
                    }
                };
                let registry = Arc::clone(&registry);
                let id = id.clone();
                tokio::spawn(
                    async move {
                        let _permit = permit;
                        if let Err(err) = relay(consumer_stream, registry, &id).await {
                            tracing::trace!(%err, "relay closed");
                        }
                    }
                    .instrument(info_span!("relay")),
                );
            }
        }
    }
}

async fn relay(mut consumer: mux::Stream, registry: Registry, id: &str) -> Result<()> {
    let mut marker = [0u8; 1];
    consumer.read_exact(&mut marker).await?;

    let pool = registry
        .get(id)
        .map(|entry| Arc::clone(entry.value()))
        .ok_or_else(|| anyhow::anyhow!("no provider registered for '{id}'"))?;
    let opener = pool.pick().context("no live provider carrier")?;
    let mut provider = opener.open().await.context("provider unavailable")?;
    provider.write_all(&[mux::STREAM_READY]).await?;

    let buf = proxy_buffer_size();
    tokio::io::copy_bidirectional_with_sizes(&mut consumer, &mut provider, buf, buf).await?;
    Ok(())
}

struct DropGuard {
    registry: Registry,
    id: String,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.id);
    }
}
