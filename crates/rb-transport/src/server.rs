//! Coordination server for rb-transport.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Semaphore};
use tokio::time::{interval, timeout, Instant};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, info_span, warn, Instrument};

use rb_core::config::ServerConfig;

use crate::auth::Authenticator;
use crate::mux;
use crate::pool::{CarrierPool, PendingCarriers};
use crate::proto::{ClientMsg, Delimited, ServerMsg, UdpCandidate};
use crate::shared::{proxy_buffer_size, tune_tcp};
use crate::transport::load_server_tls;

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const UDP_BROKER_TIMEOUT: Duration = Duration::from_secs(10);
/// A destination may be started before the source. Keep its lightweight control
/// connection pending for the same operator-scale window used by plan exchange,
/// rather than opening a relay stream that is discarded after ten seconds.
const PEER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Control substream recv deadline: the coordination server reaps a registry
/// entry whose control substream has been silent this long. A yamux substream
/// hides a half-open peer, so liveness needs an app-level recv deadline; the
/// provider/consumer must heartbeat well within it (see `CTRL_CLIENT_HEARTBEAT`).
const SECRET_CTRL_TIMEOUT: Duration = Duration::from_secs(60);
/// A peer that stops reading must not pin the control loop forever.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// A relayed substream owes its readiness marker immediately; this read holds a
/// `max_conns` permit, so it must never block indefinitely.
const STREAM_READY_TIMEOUT: Duration = Duration::from_secs(10);

pub type Registry = Arc<DashMap<String, Arc<CarrierPool>>>;

async fn send_control<S>(control: &mut Delimited<S>, msg: ServerMsg) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match timeout(CONTROL_SEND_TIMEOUT, control.send_server(msg)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            debug_assert!(!error.to_string().is_empty());
            false
        }
        Err(_) => false,
    }
}

/// Registry of UDP hole-punch matchmakers, keyed by channel id. Both the provider
/// and the consumer task `or_insert` the same matchmaker, so it exists regardless
/// of which side reaches the server first.
pub type UdpRegistry = Arc<DashMap<String, Arc<UdpMatchmaker>>>;

/// Cross-task rendezvous that exchanges the two peers' UDP hole-punch candidates.
///
/// Each side registers a one-shot sink for the *peer's* addresses and later offers
/// its own. The internal `try_match` operation fires both sinks exactly once when
/// peers have offered — so the exchange is fully order-independent (provider-first
/// or consumer-first). A side that never offers, or whose peer never offers,
/// leaves its receiver pending; the owning task's broker deadline then sends
/// `UdpUnavailable` and falls back to the relay.
#[derive(Default)]
pub struct UdpMatchmaker {
    inner: std::sync::Mutex<UdpMatchInner>,
}

#[derive(Default)]
struct UdpMatchInner {
    provider_addrs: Option<UdpOffer>,
    consumer_addrs: Option<UdpOffer>,
    /// Delivers the consumer's addresses to the provider task.
    to_provider: Option<oneshot::Sender<UdpOffer>>,
    /// Delivers the provider's addresses to the consumer task.
    to_consumer: Option<oneshot::Sender<UdpOffer>>,
}

#[derive(Clone, Debug)]
pub struct UdpOffer {
    pub addrs: Vec<SocketAddr>,
    pub candidates: Vec<UdpCandidate>,
    pub profile: crate::adaptive_nat::NatProfile,
}

impl UdpOffer {
    fn legacy(addrs: Vec<SocketAddr>) -> Self {
        Self {
            addrs,
            candidates: Vec::new(),
            profile: Default::default(),
        }
    }
}

impl UdpMatchmaker {
    fn lock(&self) -> std::sync::MutexGuard<'_, UdpMatchInner> {
        // The critical sections are tiny and panic-free; recover from a poisoned
        // lock rather than `unwrap`-panicking (no unwrap in production paths).
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Provider task: register the sink for the consumer's addresses.
    pub fn register_provider(&self) -> oneshot::Receiver<UdpOffer> {
        let (tx, rx) = oneshot::channel();
        let mut g = self.lock();
        g.to_provider = Some(tx);
        Self::try_match(&mut g);
        rx
    }

