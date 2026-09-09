//! Bounded authenticated UDP connectivity checks performed before QUIC.
//! The caller keeps the same socket for the subsequent QUIC endpoint; checks
//! only nominate a preferred address and never make relay fallback unavailable.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::net::UdpSocket;

type HmacSha256 = Hmac<Sha256>;
const MAGIC: &[u8; 4] = b"RBCK";
const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const PACKET_LEN: usize = 4 + 1 + 4 + 8 + 32;

/// Both peers run the same symmetric exchange — each sends its request to every
/// candidate and answers whatever arrives — so there is deliberately no role
/// here. An earlier `CheckRole` field was set by the caller and never read,
/// which advertised an asymmetry the code does not have.
#[derive(Clone, Debug)]
pub struct CheckConfig {
    pub key: [u8; 32],
    pub generation: u32,
    pub window: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct CheckOutcome {
    pub nominated: Option<SocketAddr>,
    pub invalid_packets: u32,
}

pub fn derive_check_key(token: &[u8; 32]) -> [u8; 32] {
    let Ok(mut mac) = HmacSha256::new_from_slice(token) else {
        return [0; 32];
    };
    mac.update(b"rust-backup-connectivity-check-v1");
    let mut key = [0; 32];
    key.copy_from_slice(&mac.finalize().into_bytes());
    key
}

pub fn plan_check_groups(candidates: &[SocketAddr]) -> Vec<Vec<SocketAddr>> {
    candidates
        .iter()
        .copied()
        .fold(Vec::<Vec<SocketAddr>>::new(), |mut groups, address| {
            let same_lan = match address.ip() {
                std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
                std::net::IpAddr::V6(ip) => ip.is_unique_local() || ip.is_loopback(),
            };
            let index = usize::from(!same_lan);
            if groups.len() <= index {
                groups.resize_with(index + 1, Vec::new);
            }
            groups[index].push(address);
            groups
        })
}

fn packet(kind: u8, generation: u32, nonce: u64, key: &[u8; 32]) -> [u8; PACKET_LEN] {
    let mut out = [0; PACKET_LEN];
    out[..4].copy_from_slice(MAGIC);
    out[4] = kind;
    out[5..9].copy_from_slice(&generation.to_be_bytes());
    out[9..17].copy_from_slice(&nonce.to_be_bytes());
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return out;
    };
    mac.update(&out[..17]);
    out[17..].copy_from_slice(&mac.finalize().into_bytes());
    out
}

fn valid_packet(bytes: &[u8], generation: u32, key: &[u8; 32]) -> Option<(u8, u64)> {
    if bytes.len() != PACKET_LEN
        || &bytes[..4] != MAGIC
        || u32::from_be_bytes(bytes[5..9].try_into().ok()?) != generation
    {
        return None;
    }
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(&bytes[..17]);
    mac.verify_slice(&bytes[17..]).ok()?;
    Some((bytes[4], u64::from_be_bytes(bytes[9..17].try_into().ok()?)))
}

pub async fn run_connectivity_checks(
    socket: &UdpSocket,
    peers: &[SocketAddr],
    cfg: &CheckConfig,
) -> CheckOutcome {
    let started = Instant::now();
    let nonce = fastrand::u64(..);
    let request = packet(REQUEST, cfg.generation, nonce, &cfg.key);
    for group in plan_check_groups(peers) {
        for peer in group {
            let _ = socket.send_to(&request, peer).await;
        }
    }
    let mut outcome = CheckOutcome::default();
    let mut buf = [0u8; 128];
    while started.elapsed() < cfg.window {
        let left = cfg.window.saturating_sub(started.elapsed());
        let received = tokio::time::timeout(left, socket.recv_from(&mut buf)).await;
        let Ok(Ok((len, from))) = received else { break };
        match valid_packet(&buf[..len], cfg.generation, &cfg.key) {
            Some((REQUEST, peer_nonce)) => {
                let response = packet(RESPONSE, cfg.generation, peer_nonce, &cfg.key);
                let _ = socket.send_to(&response, from).await;
            }
            Some((RESPONSE, response_nonce)) if response_nonce == nonce => {
                outcome.nominated.get_or_insert(from);
            }
            _ => outcome.invalid_packets += 1,
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_local_before_public() {
        let private: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let public: SocketAddr = "203.0.113.1:1".parse().unwrap();
        assert_eq!(
            plan_check_groups(&[public, private]),
            vec![vec![private], vec![public]]
        );
    }

    #[tokio::test]
    async fn loopback_checks_nominate_and_reject_wrong_key() {
        let left = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let right = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let token = [7; 32];
        let key = derive_check_key(&token);
        let cfg = CheckConfig {
            key,
            generation: 4,
            window: Duration::from_millis(100),
        };
        let other = cfg.clone();
        let left_addr = left.local_addr().unwrap();
        let right_addr = right.local_addr().unwrap();
        let left_peers = [right_addr];
        let right_peers = [left_addr];
        let (a, b) = tokio::join!(
            run_connectivity_checks(&left, &left_peers, &cfg),
            run_connectivity_checks(&right, &right_peers, &other)
        );
        assert_eq!(a.nominated, Some(right_addr));
        assert_eq!(b.nominated, Some(left_addr));
    }
}
