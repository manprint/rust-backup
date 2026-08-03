//! Client for rb-transport.

use rb_core::config::TransportConfig;
use rb_core::error::BackupError;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Duration;
use tracing::{debug, warn};

use crate::auth::Authenticator;
use crate::channel::{PairedChannel, CTRL_CLIENT_HEARTBEAT};
#[cfg(feature = "udp")]
use crate::connectivity::{derive_check_key, run_connectivity_checks, CheckConfig, CheckRole};
use crate::mux;
use crate::proto::{
    ClientMsg, Delimited, ServerMsg, UdpCandidate, UdpCandidateKind, MAX_V2_OFFER_CANDIDATES,
};
use crate::transport;

#[cfg(feature = "udp")]
use crate::direct::{bind_socket, connect_direct, derive_token, DirectConn, DirectListener};
#[cfg(feature = "udp")]
use crate::shared::UdpDirectTuning;

#[cfg(feature = "udp")]
const DIRECT_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Which side of the direct handshake this peer plays.
#[cfg(feature = "udp")]
#[derive(Clone, Copy)]
enum DirectRole {
    /// Source: listens for the QUIC connection (accepts data substreams).
    Provider,
    /// Destination: dials the QUIC connection (opens data substreams).
    Consumer,
}

/// Wait for Register/Connect acknowledgement while keeping the control stream
/// active through reverse proxies. A destination can legitimately wait several
/// minutes for its source to be started.
async fn await_registration<S>(control: &mut Delimited<S>) -> rb_core::error::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut heartbeat = tokio::time::interval(CTRL_CLIENT_HEARTBEAT);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                control.send_client(ClientMsg::Heartbeat).await.map_err(|error| {
                    BackupError::phase(
                        rb_core::error::Phase::Connect,
                        format!("send registration heartbeat: {error}"),
                    )
                })?;
            }
            response = control.recv_server() => {
                match response.map_err(|error| {
                    BackupError::phase(
                        rb_core::error::Phase::Connect,
                        format!("receive registration response: {error}"),
                    )
                })? {
                    Some(ServerMsg::Ok) => return Ok(()),
                    Some(ServerMsg::Heartbeat) => {}
                    Some(ServerMsg::Error { reason }) => {
                        return Err(BackupError::phase(
                            rb_core::error::Phase::Connect,
                            format!("server error: {reason}"),
                        ));
                    }
                    Some(ServerMsg::Challenge { .. }) => {
                        return Err(BackupError::phase(
                            rb_core::error::Phase::Connect,
                            "server requires a secret, but none was provided",
                        ));
                    }
                    other => {
                        return Err(BackupError::phase(
                            rb_core::error::Phase::Connect,
                            format!("unexpected registration response: {other:?}"),
                        ));
                    }
                }
            }
        }
    }
}

pub async fn connect_source(cfg: &TransportConfig) -> rb_core::error::Result<PairedChannel> {
    let endpoint = transport::Endpoint::parse(&cfg.to);
    let socket = transport::connect(&endpoint, cfg.insecure)
        .await
        .map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("transport: {e}"))
        })?;

    let (opener, acceptor) = mux::client(socket);
    let ctrl_stream = opener.open().await.map_err(|e| {
        BackupError::phase(rb_core::error::Phase::Connect, format!("open control: {e}"))
    })?;
    let mut control = Delimited::new(ctrl_stream);

    // Send Register FIRST to flush the lazy yamux SYN, THEN authenticate (the
    // server reads Register, then speaks the auth challenge).
    control
        .send_client(ClientMsg::Register {
            channel: cfg.channel.clone(),
        })
        .await
        .map_err(|e| {
            BackupError::phase(
                rb_core::error::Phase::Connect,
                format!("send register: {e}"),
            )
        })?;

    if let Some(secret) = &cfg.secret {
        let auth = Authenticator::new(secret)?;
        auth.client_handshake(&mut control).await.map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("auth: {e}"))
        })?;
    }

    await_registration(&mut control).await?;
    // Negotiate the direct path on the transport control stream BEFORE it is
    // moved into the channel (best-effort; falls back to relay on any failure).
    #[cfg(feature = "udp")]
    let direct = if cfg.udp {
        setup_direct(
            &mut control,
            DirectRole::Provider,
            &cfg.channel,
            cfg.secret.as_deref(),
        )
        .await
    } else {
        None
    };
    let channel = PairedChannel::source_with_carriers(acceptor, control, cfg.carriers);
    #[cfg(feature = "udp")]
    if let Some((dc, token)) = direct {
        channel.set_direct_with_token(dc, token).await;
    }
    Ok(channel)
}