    /// Consumer task: register the sink for the provider's addresses.
    pub fn register_consumer(&self) -> oneshot::Receiver<UdpOffer> {
        let (tx, rx) = oneshot::channel();
        let mut g = self.lock();
        g.to_consumer = Some(tx);
        Self::try_match(&mut g);
        rx
    }

    /// Provider task: record this side's offered candidates.
    pub fn offer_provider(&self, addrs: Vec<SocketAddr>) {
        self.offer_provider_v2(UdpOffer::legacy(addrs));
    }

    pub fn offer_provider_v2(&self, offer: UdpOffer) {
        let mut g = self.lock();
        g.provider_addrs = Some(offer);
        Self::try_match(&mut g);
    }

    /// Consumer task: record this side's offered candidates.
    pub fn offer_consumer(&self, addrs: Vec<SocketAddr>) {
        self.offer_consumer_v2(UdpOffer::legacy(addrs));
    }

    pub fn offer_consumer_v2(&self, offer: UdpOffer) {
        let mut g = self.lock();
        g.consumer_addrs = Some(offer);
        Self::try_match(&mut g);
    }

    /// Fire both sinks (each gets the *other* side's addresses) once both peers
    /// have offered. Idempotent: the `take()`s ensure it delivers at most once.
    fn try_match(g: &mut UdpMatchInner) {
        let (Some(provider), Some(consumer)) = (&g.provider_addrs, &g.consumer_addrs) else {
            return;
        };
        if let Some(tx) = g.to_provider.take() {
            let _ = tx.send(consumer.clone());
        }
        if let Some(tx) = g.to_consumer.take() {
            let _ = tx.send(provider.clone());
        }
    }
}

/// Which peer a shared control loop serves — selects the matchmaker sink.
#[derive(Clone, Copy)]
enum BrokerSide {
    Provider,
    Consumer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderWait {
    Available,
    ClientClosed,
    TimedOut,
}

/// Wait for a source registration before acknowledging the destination. This
/// keeps destination-first startup order reliable without holding a relay
/// stream or a `max_conns` permit while an operator starts the other command.
async fn wait_for_provider<S>(
    control: &mut Delimited<S>,
    registry: &Registry,
    id: &str,
    wait: Duration,
) -> Result<ProviderWait>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if registry.contains_key(id) {
        return Ok(ProviderWait::Available);
    }

    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    let mut poll = interval(REGISTRY_POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = poll.tick() => {
                if registry.contains_key(id) {
                    return Ok(ProviderWait::Available);
                }
            }
            _ = &mut deadline => return Ok(ProviderWait::TimedOut),
            message = control.recv_client() => {
                match message? {
                    Some(ClientMsg::Heartbeat) => {}
                    Some(message) => warn!(%id, ?message, "unexpected message while waiting for provider"),
                    None => return Ok(ProviderWait::ClientClosed),
                }
            }
        }
    }
}

