//! Direct UDP/QUIC path — Phase 1 implementation.
//!
//! Ported from bore's holepunch.rs. Provides hole-punching and QUIC connection
//! establishment for peer-to-peer data transfers without a relay.

#![cfg(feature = "udp")]

use anyhow::{bail, Context as _, Result};
use hmac::{Hmac, Mac};
use quinn::rustls;
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
use rcgen;
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

use crate::shared::UdpDirectTuning;

type HmacSha256 = Hmac<Sha256>;

// === Constants ===

/// Authentication token length in bytes.
pub const TOKEN_LEN: usize = 32;

/// ALPN protocol for bore QUIC connections.
const ALPN: &[u8] = b"bore-udp";

/// Keep-alive interval for idle QUIC connections.
const QUIC_KEEPALIVE: Duration = Duration::from_secs(3);

/// Maximum idle time before a QUIC connection is closed, in milliseconds.
/// Expressed in millis so the quinn `IdleTimeout` is built from a `VarInt`
/// (infallible) rather than a fallible `Duration` conversion.
const QUIC_MAX_IDLE_MS: u32 = 10_000;

/// Total timeout for all direct connection attempts.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);

// === Token Helpers ===

/// Derive the shared QUIC authentication token from the tunnel secret (if any)
/// and the server-issued session nonce. Both peers compute the same value.
pub fn derive_token(secret: Option<&str>, nonce: &[u8]) -> Result<[u8; TOKEN_LEN]> {
    let key = secret.map(str::as_bytes).unwrap_or(&[]);
    let mut mac = HmacSha256::new_from_slice(key).context("construct direct-path HMAC")?;
    mac.update(nonce);
    let mut token = [0u8; TOKEN_LEN];
    token.copy_from_slice(&mac.finalize().into_bytes());
    Ok(token)
}

/// Constant-time comparison of two tokens.
fn tokens_match(a: &[u8; TOKEN_LEN], b: &[u8; TOKEN_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..TOKEN_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// === Socket Primitives ===

/// Build a fresh, unbound UDP socket with tuning applied.
fn make_socket() -> Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create UDP socket")?;
    configure_udp_socket_buffers(&socket, &UdpDirectTuning::default());
    socket
        .set_nonblocking(true)
        .context("failed to set UDP socket non-blocking")?;
    Ok(socket)
}

/// Bind a UDP socket for a hole-punch session. `port` 0 picks a random ephemeral
/// port (the default); a fixed port lets a strict *egress* firewall be opened for
/// exactly that port (use the same value on both peers) and makes the public
/// mapping predictable on a port-preserving NAT.
///
/// CRITICAL — NOT `SO_REUSEADDR`: two wildcard UDP sockets that BOTH set
/// `SO_REUSEADDR` co-bind the same port and the kernel delivers inbound datagrams
/// to the *last* binder. Without `SO_REUSEADDR` the second bind is cleanly refused
/// (`EADDRINUSE`) and we fall back to an ephemeral port.
pub async fn bind_socket(port: u16) -> Result<UdpSocket> {
    let socket = make_socket()?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, port).into();
    match socket.bind(&addr.into()) {
        Ok(()) => {
            return UdpSocket::from_std(socket.into())
                .context("failed to register UDP socket with tokio");
        }
        Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            warn!(
                preferred_port = port,
                "fixed UDP port {port} is already in use (another tunnel or process holds it); \
                 falling back to an ephemeral port. Behind a strict egress firewall that only \
                 permits {port}, this tunnel may stay on the relay path."
            );
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "failed to bind fixed UDP port {port} (free? allowed?)"
            )));
        }
    }

    let socket = make_socket()?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, 0).into();
    socket
        .bind(&addr.into())
        .context("failed to bind ephemeral UDP socket after fixed-port fallback")?;
    UdpSocket::from_std(socket.into()).context("failed to register UDP socket with tokio")
}