pub async fn connect_destination(cfg: &TransportConfig) -> rb_core::error::Result<PairedChannel> {
    let endpoint = transport::Endpoint::parse(&cfg.to);
    let socket = transport::connect(&endpoint, cfg.insecure)
        .await
        .map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("transport: {e}"))
        })?;

    // Consumer also OPENS the control substream and sends Connect first (flush
    // SYN), mirroring the provider. The unused inbound acceptor is dropped: the
    // server never opens substreams toward the consumer (it relays the consumer's
    // opens to the provider).
    let (opener, _acceptor) = mux::client(socket);
    let ctrl_stream = opener.open().await.map_err(|e| {
        BackupError::phase(rb_core::error::Phase::Connect, format!("open control: {e}"))
    })?;
    let mut control = Delimited::new(ctrl_stream);

    control
        .send_client(ClientMsg::Connect {
            channel: cfg.channel.clone(),
        })
        .await
        .map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("send connect: {e}"))
        })?;

    if let Some(secret) = &cfg.secret {
        let auth = Authenticator::new(secret)?;
        auth.client_handshake(&mut control).await.map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("auth: {e}"))
        })?;
    }

    await_registration(&mut control).await?;
    #[cfg(feature = "udp")]
    let direct = if cfg.udp {
        setup_direct(
            &mut control,
            DirectRole::Consumer,
            &cfg.channel,
            cfg.secret.as_deref(),
        )
        .await
    } else {
        None
    };
    let channel = PairedChannel::destination_with_carriers(opener, control, cfg.carriers);
    #[cfg(feature = "udp")]
    if let Some((dc, token)) = direct {
        channel.set_direct_with_token(dc, token).await;
    }
    Ok(channel)
}

// --- Direct (QUIC hole-punch) path setup --------------------------------------

/// Negotiate and establish the direct QUIC path on the transport control stream.
/// Best-effort: returns `None` (→ relay) on any failure or timeout. Must be
/// called BEFORE `control` is moved into the channel; the module's plan exchange
/// runs on a separate data substream, so the UDP-broker messages consumed here do
/// not disturb it.
#[cfg(feature = "udp")]
async fn setup_direct(
    control: &mut Delimited<mux::Stream>,
    role: DirectRole,
    channel_id: &str,
    secret: Option<&str>,
) -> Option<(DirectConn, [u8; crate::direct::TOKEN_LEN])> {
    match tokio::time::timeout(
        DIRECT_SETUP_TIMEOUT,
        setup_direct_inner(control, role, channel_id, secret),
    )
    .await
    {
        Ok(Ok((dc, token))) => {
            debug!("direct QUIC path established");
            Some((dc, token))
        }
        Ok(Err(e)) => {
            crate::pair_cache::invalidate(channel_id);
            warn!("direct path unavailable, using relay: {e:#}");
            None
        }
        Err(_) => {
            crate::pair_cache::invalidate(channel_id);
            warn!("direct path setup timed out, using relay");
            None
        }
    }
}