/// Shared control-plane loop for both peers. Sends the server heartbeat, brokers
/// the UDP hole-punch candidate exchange against [`UDP_BROKER_TIMEOUT`], and reaps
/// the peer if its control substream is silent past `reap_timeout` — a yamux
/// substream hides a half-open peer, so liveness needs this app-level recv
/// deadline. Returns `Ok(())` when the peer disconnects or is reaped. `reap_timeout`
/// is a parameter (not a global) so tests can drive it fast without racing.
async fn serve_control<S>(
    control: &mut Delimited<S>,
    matchmaker: &Arc<UdpMatchmaker>,
    side: BrokerSide,
    id: &str,
    reap_timeout: Duration,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut udp_rx = match side {
        BrokerSide::Provider => matchmaker.register_provider(),
        BrokerSide::Consumer => matchmaker.register_consumer(),
    };
    let udp_deadline = tokio::time::sleep(UDP_BROKER_TIMEOUT);
    tokio::pin!(udp_deadline);
    let mut udp_pending = true;

    // Reset on every inbound frame; fires only after a full silent `reap_timeout`.
    let reap = tokio::time::sleep(reap_timeout);
    tokio::pin!(reap);

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if !send_control(control, ServerMsg::Heartbeat).await {
                    return Ok(());
                }
            }
            res = &mut udp_rx, if udp_pending => {
                udp_pending = false;
                if let Ok(peer) = res {
                    info!(%id, peer_candidate_count = peer.addrs.len(), "received peer candidates");
                    if !send_control(
                        control,
                        ServerMsg::UdpPunch {
                            peer_addrs: peer.addrs,
                            peer_candidates: peer.candidates,
                            peer_profile: peer.profile,
                        },
                    ).await {
                        return Ok(());
                    }
                }
            }
            _ = &mut udp_deadline, if udp_pending => {
                udp_pending = false;
                warn!(%id, "udp broker timeout; falling back to relay");
                if !send_control(control, ServerMsg::UdpUnavailable).await {
                    return Ok(());
                }
            }
            _ = &mut reap => {
                warn!(%id, "control substream silent past deadline; reaping channel");
                return Ok(());
            }
            msg = control.recv_client() => {
                // Any inbound frame proves the peer is alive — push the reap deadline.
                reap.as_mut().reset(Instant::now() + reap_timeout);
                match msg? {
                    Some(ClientMsg::Heartbeat) => {}
                    Some(ClientMsg::UdpCandidateOffer { addrs, mut candidates, nat_profile, .. }) => {
                        // A peer-controlled list is sanitized BEFORE it is stored or
                        // forwarded: invalid entries dropped, deduped, capped at
                        // MAX_UDP_CANDIDATES so the far side never fans out more
                        // dials/punches than the contract allows.
                        let mut addrs = addrs;
                        crate::shared::sanitize_and_log("broker offer", &mut addrs);
                        candidates.retain(|candidate| addrs.contains(&candidate.addr));
                        candidates.truncate(crate::shared::MAX_UDP_CANDIDATES);
                        info!(%id, candidate_count = addrs.len(), "peer offered udp candidates");
                        match side {
                            BrokerSide::Provider => matchmaker.offer_provider_v2(UdpOffer { addrs, candidates, profile: nat_profile }),
                            BrokerSide::Consumer => matchmaker.offer_consumer_v2(UdpOffer { addrs, candidates, profile: nat_profile }),
                        }
                    }
                    Some(_) => warn!(%id, "unexpected control message"),
                    None => return Ok(()),
                }
            }
        }
    }
}

pub async fn run_server(cfg: &ServerConfig) -> Result<()> {
    let addr = format!("{}:{}", cfg.bind_addr, cfg.control_port);
    let listener = TcpListener::bind(&addr)
        .await
        .context(format!("bind {addr}"))?;
    info!(addr, "rb-transport server listening");

    let registry = Arc::new(DashMap::new());
    let max_conns = Arc::new(Semaphore::new(cfg.max_conns));
    let pending_carriers = Arc::new(DashMap::new());
    let udp_registry = Arc::new(DashMap::new());
    let auth = cfg.secret.as_deref().map(Authenticator::new).transpose()?;
    let tls = match (&cfg.tls_cert, &cfg.tls_key) {
        (None, None) => None,
        (Some(cert), Some(key)) => Some(load_server_tls(cert, key)?),
        _ => anyhow::bail!("--tls-cert and --tls-key must be supplied together"),
    };

    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                tune_tcp(&socket);
                let registry = Arc::clone(&registry);
                let max_conns = Arc::clone(&max_conns);
                let pending_carriers = Arc::clone(&pending_carriers);
                let udp_registry = Arc::clone(&udp_registry);
                let auth_opt = auth.clone();
                let tls = tls.clone();
                tokio::spawn(
                    async move {
                        if let Err(error) = handle_accepted_conn(
                            socket,
                            tls,
                            peer,
                            registry,
                            max_conns,
                            pending_carriers,
                            udp_registry,
                            auth_opt,
                        )
                        .await
                        {
                            warn!(%error, "client handler failed");
                        }
                    }
                    .instrument(info_span!("client", %peer)),
                );
            }
            Err(e) => {
                error!(%e, "accept error");
            }
        }
    }
}