/// Configure UDP socket buffers for optimal direct-path throughput.
#[cfg(all(feature = "udp", windows))]
fn configure_udp_socket_buffers<S: std::os::windows::io::AsSocket>(
    socket: &S,
    tuning: &UdpDirectTuning,
) {
    let socket = socket2::SockRef::from(socket);
    if let Err(err) = socket.set_recv_buffer_size(tuning.udp_socket_recv_buffer) {
        debug!(%err, requested = tuning.udp_socket_recv_buffer, "failed to raise UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(tuning.udp_socket_send_buffer) {
        debug!(%err, requested = tuning.udp_socket_send_buffer, "failed to raise UDP send buffer");
    }

    debug!(
        requested_recv = tuning.udp_socket_recv_buffer,
        actual_recv = ?socket.recv_buffer_size().ok(),
        requested_send = tuning.udp_socket_send_buffer,
        actual_send = ?socket.send_buffer_size().ok(),
        "configured UDP socket buffers"
    );
}

#[cfg(all(feature = "udp", target_os = "linux"))]
fn configure_udp_socket_buffers<S: std::os::fd::AsFd>(socket: &S, tuning: &UdpDirectTuning) {
    use nix::sys::socket::{getsockopt, setsockopt, sockopt};

    let fd = socket.as_fd();

    let recv_forced = setsockopt(&fd, sockopt::RcvBufForce, &tuning.udp_socket_recv_buffer).is_ok();
    if !recv_forced {
        let _ = setsockopt(&fd, sockopt::RcvBuf, &tuning.udp_socket_recv_buffer);
    }
    let send_forced = setsockopt(&fd, sockopt::SndBufForce, &tuning.udp_socket_send_buffer).is_ok();
    if !send_forced {
        let _ = setsockopt(&fd, sockopt::SndBuf, &tuning.udp_socket_send_buffer);
    }

    let actual_recv = getsockopt(&fd, sockopt::RcvBuf).unwrap_or(0);
    let actual_send = getsockopt(&fd, sockopt::SndBuf).unwrap_or(0);
    let recv_clamped = actual_recv < tuning.udp_socket_recv_buffer;
    let send_clamped = actual_send < tuning.udp_socket_send_buffer;

    if recv_clamped || send_clamped {
        warn!(
            requested_recv = tuning.udp_socket_recv_buffer,
            effective_recv = actual_recv,
            requested_send = tuning.udp_socket_send_buffer,
            effective_send = actual_send,
            recv_forced,
            send_forced,
            "UDP socket buffer clamped below request — direct-path throughput will be \
             limited to roughly buffer/RTT. Run with CAP_NET_ADMIN (privileged) for \
             SO_*BUFFORCE, or raise net.core.rmem_max and net.core.wmem_max"
        );
    } else {
        info!(
            requested_recv = tuning.udp_socket_recv_buffer,
            effective_recv = actual_recv,
            requested_send = tuning.udp_socket_send_buffer,
            effective_send = actual_send,
            forced = recv_forced && send_forced,
            "configured UDP socket buffers"
        );
    }
}

#[cfg(all(feature = "udp", unix, not(target_os = "linux")))]
fn configure_udp_socket_buffers<S: std::os::fd::AsFd>(socket: &S, tuning: &UdpDirectTuning) {
    let socket = socket2::SockRef::from(socket);
    if let Err(err) = socket.set_recv_buffer_size(tuning.udp_socket_recv_buffer) {
        debug!(%err, requested = tuning.udp_socket_recv_buffer, "failed to raise UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(tuning.udp_socket_send_buffer) {
        debug!(%err, requested = tuning.udp_socket_send_buffer, "failed to raise UDP send buffer");
    }

    debug!(
        requested_recv = tuning.udp_socket_recv_buffer,
        actual_recv = ?socket.recv_buffer_size().ok(),
        requested_send = tuning.udp_socket_send_buffer,
        actual_send = ?socket.send_buffer_size().ok(),
        "configured UDP socket buffers"
    );
}

/// Convert a Tokio UDP socket into a nonblocking std socket for quinn.
fn into_std(socket: UdpSocket) -> Result<StdUdpSocket> {
    let socket = socket.into_std().context("failed to detach UDP socket")?;
    socket
        .set_nonblocking(true)
        .context("failed to set socket nonblocking")?;
    Ok(socket)
}

// === Hole-Punching ===

/// Fire warmup datagrams at peer candidates to open NAT mappings.
async fn punch(socket: &UdpSocket, peers: &[SocketAddr]) {
    for _ in 0..5 {
        for peer in peers {
            let _ = socket.send_to(b"bore-punch", peer).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// === QUIC Configuration ===

/// Build a QUIC transport config with optimized flow-control windows and BBR.
fn transport_config(tuning: &UdpDirectTuning) -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    cfg.keep_alive_interval(Some(QUIC_KEEPALIVE));
    // Built from a VarInt (millis) instead of `Duration::try_into` so this hot
    // path carries no `expect` (I-ERRORS): the conversion cannot fail.
    cfg.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(
        QUIC_MAX_IDLE_MS,
    ))));

    cfg.stream_receive_window(tuning.stream_receive_window.into());
    cfg.receive_window(tuning.connection_receive_window.into());
    cfg.send_window(tuning.send_window);

    cfg.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));

    cfg.max_concurrent_bidi_streams(tuning.max_direct_streams.into());

    cfg.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    cfg.datagram_send_buffer_size(8 * 1024 * 1024);

    cfg
}