#[cfg(feature = "udp")]
async fn setup_direct_inner(
    control: &mut Delimited<mux::Stream>,
    role: DirectRole,
    channel_id: &str,
    secret: Option<&str>,
) -> anyhow::Result<(DirectConn, [u8; crate::direct::TOKEN_LEN])> {
    let tuning = UdpDirectTuning::default();
    // One socket is used for both candidate gathering and QUIC, so any STUN
    // reflexive mapping stays valid for the hole-punched connection.
    let socket = bind_socket(0).await?;
    let mut gathered = gather_candidates(&socket).await?;
    crate::shared::sanitize_and_log("local offer", &mut gathered.candidates);
    gathered.candidates.truncate(MAX_V2_OFFER_CANDIDATES);
    if gathered.candidates.is_empty() {
        anyhow::bail!("no usable local candidates");
    }
    control
        .send_client(ClientMsg::UdpCandidateOffer {
            candidates: gathered
                .candidates
                .iter()
                .enumerate()
                .map(|(index, addr)| UdpCandidate {
                    addr: *addr,
                    kind: if gathered.reflexive_addrs.contains(addr) {
                        UdpCandidateKind::Reflexive
                    } else {
                        UdpCandidateKind::Host
                    },
                    priority: u16::MAX.saturating_sub(index as u16),
                })
                .collect(),
            addrs: gathered.candidates,
            generation: 0,
            nat_profile: crate::adaptive_nat::NatProfile {
                mapping: Some(crate::adaptive_nat::classify_nat(&gathered.reflexive_addrs)),
                reflexive_addrs: gathered.reflexive_addrs,
            },
        })
        .await
        .map_err(|e| anyhow::anyhow!("send candidate offer: {e}"))?;

    let mut peer_addrs = recv_punch(control).await?;
    // Defense in depth: the broker already sanitizes, but the punch/dial entry
    // point never trusts a peer-controlled list either.
    crate::shared::sanitize_and_log("peer punch", &mut peer_addrs);
    if let Some(cached) = crate::pair_cache::recall(channel_id) {
        if peer_addrs.contains(&cached) {
            peer_addrs.retain(|address| *address != cached);
            peer_addrs.insert(0, cached);
        }
    }
    if peer_addrs.is_empty() {
        anyhow::bail!("no peer candidates");
    }
    // Both sides derive the same token from the shared secret + channel id.
    let token = derive_token(secret, channel_id.as_bytes())?;
    let check_role = match role {
        DirectRole::Provider => CheckRole::Listener,
        DirectRole::Consumer => CheckRole::Dialer,
    };
    let checks = run_connectivity_checks(
        &socket,
        &peer_addrs,
        &CheckConfig {
            key: derive_check_key(&token),
            generation: 0,
            role: check_role,
            window: Duration::from_millis(500),
        },
    )
    .await;
    if let Some(nominated) = checks.nominated {
        peer_addrs.retain(|address| *address != nominated);
        peer_addrs.insert(0, nominated);
    }

    let direct = match role {
        DirectRole::Provider => {
            let listener = DirectListener::new(socket, peer_addrs, tuning).await?;
            listener.accept(token).await?
        }
        DirectRole::Consumer => connect_direct(socket, peer_addrs, token, tuning).await?,
    };
    crate::pair_cache::remember(channel_id, direct.remote_address());
    Ok((direct, token))
}

/// Await the server's hole-punch decision, tolerating interleaved heartbeats.
#[cfg(feature = "udp")]
async fn recv_punch(control: &mut Delimited<mux::Stream>) -> anyhow::Result<Vec<SocketAddr>> {
    loop {
        match control
            .recv_server()
            .await
            .map_err(|e| anyhow::anyhow!("recv punch: {e}"))?
        {
            Some(ServerMsg::UdpPunch { peer_addrs, .. }) => return Ok(peer_addrs),
            Some(ServerMsg::UdpUnavailable) => anyhow::bail!("server reports udp unavailable"),
            Some(ServerMsg::Heartbeat) => continue,
            Some(other) => anyhow::bail!("unexpected control message during punch: {other:?}"),
            None => anyhow::bail!("control closed during punch"),
        }
    }
}

/// Gather hole-punch candidate addresses: the local bound address, plus a
/// best-effort STUN reflexive address when `RUST_BACKUP_STUN_SERVER` is set
/// (failure is non-fatal — local candidates suffice on the same network).
#[cfg(feature = "udp")]
struct GatheredCandidates {
    candidates: Vec<SocketAddr>,
    reflexive_addrs: Vec<SocketAddr>,
}