// Connection state is assembled at the accept boundary; grouping it would make
// TLS/plain dispatch less explicit, so this handler keeps the boundary visible.
#[allow(clippy::too_many_arguments)]
async fn handle_accepted_conn(
    socket: TcpStream,
    tls: Option<TlsAcceptor>,
    peer: SocketAddr,
    registry: Registry,
    max_conns: Arc<Semaphore>,
    pending_carriers: PendingCarriers,
    udp_registry: UdpRegistry,
    auth: Option<Authenticator>,
) -> Result<()> {
    if let Some(tls) = tls {
        let stream = tls.accept(socket).await.context("TLS handshake failed")?;
        handle_conn(
            stream,
            peer,
            registry,
            max_conns,
            pending_carriers,
            udp_registry,
            auth,
        )
        .await
    } else {
        handle_conn(
            socket,
            peer,
            registry,
            max_conns,
            pending_carriers,
            udp_registry,
            auth,
        )
        .await
    }
}

async fn handle_conn<S: mux::Transport>(
    socket: S,
    peer: SocketAddr,
    registry: Registry,
    max_conns: Arc<Semaphore>,
    pending_carriers: PendingCarriers,
    udp_registry: UdpRegistry,
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

    // A peer that opens the control substream and then goes silent must not pin
    // this task: the first frame is owed immediately (bore's `recv_timeout`).
    let first = control.recv_client_timeout().await?;

    if let Some(authenticator) = auth {
        authenticator.server_handshake(&mut control).await?;
    }

    match first {
        Some(ClientMsg::Register { channel }) => {
            serve_provider(
                control,
                opener,
                registry,
                udp_registry,
                channel,
                peer,
                pending_carriers,
            )
            .await
        }
        Some(ClientMsg::Connect { channel }) => {
            serve_consumer(
                control,
                acceptor,
                registry,
                udp_registry,
                channel,
                peer,
                max_conns,
            )
            .await
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
    udp_registry: UdpRegistry,
    id: String,
    _peer: SocketAddr,
    _pending_carriers: PendingCarriers,
) -> Result<()> {
    // Claim the channel id ATOMICALLY. A `contains_key` probe followed by an
    // `insert` lets two sources that register at the same moment both pass the
    // probe; the second then replaces the first's pool in the registry, so the
    // consumer's relayed substreams are spliced to the wrong source and the
    // first source's `DropGuard` later removes an entry it no longer owns —
    // failing an in-flight backup with a confusing "source is no longer
    // available".
    let pool = Arc::new(CarrierPool::new(opener));
    match registry.entry(id.clone()) {
        Entry::Occupied(_) => {
            warn!(%id, "channel id already in use");
            control
                .send_server(ServerMsg::Error {
                    reason: "channel already in use".into(),
                })
                .await?;
            return Ok(());
        }
        Entry::Vacant(vacant) => {
            vacant.insert(Arc::clone(&pool));
        }
    }
    // UDP hole-punch brokering + control-plane liveness run in the shared control
    // loop. The matchmaker is shared with the consumer task via the registry
    // (created by whichever side arrives first); the relay path is unaffected.
    // It is taken before the guard so the guard can remove exactly the one this
    // provider used.
    let matchmaker = udp_registry
        .entry(id.clone())
        .or_insert_with(|| Arc::new(UdpMatchmaker::default()))
        .clone();
    // When this returns, `_guard` drops and removes the registry entries — so a
    // reaped/disconnected provider releases its channel id.
    let _guard = DropGuard {
        registry: Arc::clone(&registry),
        udp_registry: Arc::clone(&udp_registry),
        id: id.clone(),
        pool: Arc::clone(&pool),
        matchmaker: Arc::clone(&matchmaker),
    };

    control.send_server(ServerMsg::Ok).await?;
    info!(%id, "provider registered");

    serve_control(
        &mut control,
        &matchmaker,
        BrokerSide::Provider,
        &id,
        SECRET_CTRL_TIMEOUT,
    )
    .await
}

async fn serve_consumer(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    registry: Registry,
    udp_registry: UdpRegistry,
    id: String,
    _peer: SocketAddr,
    max_conns: Arc<Semaphore>,
) -> Result<()> {
    match wait_for_provider(&mut control, &registry, &id, PEER_REGISTRATION_TIMEOUT).await? {
        ProviderWait::Available => {}
        ProviderWait::ClientClosed => return Ok(()),
        ProviderWait::TimedOut => {
            control
                .send_server(ServerMsg::Error {
                    reason: format!("source did not register channel '{id}' within 10 minutes"),
                })
                .await?;
            return Ok(());
        }
    }

    // A channel pairs one source with one destination. Refuse a second
    // destination explicitly instead of letting its relayed substreams
    // interleave with the first one's on the same provider.
    let Some(pool) = registry.get(&id).map(|entry| Arc::clone(entry.value())) else {
        control
            .send_server(ServerMsg::Error {
                reason: format!("source for channel '{id}' is no longer available"),
            })
            .await?;
        return Ok(());
    };
    let Some(_consumer_claim) = pool.claim_consumer() else {
        warn!(%id, "channel already has a destination");
        control
            .send_server(ServerMsg::Error {
                reason: format!("channel '{id}' already has a destination connected"),
            })
            .await?;
        return Ok(());
    };

    control.send_server(ServerMsg::Ok).await?;
    info!(%id, "consumer connected");

    // The shared control loop (heartbeat, UDP broker, reaper) runs alongside the
    // consumer-specific relay-accept loop; whichever ends first ends the task.
    let matchmaker = udp_registry
        .entry(id.clone())
        .or_insert_with(|| Arc::new(UdpMatchmaker::default()))
        .clone();

    tokio::select! {
        r = serve_control(&mut control, &matchmaker, BrokerSide::Consumer, &id, SECRET_CTRL_TIMEOUT) => r,
        r = accept_relays(&mut acceptor, &registry, &id, &max_conns) => r,
    }
}

/// Consumer-specific loop: accept each relayed substream and splice it to a live
/// provider carrier under the `max_conns` semaphore.
async fn accept_relays(
    acceptor: &mut mux::Acceptor,
    registry: &Registry,
    id: &str,
    max_conns: &Arc<Semaphore>,
) -> Result<()> {
    loop {
        let Some(consumer_stream) = acceptor.accept().await else {
            return Ok(());
        };
        let permit = match Arc::clone(max_conns).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(%id, "too many active connections, dropping");
                continue;
            }
        };
        let registry = Arc::clone(registry);
        let id = id.to_string();
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

/// Relay one consumer substream to the provider that was present when the
/// consumer control connection was acknowledged.
async fn relay(consumer: mux::Stream, registry: Registry, id: &str) -> Result<()> {
    relay_with_timeout(consumer, registry, id, STREAM_READY_TIMEOUT).await
}

/// As [`relay`], generic over the consumer stream and with the readiness
/// deadline injected so tests can drive it fast.
async fn relay_with_timeout<S: mux::Transport>(
    mut consumer: S,
    registry: Registry,
    id: &str,
    ready_timeout: Duration,
) -> Result<()> {
    // The readiness marker is owed as soon as the substream opens. This read runs
    // while holding a `max_conns` permit, so a consumer that opens substreams and
    // never writes would otherwise exhaust the relay's real connection bound for
    // every channel on the server.
    let mut marker = [0u8; 1];
    timeout(ready_timeout, consumer.read_exact(&mut marker))
        .await
        .context("timed out waiting for relay stream readiness marker")??;

    let pool = registry
        .get(id)
        .map(|entry| Arc::clone(entry.value()))
        .with_context(|| format!("source for channel '{id}' is no longer available"))?;
    let opener = pool.pick().context("no live provider carrier")?;
    let mut provider = opener.open().await.context("provider unavailable")?;
    provider.write_all(&[mux::STREAM_READY]).await?;

    let buf = proxy_buffer_size();
    tokio::io::copy_bidirectional_with_sizes(&mut consumer, &mut provider, buf, buf).await?;
    Ok(())
}

struct DropGuard {
    registry: Registry,
    udp_registry: UdpRegistry,
    id: String,
    pool: Arc<CarrierPool>,
    matchmaker: Arc<UdpMatchmaker>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Remove only the entries this provider actually installed, so a losing
        // or already-replaced registration can never evict the live source of
        // the same channel id — nor its matchmaker, whose loss would silently
        // split the two peers onto separate matchmakers and drop the whole
        // channel to the relay after the broker timeout.
        self.registry
            .remove_if(&self.id, |_, pool| Arc::ptr_eq(pool, &self.pool));
        self.udp_registry.remove_if(&self.id, |_, matchmaker| {
            Arc::ptr_eq(matchmaker, &self.matchmaker)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test(start_paused = true)]
    async fn destination_waits_beyond_the_old_ten_second_registration_window() {
        let registry = Arc::new(DashMap::new());
        let (server_side, client_side) = duplex(1024);
        let mut control = Delimited::new(server_side);
        let wait_registry = Arc::clone(&registry);
        let waiter = tokio::spawn(async move {
            wait_for_provider(
                &mut control,
                &wait_registry,
                "delayed-source",
                Duration::from_secs(10 * 60),
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(
            !waiter.is_finished(),
            "destination must still wait after the former ten-second cutoff"
        );

        drop(client_side);
        assert_eq!(
            waiter.await.expect("wait task").expect("wait result"),
            ProviderWait::ClientClosed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn destination_provider_wait_has_an_explicit_deadline() {
        let registry = Arc::new(DashMap::new());
        let (server_side, _client_side) = duplex(1024);
        let mut control = Delimited::new(server_side);
        let waiter = tokio::spawn(async move {
            wait_for_provider(
                &mut control,
                &registry,
                "missing-source",
                Duration::from_secs(2),
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            waiter.await.expect("wait task").expect("wait result"),
            ProviderWait::TimedOut
        );
    }

    /// A provider teardown must not take a *newer* channel's UDP matchmaker with
    /// it. Losing it does not corrupt anything, but the two peers then register
    /// on separate matchmakers, never match, and the whole channel drops to the
    /// relay after the broker timeout.
    #[tokio::test]
    async fn a_stale_provider_teardown_keeps_the_live_channel_state() {
        let registry: Registry = Arc::new(DashMap::new());
        let udp_registry: UdpRegistry = Arc::new(DashMap::new());
        let id = "reused-channel";

        let (stale_side, _stale_peer) = duplex(1024);
        let (stale_opener, _stale_acceptor) = mux::server(stale_side);
        let stale_pool = Arc::new(CarrierPool::new(stale_opener));
        let stale_matchmaker = Arc::new(UdpMatchmaker::default());
        registry.insert(id.to_string(), Arc::clone(&stale_pool));
        udp_registry.insert(id.to_string(), Arc::clone(&stale_matchmaker));
        let stale_guard = DropGuard {
            registry: Arc::clone(&registry),
            udp_registry: Arc::clone(&udp_registry),
            id: id.to_string(),
            pool: stale_pool,
            matchmaker: stale_matchmaker,
        };

        // A new provider takes the same channel id over with its own state.
        let (live_side, _live_peer) = duplex(1024);
        let (live_opener, _live_acceptor) = mux::server(live_side);
        let live_pool = Arc::new(CarrierPool::new(live_opener));
        let live_matchmaker = Arc::new(UdpMatchmaker::default());
        registry.insert(id.to_string(), Arc::clone(&live_pool));
        udp_registry.insert(id.to_string(), Arc::clone(&live_matchmaker));

        drop(stale_guard);

        let pool = registry.get(id).expect("the live pool must survive");
        assert!(Arc::ptr_eq(pool.value(), &live_pool));
        let matchmaker = udp_registry
            .get(id)
            .expect("the live matchmaker must survive");
        assert!(Arc::ptr_eq(matchmaker.value(), &live_matchmaker));
    }

    /// A relayed substream that never sends its readiness marker must release
    /// the `max_conns` permit it holds instead of pinning it forever.
    #[tokio::test]
    async fn relay_without_a_readiness_marker_gives_up() {
        // A consumer substream that is opened and then held silent.
        let (_silent_peer, consumer_stream) = duplex(8192);

        let registry: Registry = Arc::new(DashMap::new());
        let error =
            relay_with_timeout(consumer_stream, registry, "chan", Duration::from_millis(50))
                .await
                .expect_err("a silent substream must not pin the relay");
        assert!(
            error.to_string().contains("readiness marker"),
            "unexpected error: {error}"
        );
    }

    /// A peer whose control substream goes silent is reaped once the recv deadline
    /// elapses — even while the server keeps heartbeating it. Drains server→client
    /// so the heartbeat sends never block; the client never sends a frame.
    #[tokio::test(start_paused = true)]
    async fn serve_control_reaps_silent_peer() {
        let (srv, cli) = duplex(8192);
        let matchmaker = Arc::new(UdpMatchmaker::default());
        let mut server = Delimited::new(srv);
        let mm = Arc::clone(&matchmaker);
        let handle = tokio::spawn(async move {
            serve_control(
                &mut server,
                &mm,
                BrokerSide::Provider,
                "chan",
                Duration::from_secs(2),
            )
            .await
        });

        let mut client = Delimited::new(cli);
        let drain =
            tokio::spawn(async move { while let Ok(Some(_)) = client.recv_server().await {} });

        let joined = tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("silent peer must be reaped within the deadline");
        joined.expect("join").expect("serve_control ok");
        drain.abort();
    }

    /// A peer that heartbeats within the deadline is NOT reaped; the loop ends only
    /// once that peer disconnects.
    #[tokio::test(start_paused = true)]
    async fn serve_control_keeps_heartbeating_peer() {
        let (srv, cli) = duplex(8192);
        let matchmaker = Arc::new(UdpMatchmaker::default());
        let mut server = Delimited::new(srv);
        let mm = Arc::clone(&matchmaker);
        let handle = tokio::spawn(async move {
            serve_control(
                &mut server,
                &mm,
                BrokerSide::Consumer,
                "chan",
                Duration::from_secs(2),
            )
            .await
        });

        let mut client = Delimited::new(cli);
        // Heartbeat every 1s (< 2s deadline) for ~6s; the loop must stay alive.
        for _ in 0..6 {
            client
                .send_client(ClientMsg::Heartbeat)
                .await
                .expect("send heartbeat");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert!(
            !handle.is_finished(),
            "heartbeating peer must not be reaped"
        );

        // Stop heartbeating and disconnect; the loop ends on EOF.
        drop(client);
        let joined = tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("loop must end after the peer disconnects");
        joined.expect("join").expect("serve_control ok");
    }
}