/// QUIC client config: accept any server certificate (the token handshake, not
/// the certificate, authenticates the peer).
fn client_config(tuning: &UdpDirectTuning) -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .context("failed to configure QUIC TLS")?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(SkipVerify))
    .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .context("invalid QUIC client crypto")?;
    let mut config = ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// QUIC server config with a self-signed certificate.
fn server_config(tuning: &UdpDirectTuning) -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["bore".to_string()])
        .context("failed to generate self-signed certificate")?;
    let cert_der = cert.cert.der().clone();
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .context("failed to configure QUIC TLS")?
    .with_no_client_auth()
    .with_single_cert(
        vec![cert_der],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
    )
    .context("invalid QUIC server certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("invalid QUIC server crypto")?;
    let mut config = ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// A certificate verifier that accepts any server certificate. Safe here because
/// the peer is authenticated by the shared token, not by its certificate.
#[derive(Debug)]
struct SkipVerify;

impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a QUIC client endpoint over a UDP socket.
fn client_endpoint(socket: UdpSocket, tuning: &UdpDirectTuning) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC client endpoint")?;
    endpoint.set_default_client_config(client_config(tuning)?);
    Ok(endpoint)
}

/// Build a QUIC server endpoint over an already-bound UDP socket.
fn server_endpoint(socket: UdpSocket, tuning: &UdpDirectTuning) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config(tuning)?),
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC server endpoint")?;
    endpoint.set_default_client_config(client_config(tuning)?);
    Ok(endpoint)
}

// === Connection Types ===

/// Outcome of a best-effort datagram send on the direct QUIC path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramSend {
    /// Queued for transmission.
    Sent,
    /// Dropped: larger than the current path-MTU datagram limit.
    TooLarge,
}

/// One native QUIC bidirectional stream wrapped as an `AsyncRead`/`AsyncWrite`
/// carrier for a single proxied connection.
pub struct QuicTransport {
    recv: quinn::RecvStream,
    send: quinn::SendStream,
    _conn: Connection,
    _endpoint: Endpoint,
}

impl AsyncRead for QuicTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for QuicTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

/// An authenticated direct QUIC connection between a consumer and a provider.
/// Proxied connections are carried over native QUIC streams (one bidi each).
#[derive(Clone)]
pub struct DirectConn {
    conn: Connection,
    endpoint: Endpoint,
}

impl DirectConn {
    /// Open a new native QUIC bidi stream for one proxied connection.
    pub async fn open_stream(&self) -> Result<QuicTransport> {
        let (send, recv) = self.conn.open_bi().await.context("open_bi failed")?;
        Ok(QuicTransport {
            recv,
            send,
            _conn: self.conn.clone(),
            _endpoint: self.endpoint.clone(),
        })
    }

    /// Accept the next native QUIC bidi stream for one proxied connection.
    pub async fn accept_stream(&self) -> Result<QuicTransport> {
        let (send, recv) = self.conn.accept_bi().await.context("accept_bi failed")?;
        Ok(QuicTransport {
            recv,
            send,
            _conn: self.conn.clone(),
            _endpoint: self.endpoint.clone(),
        })
    }

    /// Resolve when the QUIC connection closes.
    pub async fn closed(&self) {
        self.conn.closed().await;
    }

