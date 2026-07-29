//! Phase 1.2 acceptance tests for the UDP hole-punch broker.
//!
//! Two layers:
//!  - proto serde round-trips for the new control-message variants;
//!  - **behavioral** tests that drive the real [`UdpMatchmaker`] decision logic
//!    (the broker's brain) directly — both offer orderings, the timeout path, and
//!    the no-false-timeout case. These exercise actual broker code, not a
//!    hand-built message echoed back.

use std::net::SocketAddr;
use std::time::Duration;

use rb_transport::proto::{
    ClientMsg, ServerMsg, UdpCandidate, UdpCandidateKind, MAX_FRAME_LENGTH, MAX_V2_OFFER_CANDIDATES,
};
use rb_transport::server::UdpMatchmaker;

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("valid socket addr")
}

// --- proto serde round-trips --------------------------------------------------

fn roundtrip_client(msg: &ClientMsg) -> ClientMsg {
    let json = serde_json::to_vec(msg).expect("serialize");
    serde_json::from_slice(&json).expect("deserialize")
}

fn roundtrip_server(msg: &ServerMsg) -> ServerMsg {
    let json = serde_json::to_vec(msg).expect("serialize");
    serde_json::from_slice(&json).expect("deserialize")
}

#[test]
fn udp_candidate_offer_round_trip() {
    let msg = ClientMsg::UdpCandidateOffer {
        addrs: vec![addr("203.0.113.7:7000"), addr("[2001:db8::1]:7000")],
        candidates: vec![],
        generation: 0,
        nat_profile: Default::default(),
    };
    assert_eq!(roundtrip_client(&msg), msg);
}

#[test]
fn udp_candidate_offer_empty_round_trip() {
    let msg = ClientMsg::UdpCandidateOffer {
        addrs: vec![],
        candidates: vec![],
        generation: 0,
        nat_profile: Default::default(),
    };
    assert_eq!(roundtrip_client(&msg), msg);
}

#[test]
fn udp_offer_v1_deserializes_into_v2_defaults() {
    let old = br#"{"type":"udpcandidateoffer","addrs":["203.0.113.7:7000"]}"#;
    let parsed: ClientMsg = serde_json::from_slice(old).expect("v1 frame must remain readable");
    assert_eq!(
        parsed,
        ClientMsg::UdpCandidateOffer {
            addrs: vec![addr("203.0.113.7:7000")],
            candidates: vec![],
            generation: 0,
            nat_profile: Default::default(),
        }
    );
}

#[test]
fn worst_case_v2_offer_stays_inside_control_frame_limit() {
    let msg = ClientMsg::UdpCandidateOffer {
        addrs: (0..MAX_V2_OFFER_CANDIDATES)
            .map(|port| addr(&format!("203.0.113.7:{}", 7000 + port)))
            .collect(),
        candidates: (0..MAX_V2_OFFER_CANDIDATES)
            .map(|port| UdpCandidate {
                addr: addr(&format!("203.0.113.7:{}", 7000 + port)),
                kind: UdpCandidateKind::Reflexive,
                priority: u16::MAX - port as u16,
            })
            .collect(),
        generation: u32::MAX,
        nat_profile: Default::default(),
    };
    assert!(serde_json::to_vec(&msg).unwrap().len() <= MAX_FRAME_LENGTH);
}

#[test]
fn udp_punch_round_trip() {
    let msg = ServerMsg::UdpPunch {
        peer_addrs: vec![addr("198.51.100.4:9000"), addr("[2001:db8::2]:9000")],
        peer_candidates: vec![],
        peer_profile: Default::default(),
    };
    assert_eq!(roundtrip_server(&msg), msg);
}

#[test]
fn udp_unavailable_round_trip() {
    let msg = ServerMsg::UdpUnavailable;
    assert_eq!(roundtrip_server(&msg), msg);
}

// --- broker behavior: both peers exchange the OTHER side's candidates ---------

/// Provider offers first, then consumer. Both receivers must resolve with the
/// peer's addresses.
#[tokio::test]
async fn broker_happy_path_provider_first() {
    let m = UdpMatchmaker::default();
    let prov = vec![addr("10.0.0.1:1111")];
    let cons = vec![addr("10.0.0.2:2222")];

    let prx = m.register_provider();
    let crx = m.register_consumer();

    m.offer_provider(prov.clone());
    m.offer_consumer(cons.clone());

    assert_eq!(prx.await.expect("provider notified"), cons);
    assert_eq!(crx.await.expect("consumer notified"), prov);
}

/// Consumer offers first, then provider — the ordering the broken broker failed.
/// Both receivers must still resolve with the peer's addresses.
#[tokio::test]
async fn broker_happy_path_consumer_first() {
    let m = UdpMatchmaker::default();
    let prov = vec![addr("10.0.0.3:3333")];
    let cons = vec![addr("10.0.0.4:4444")];

    // Register order also reversed to stress order-independence.
    let crx = m.register_consumer();
    let prx = m.register_provider();

    m.offer_consumer(cons.clone());
    m.offer_provider(prov.clone());

    assert_eq!(crx.await.expect("consumer notified"), prov);
    assert_eq!(prx.await.expect("provider notified"), cons);
}

/// A side whose peer never offers must NOT be matched; in the real serve loop its
/// receiver stays pending and the broker deadline fires `UdpUnavailable`. Modeled
/// here with the same `select!` the server uses, under virtual time.
#[tokio::test(start_paused = true)]
async fn broker_timeout_when_peer_never_offers() {
    let m = UdpMatchmaker::default();
    let _prx = m.register_provider();
    let mut crx = m.register_consumer();

    // Provider offers; consumer never does.
    m.offer_provider(vec![addr("10.0.0.5:5555")]);

    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    let timed_out = tokio::select! {
        _ = &mut crx => false,
        _ = &mut deadline => true,
    };
    assert!(
        timed_out,
        "unmatched consumer must stay pending so the deadline fires UdpUnavailable"
    );
}

/// The mirror of the above: a matched side resolves BEFORE the deadline, so it
/// never falsely emits `UdpUnavailable`.
#[tokio::test(start_paused = true)]
async fn broker_matched_side_beats_deadline() {
    let m = UdpMatchmaker::default();
    let prov = vec![addr("10.0.0.6:6666")];
    let cons = vec![addr("10.0.0.7:7777")];

    let _prx = m.register_provider();
    let mut crx = m.register_consumer();
    m.offer_provider(prov.clone());
    m.offer_consumer(cons.clone());

    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    let got = tokio::select! {
        r = &mut crx => Some(r.expect("consumer notified")),
        _ = &mut deadline => None,
    };
    assert_eq!(
        got,
        Some(prov),
        "matched consumer must resolve before deadline"
    );
}