#[cfg(feature = "udp")]
async fn gather_candidates(socket: &tokio::net::UdpSocket) -> anyhow::Result<GatheredCandidates> {
    use std::net::{IpAddr, Ipv4Addr};

    let local = socket.local_addr()?;
    let port = local.port();
    let mut candidates = Vec::new();
    let mut reflexive_addrs = Vec::new();
    if local.ip().is_unspecified() {
        // A wildcard bind (`0.0.0.0`/`::`) is not a routable candidate — the peer
        // cannot dial it (quinn rejects it). Offer the loopback (same-host) and
        // the primary outbound interface address (LAN) instead.
        candidates.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
        if let Some(ip) = primary_local_ip() {
            let a = SocketAddr::new(ip, port);
            if !candidates.contains(&a) {
                candidates.push(a);
            }
        }
    } else {
        candidates.push(local);
    }
    for stun in stun_targets() {
        match discover_reflexive(socket, &stun).await {
            Ok(addr) if !candidates.contains(&addr) => {
                candidates.push(addr);
                reflexive_addrs.push(addr);
            }
            Ok(addr) if !reflexive_addrs.contains(&addr) => reflexive_addrs.push(addr),
            Ok(_) => {}
            Err(e) => warn!("STUN reflexive discovery failed ({stun}): {e:#}"),
        }
    }
    Ok(GatheredCandidates {
        candidates,
        reflexive_addrs,
    })
}

/// Ordered best-effort STUN chain. Operators may replace it with a comma-separated
/// `RUST_BACKUP_STUN_SERVERS`; the legacy singular variable remains supported.
#[cfg(feature = "udp")]
#[cfg(feature = "udp")]
fn stun_targets() -> Vec<String> {
    let configured = std::env::var("RUST_BACKUP_STUN_SERVERS")
        .ok()
        .or_else(|| std::env::var("RUST_BACKUP_STUN_SERVER").ok())
        .unwrap_or_else(|| "stun.l.google.com:19302,stun.cloudflare.com:3478".into());
    configured
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .take(4)
        .collect()
}

/// Best-effort primary outbound interface IP, learned by `connect`ing a throwaway
/// UDP socket to a public address (no packets are sent — `connect` only sets the
/// kernel's route selection, so `local_addr` then reflects the chosen source IP).
#[cfg(feature = "udp")]
fn primary_local_ip() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}

/// Real RFC 5389 STUN Binding Request through `socket`; returns the parsed
/// XOR-MAPPED-ADDRESS. Best-effort NAT-traversal helper.
#[cfg(feature = "udp")]
async fn discover_reflexive(
    socket: &tokio::net::UdpSocket,
    stun_server: &str,
) -> anyhow::Result<SocketAddr> {
    let server = tokio::net::lookup_host(stun_server)
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("STUN server did not resolve"))?;

    let txid = stun_txid();
    let req = build_stun_request(&txid);
    socket.send_to(&req, server).await?;

    let mut buf = [0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("STUN response timeout"))?
        .map(|(n, _)| n)?;
    parse_xor_mapped_address(&buf[..n], &txid)
}

#[cfg(feature = "udp")]
const STUN_MAGIC: u32 = 0x2112_A442;

#[cfg(feature = "udp")]
fn stun_txid() -> [u8; 12] {
    // A fresh socket does one synchronous request/response, so a non-random txid
    // is sufficient here (we still validate the response message type).
    let mut txid = [0u8; 12];
    for (i, b) in txid.iter_mut().enumerate() {
        *b = (0x21u8).wrapping_add(i as u8);
    }
    txid
}

#[cfg(feature = "udp")]
fn build_stun_request(txid: &[u8; 12]) -> Vec<u8> {
    let mut req = Vec::with_capacity(20);
    req.extend_from_slice(&0x0001u16.to_be_bytes()); // Binding Request
    req.extend_from_slice(&0u16.to_be_bytes()); // length
    req.extend_from_slice(&STUN_MAGIC.to_be_bytes());
    req.extend_from_slice(txid);
    req
}