    /// Gracefully close the QUIC connection.
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"direct path closed");
    }

    /// Snapshot the current QUIC connection statistics.
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.conn.stats()
    }

    /// Snapshot the current path MTU-dependent datagram size.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    /// Send an IP packet as a QUIC unreliable datagram (non-blocking).
    pub fn send_datagram(&self, pkt: bytes::Bytes) -> Result<DatagramSend> {
        match self.conn.send_datagram(pkt) {
            Ok(()) => Ok(DatagramSend::Sent),
            Err(quinn::SendDatagramError::TooLarge) => Ok(DatagramSend::TooLarge),
            Err(e) => Err(anyhow::anyhow!("send_datagram: {e}")),
        }
    }

    /// Send an IP packet as a QUIC datagram, awaiting send-buffer room.
    pub async fn send_datagram_wait(&self, pkt: bytes::Bytes) -> Result<DatagramSend> {
        match self.conn.send_datagram_wait(pkt).await {
            Ok(()) => Ok(DatagramSend::Sent),
            Err(quinn::SendDatagramError::TooLarge) => Ok(DatagramSend::TooLarge),
            Err(e) => Err(anyhow::anyhow!("send_datagram_wait: {e}")),
        }
    }

    /// Read the next QUIC datagram.
    pub async fn read_datagram(&self) -> Result<bytes::Bytes> {
        self.conn.read_datagram().await.context("read_datagram")
    }

    /// The connection's resolved remote address.
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Open an additional authenticated QUIC connection to the same peer over
    /// the same endpoint/socket.
    pub async fn open_sibling(&self, token: [u8; TOKEN_LEN]) -> Result<DirectConn> {
        let peer = self.conn.remote_address();
        let conn = self
            .endpoint
            .connect(peer, "bore")
            .context("failed to start direct carrier connect")?
            .await
            .context("direct carrier QUIC handshake failed")?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .context("carrier auth open_bi failed")?;
        send.write_all(&token).await?;
        send.flush().await?;
        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            bail!("direct carrier token mismatch");
        }
        let _ = send.finish();
        debug!(%peer, "direct carrier connection established (consumer, token verified)");
        Ok(DirectConn {
            conn,
            endpoint: self.endpoint.clone(),
        })
    }
}

/// A long-lived QUIC server endpoint that accepts direct connections from punched consumers.
pub struct DirectListener {
    endpoint: Endpoint,
}

impl DirectListener {
    /// Punch toward `peers` and start a QUIC server endpoint over `socket`.
    pub async fn new(
        socket: UdpSocket,
        peers: Vec<SocketAddr>,
        tuning: UdpDirectTuning,
    ) -> Result<Self> {
        configure_udp_socket_buffers(&socket, &tuning);
        let local_addr = socket.local_addr().ok();
        info!(
            udp_local_addr = ?local_addr,
            peer_candidates = ?peers,
            "provider punching UDP peer candidates and starting QUIC listener"
        );
        punch(&socket, &peers).await;
        let endpoint = server_endpoint(socket, &tuning)?;
        Ok(DirectListener { endpoint })
    }

    /// Gracefully close the endpoint and all its connections.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"provider shutting down");
    }

    /// Re-open this endpoint's NAT mapping toward reconnecting consumers.
    pub fn punch_via_endpoint(&self, peers: &[SocketAddr]) {
        info!(peer_candidates = ?peers, "provider re-punching UDP peer candidates");
        for &peer in peers {
            if let Ok(connecting) = self.endpoint.connect(peer, "bore") {
                tokio::spawn(async move {
                    let _ = timeout(NETWORK_TIMEOUT, connecting).await;
                });
            }
        }
    }

    /// Accept the next direct connection and authenticate it with `token`.
    pub async fn accept(&self, token: [u8; TOKEN_LEN]) -> Result<DirectConn> {
        loop {
            let conn = self
                .endpoint
                .accept()
                .await
                .context("endpoint accept failed")?;
            let conn = match timeout(NETWORK_TIMEOUT, conn).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    debug!("QUIC handshake failed: {e}");
                    continue;
                }
                Err(_) => {
                    debug!("QUIC handshake timed out");
                    continue;
                }
            };
            trace!("QUIC accepted");
            match timeout(
                NETWORK_TIMEOUT,
                Self::auth_accept(&conn, &self.endpoint, token),
            )
            .await
            {
                Ok(Ok(dc)) => return Ok(dc),
                Ok(Err(e)) => {
                    debug!("auth accept failed: {e}");
                    continue;
                }
                Err(_) => {
                    debug!("auth accept timed out");
                    continue;
                }
            }
        }
    }

    /// Authenticate an inbound connection by reading and verifying the peer's token.
    async fn auth_accept(
        conn: &Connection,
        endpoint: &Endpoint,
        token: [u8; TOKEN_LEN],
    ) -> Result<DirectConn> {
        let (mut send, mut recv) = conn.accept_bi().await.context("auth accept_bi failed")?;
        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            warn!("direct QUIC candidate failed token verification");
            bail!("token mismatch");
        }
        send.write_all(&token).await?;
        send.flush().await?;
        let _ = send.finish();
        info!(target_addr = %conn.remote_address(),
            "direct udp connection established (provider, token verified)");
        let dc = DirectConn {
            conn: conn.clone(),
            endpoint: endpoint.clone(),
        };
        debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (provider)");
        Ok(dc)
    }
}

// === Connection Establishment ===

/// Consumer side: punch toward `peers`, connect a QUIC client over `socket`,
/// and authenticate the connection with `token` on a dedicated stream.
pub async fn connect_direct(
    socket: UdpSocket,
    peers: Vec<SocketAddr>,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn> {
    if peers.is_empty() {
        bail!("no peer candidates to connect to");
    }
    configure_udp_socket_buffers(&socket, &tuning);
    let local_addr = socket.local_addr().ok();
    info!(
        udp_local_addr = ?local_addr,
        peer_candidates = ?peers,
        "consumer punching UDP peer candidates"
    );
    punch(&socket, &peers).await;
    let endpoint = client_endpoint(socket, &tuning)?;

    let errors: Arc<std::sync::Mutex<Vec<(SocketAddr, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let attempts: Vec<_> = peers
        .iter()
        .map(|&peer| {
            let endpoint = endpoint.clone();
            let errors = Arc::clone(&errors);
            Box::pin(async move {
                debug!(%peer, "attempting direct QUIC candidate");
                let connecting = match endpoint.connect(peer, "bore") {
                    Ok(connecting) => connecting,
                    Err(err) => {
                        let msg = format!("start failed: {err}");
                        debug!(%peer, %err, "failed to start direct QUIC candidate");
                        errors
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((peer, msg));
                        return Err(err.into());
                    }
                };
                let conn = match connecting.await {
                    Ok(conn) => conn,
                    Err(err) => {
                        let msg = format!("{err}");
                        debug!(%peer, %err, "direct QUIC candidate failed");
                        errors
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((peer, msg));
                        return Err(err.into());
                    }
                };
                trace!(%peer, "QUIC connected");
                let (mut send, mut recv) = conn.open_bi().await.context("auth open_bi failed")?;
                send.write_all(&token).await?;
                send.flush().await?;
                let mut peer_token = [0u8; TOKEN_LEN];
                recv.read_exact(&mut peer_token).await?;
                if !tokens_match(&token, &peer_token) {
                    let msg = "token mismatch".to_string();
                    warn!(%peer, "direct QUIC candidate failed token verification");
                    errors
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((peer, msg));
                    bail!("direct path token mismatch");
                }
                let _ = send.finish();
                info!(target_addr = %peer, peer = %conn.remote_address(),
                    "direct udp connection established (consumer, token verified)");
                let dc = DirectConn {
                    conn,
                    endpoint,
                };
                debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (consumer)");
                anyhow::Ok(dc)
            })
        })
        .collect();

    match timeout(NETWORK_TIMEOUT, futures_util::future::select_ok(attempts)).await {
        Ok(Ok((conn, _losers))) => Ok(conn),
        Ok(Err(err)) => {
            let err_summary: Vec<String> = errors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .map(|(addr, msg)| format!("{addr} → {msg}"))
                .collect();
            warn!(
                candidates = ?peers,
                errors = ?err_summary,
                "all {n} direct QUIC candidates failed; falling back to relay",
                n = peers.len(),
            );
            Err(err).context("all direct candidates failed")
        }
        Err(_) => {
            warn!(
                timeout = ?NETWORK_TIMEOUT,
                candidates = ?peers,
                "direct QUIC connect exhausted {NETWORK_TIMEOUT:?} budget \
                 across {n} candidates; none responded — all candidates timed out \
                 (firewall/UDP blocked on both ends, or peer IP unreachable). \
                 Falling back to relay",
                n = peers.len(),
            );
            bail!("direct connect exhausted the {NETWORK_TIMEOUT:?} budget")
        }
    }
}

// === Tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    // Two QUIC endpoints establish a direct connection over loopback and exchange
    // a bidi stream (Phase 1.1 Done-criteria). `bind_socket` binds the wildcard
    // address (correct for real hole-punching), so the consumer must dial the
    // listener's port on explicit loopback — quinn rejects `0.0.0.0` as a target.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_loopback_handshake() {
        let tuning = UdpDirectTuning::default();

        let listener_socket = bind_socket(0).await.expect("bind listener socket");
        let listener_port = listener_socket
            .local_addr()
            .expect("listener local_addr")
            .port();
        let connect_addr =
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, listener_port));

        let listener = DirectListener::new(listener_socket, vec![], tuning)
            .await
            .expect("create listener");

        let token = [42u8; TOKEN_LEN];

        let consumer_socket = bind_socket(0).await.expect("bind consumer socket");
        let consumer_task = tokio::spawn(async move {
            connect_direct(consumer_socket, vec![connect_addr], token, tuning)
                .await
                .expect("connect_direct")
        });

        let provider_task =
            tokio::spawn(async move { listener.accept(token).await.expect("accept") });

        let provider_conn = provider_task.await.expect("provider join");
        let consumer_conn = consumer_task.await.expect("consumer join");

        let provider_stream = tokio::spawn({
            let conn = provider_conn.clone();
            async move { conn.accept_stream().await.expect("provider accept_stream") }
        });

        let mut consumer_stream = consumer_conn
            .open_stream()
            .await
            .expect("consumer open_stream");

        let test_byte = 42u8;
        futures_util::future::join(
            async {
                AsyncWriteExt::write_all(&mut consumer_stream, &[test_byte])
                    .await
                    .expect("consumer write");
                AsyncWriteExt::flush(&mut consumer_stream)
                    .await
                    .expect("consumer flush");
            },
            async {
                let mut provider_stream = provider_stream.await.expect("provider join");
                let mut buf = [0u8; 1];
                AsyncReadExt::read_exact(&mut provider_stream, &mut buf)
                    .await
                    .expect("provider read");
                assert_eq!(buf[0], test_byte, "loopback byte mismatch");
            },
        )
        .await;
    }

    #[test]
    fn test_transport_config_asserts() {
        let tuning = UdpDirectTuning::default();
        let _cfg = transport_config(&tuning);
        // transport_config returns a config with BBR enabled, 16 MiB flow-control windows
        // and 100 max concurrent streams. We can't easily inspect these in quinn 0.11,
        // so we just verify the function doesn't panic.
        assert_eq!(tuning.max_direct_streams, 100, "max_direct_streams");
        assert_eq!(
            tuning.stream_receive_window,
            16 * 1024 * 1024,
            "stream window"
        );
        assert_eq!(
            tuning.connection_receive_window,
            16 * 1024 * 1024,
            "connection window"
        );
    }

    #[tokio::test]
    async fn test_bind_socket_ephemeral_fallback() {
        // Own a real port via an ephemeral bind (race-free, no hardcoded port),
        // then re-bind that exact port. Because the punch socket sets no
        // SO_REUSEADDR, the second bind MUST hit EADDRINUSE and fall back to a
        // different ephemeral port (BUG-S3: co-bound REUSEADDR sockets flap).
        let socket1 = bind_socket(0).await.expect("first (ephemeral) bind");
        let port = socket1.local_addr().expect("local_addr").port();
        assert_ne!(port, 0, "ephemeral bind must yield a concrete port");

        let socket2 = bind_socket(port)
            .await
            .expect("second bind must succeed via ephemeral fallback");
        let fallback_port = socket2.local_addr().expect("local_addr").port();

        assert_ne!(
            fallback_port, port,
            "second bind of a held port must fall back to a different ephemeral port \
             (no SO_REUSEADDR co-bind)"
        );
    }

    /// Phase 1.4: `bind_socket` applies `configure_udp_socket_buffers`, so a tuned
    /// socket's receive buffer is getsockopt-verified to be no smaller than an
    /// untuned default socket's (robust across kernels — the exact size is clamped
    /// by `net.core.rmem_max` without CAP_NET_ADMIN, but never shrinks).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_socket_buffers_enlarged() {
        use nix::sys::socket::{getsockopt, sockopt};

        let plain = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("plain bind");
        let plain_recv = getsockopt(&plain, sockopt::RcvBuf).unwrap_or(0);

        let tuned = bind_socket(0).await.expect("tuned bind");
        let tuned_recv = getsockopt(&tuned, sockopt::RcvBuf).unwrap_or(0);

        assert!(tuned_recv > 0, "tuned recv buffer must be set");
        assert!(
            tuned_recv >= plain_recv,
            "tuned recv buffer {tuned_recv} must be >= default {plain_recv}"
        );
    }
}