/// Parse the XOR-MAPPED-ADDRESS (0x0020) attribute from a STUN success response.
#[cfg(feature = "udp")]
fn parse_xor_mapped_address(msg: &[u8], txid: &[u8; 12]) -> anyhow::Result<SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    if msg.len() < 20 || u16::from_be_bytes([msg[0], msg[1]]) != 0x0101 {
        anyhow::bail!("not a STUN binding success response");
    }
    let mut i = 20;
    while i + 4 <= msg.len() {
        let attr = u16::from_be_bytes([msg[i], msg[i + 1]]);
        let len = u16::from_be_bytes([msg[i + 2], msg[i + 3]]) as usize;
        let val = &msg[i + 4..];
        if attr == 0x0020 && val.len() >= 4 {
            let family = val[1];
            let xport = u16::from_be_bytes([val[2], val[3]]) ^ (STUN_MAGIC >> 16) as u16;
            return match family {
                0x01 if val.len() >= 8 => {
                    let raw = u32::from_be_bytes([val[4], val[5], val[6], val[7]]) ^ STUN_MAGIC;
                    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(raw)), xport))
                }
                0x02 if val.len() >= 20 => {
                    let mut key = [0u8; 16];
                    key[..4].copy_from_slice(&STUN_MAGIC.to_be_bytes());
                    key[4..].copy_from_slice(txid);
                    let mut ip = [0u8; 16];
                    for j in 0..16 {
                        ip[j] = val[4 + j] ^ key[j];
                    }
                    Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), xport))
                }
                _ => anyhow::bail!("unsupported STUN address family"),
            };
        }
        i += 4 + len.div_ceil(4) * 4; // attributes are 32-bit aligned
    }
    anyhow::bail!("no XOR-MAPPED-ADDRESS in STUN response");
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test(start_paused = true)]
    async fn pending_registration_heartbeats_until_server_is_ready() {
        let (client_side, server_side) = duplex(1024);
        let mut client = Delimited::new(client_side);
        let mut server = Delimited::new(server_side);
        let waiter = tokio::spawn(async move { await_registration(&mut client).await });

        tokio::task::yield_now().await;
        tokio::time::advance(CTRL_CLIENT_HEARTBEAT).await;
        assert_eq!(
            server.recv_client().await.expect("receive heartbeat"),
            Some(ClientMsg::Heartbeat)
        );
        server
            .send_server(ServerMsg::Ok)
            .await
            .expect("send registration ok");
        waiter
            .await
            .expect("registration task")
            .expect("registration succeeds");
    }
}

#[cfg(all(test, feature = "udp"))]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn stun_chain_is_bounded_and_ignores_empty_targets() {
        let parsed: Vec<_> = " one:1, ,two:2,three:3,four:4,five:5 "
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .take(4)
            .collect();
        assert_eq!(parsed, ["one:1", "two:2", "three:3", "four:4"]);
    }

    /// Build a STUN success response carrying an IPv4 XOR-MAPPED-ADDRESS and prove
    /// the parser recovers the original address (real STUN decode, no network).
    #[test]
    fn parse_xor_mapped_ipv4() {
        let txid = stun_txid();
        let real = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)), 51234);

        let xport = real.port() ^ (STUN_MAGIC >> 16) as u16;
        let xip = match real.ip() {
            IpAddr::V4(v4) => u32::from(v4) ^ STUN_MAGIC,
            _ => unreachable!(),
        };

        let mut msg = Vec::new();
        msg.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success
        msg.extend_from_slice(&12u16.to_be_bytes()); // attrs length
        msg.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        msg.extend_from_slice(&txid);
        // XOR-MAPPED-ADDRESS attribute
        msg.extend_from_slice(&0x0020u16.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0);
        msg.push(0x01); // IPv4
        msg.extend_from_slice(&xport.to_be_bytes());
        msg.extend_from_slice(&xip.to_be_bytes());

        let parsed = parse_xor_mapped_address(&msg, &txid).expect("parse");
        assert_eq!(parsed, real);
    }

    #[test]
    fn parse_rejects_non_success() {
        let txid = stun_txid();
        let mut msg = vec![0u8; 20];
        msg[0] = 0x00;
        msg[1] = 0x01; // Binding Request, not success
        assert!(parse_xor_mapped_address(&msg, &txid).is_err());
    }
}
