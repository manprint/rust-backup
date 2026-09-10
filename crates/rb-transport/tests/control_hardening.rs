//! Regressions for the coordination server's control-plane hardening: bounded
//! control frames, handshake deadlines, and one-source/one-destination channel
//! ownership.

use std::time::Duration;

use rb_core::channel::DataChannel;
use rb_core::config::{ServerConfig, TransportConfig};
use rb_transport::proto::{ClientMsg, Delimited, ServerMsg, MAX_FRAME_LENGTH};
use tokio::io::AsyncWriteExt;

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

async fn start_server(port: u16) {
    start_server_with_max_conns(port, 256).await
}

async fn start_server_with_max_conns(port: u16, max_conns: usize) {
    let cfg = ServerConfig {
        bind_addr: "127.0.0.1".to_string(),
        control_port: port,
        secret: None,
        tls_cert: None,
        tls_key: None,
        max_conns,
        udp: false,
    };
    tokio::spawn(async move {
        let _ = rb_transport::run_server(&cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
}

fn transport(port: u16, channel: &str) -> TransportConfig {
    TransportConfig {
        to: format!("127.0.0.1:{port}"),
        channel: channel.to_string(),
        secret: None,
        carriers: 1,
        udp: false,
        insecure: true,
        max_rate: None,
    }
}

/// A peer that streams bytes and never sends the null delimiter must be refused
/// once the frame bound is reached, not allowed to grow the buffer without limit.
#[tokio::test]
async fn control_frame_without_a_delimiter_is_refused() {
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    let mut framed: Delimited<_> = Delimited::new(reader);

    let flood = tokio::spawn(async move {
        let chunk = vec![b'x'; 4096];
        // Far more than the bound; the reader must give up rather than buffer it.
        for _ in 0..64 {
            if writer.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });

    let error = framed
        .recv_client()
        .await
        .expect_err("an undelimited frame must be refused");
    assert!(
        error.to_string().contains("MAX_FRAME_LENGTH"),
        "unexpected error: {error}"
    );
    flood.abort();
}

/// A frame the peer owes immediately has a deadline; the untimed form does not.
#[tokio::test(start_paused = true)]
async fn handshake_reads_have_a_deadline() {
    let (_peer, silent) = tokio::io::duplex(1024);
    let mut framed: Delimited<_> = Delimited::new(silent);

    let waiter = tokio::spawn(async move { framed.recv_client_timeout().await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(11)).await;

    let error = waiter
        .await
        .expect("handshake task")
        .expect_err("a silent peer must not pin the handshake");
    assert!(
        error.to_string().contains("timed out"),
        "unexpected error: {error}"
    );
}

/// The worst case the coordination server itself emits — a full IPv6 candidate
/// set forwarded to the peer — must fit inside the enforced frame bound.
#[test]
fn worst_case_forwarded_punch_fits_the_frame_bound() {
    let addrs: Vec<std::net::SocketAddr> = (0..rb_transport::shared::MAX_UDP_CANDIDATES)
        .map(|i| {
            format!("[2001:0db8:85a3:1111:2222:8a2e:0370:{i:04x}]:65535")
                .parse()
                .expect("ipv6 candidate literal")
        })
        .collect();
    let msg = ServerMsg::UdpPunch {
        peer_addrs: addrs.clone(),
        peer_candidates: addrs
            .iter()
            .map(|addr| rb_transport::proto::UdpCandidate {
                addr: *addr,
                kind: rb_transport::proto::UdpCandidateKind::Reflexive,
                priority: u16::MAX,
            })
            .collect(),
        peer_profile: Default::default(),
    };
    let encoded = serde_json::to_vec(&msg).expect("encode punch");
    assert!(
        encoded.len() < MAX_FRAME_LENGTH,
        "forwarded punch of {} bytes must stay under MAX_FRAME_LENGTH ({MAX_FRAME_LENGTH})",
        encoded.len()
    );
}

/// A channel pairs one source with one destination. A second destination is
/// refused with an explicit reason instead of interleaving its substreams with
/// the first destination's on the same source.
#[tokio::test]
async fn a_second_destination_on_one_channel_is_refused() {
    let port = free_port();
    start_server(port).await;
    let cfg = transport(port, "exclusive-channel");

    let source = rb_transport::connect_source(&cfg)
        .await
        .expect("source connect");
    let first = rb_transport::connect_destination(&cfg)
        .await
        .expect("first destination connect");

    let error = match rb_transport::connect_destination(&cfg).await {
        Ok(_) => panic!("a second destination must be refused"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("already has a destination"),
        "unexpected error: {error}"
    );

    // The first pairing is untouched and still usable.
    let mut opened = first.open_stream().await.expect("first destination opens");
    let mut accepted = source.accept_stream().await.expect("source accepts");
    opened.write_all(b"still paired").await.expect("write");
    opened.shutdown().await.expect("shutdown");
    let mut got = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut accepted, &mut got)
        .await
        .expect("read");
    assert_eq!(&got, b"still paired");

    // Once the first destination is gone the channel accepts a replacement.
    drop(first);
    drop(accepted);
    tokio::time::sleep(Duration::from_millis(200)).await;
    rb_transport::connect_destination(&cfg)
        .await
        .expect("replacement destination connect");
}

/// Two sources registering the same channel id at the same moment must leave
/// exactly one registration standing, and the loser must not evict the winner.
#[tokio::test]
async fn concurrent_sources_leave_one_usable_registration() {
    let port = free_port();
    start_server(port).await;
    let cfg = transport(port, "contended-channel");

    let first_cfg = transport(port, "contended-channel");
    let second_cfg = transport(port, "contended-channel");
    let (first, second) = tokio::join!(
        rb_transport::connect_source(&first_cfg),
        rb_transport::connect_source(&second_cfg)
    );

    // `connect_source` returns once the server acknowledged the registration, so
    // the rejected side surfaces the refusal as a Connect-phase error.
    let winner = match (first, second) {
        (Ok(channel), Err(error)) | (Err(error), Ok(channel)) => {
            assert!(
                error.to_string().contains("already in use"),
                "unexpected rejection: {error}"
            );
            channel
        }
        (Ok(_), Ok(_)) => panic!("both sources registered the same channel id"),
        (Err(a), Err(b)) => panic!("both sources were rejected: {a} / {b}"),
    };

    // The surviving registration still relays, i.e. the loser did not remove it.
    let destination = rb_transport::connect_destination(&cfg)
        .await
        .expect("destination connect");
    let mut opened = destination.open_stream().await.expect("open");
    let mut accepted = winner.accept_stream().await.expect("accept");
    opened.write_all(b"winner stands").await.expect("write");
    opened.shutdown().await.expect("shutdown");
    let mut got = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut accepted, &mut got)
        .await
        .expect("read");
    assert_eq!(&got, b"winner stands");
}

/// The client control socket carries the whole multiplexed relay data plane, so
/// it must be tuned exactly like the server's accepted socket.
#[tokio::test]
async fn client_control_socket_is_tuned() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accept = tokio::spawn(async move { listener.accept().await.map(|(stream, _)| stream) });

    let stream = rb_transport::transport::connect_with_timeout("127.0.0.1", addr.port())
        .await
        .expect("connect");
    assert!(stream.nodelay().expect("read nodelay"));
    let _accepted = accept.await.expect("accept task").expect("accepted");
}

/// A TLS endpoint that completes the TCP handshake and then stalls must fail with
/// a handshake deadline rather than hanging the caller.
#[tokio::test]
async fn tls_handshake_has_a_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stall = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        // Hold the connection open without ever speaking TLS.
        std::future::pending::<()>().await;
        drop(stream);
    });

    let endpoint = rb_transport::transport::Endpoint {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        tls: true,
    };
    let error = match tokio::time::timeout(
        Duration::from_secs(30),
        rb_transport::transport::connect(&endpoint, true),
    )
    .await
    .expect("TLS connect must not hang")
    {
        Ok(_) => panic!("a stalled TLS peer must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("timed out during TLS handshake"),
        "unexpected error: {error}"
    );
    stall.abort();
}

/// `ClientMsg`/`ServerMsg` are only reachable through the codec, so keep the
/// unused-import lint honest about the handshake types used above.
#[test]
fn control_message_types_are_public() {
    let _ = ClientMsg::Heartbeat;
    let _ = ServerMsg::Ok;
}

/// `--max-conns` must bound the accepted control connections, not only the
/// relayed substreams inside one of them. Before this, the accept loop spawned a
/// task and a yamux session for every peer that could reach the port, and a
/// `Register` then claimed a registry entry that outlived the handshake — none
/// of it capped by the flag operators are told is "the real bound".
#[tokio::test]
async fn max_conns_bounds_the_accepted_control_connections() {
    let port = free_port();
    start_server_with_max_conns(port, 1).await;

    let hog = rb_transport::connect_source(&transport(port, "hog"))
        .await
        .expect("the first source takes the only permit");

    let blocked = tokio::time::timeout(
        Duration::from_millis(750),
        rb_transport::connect_source(&transport(port, "second")),
    )
    .await;
    assert!(
        blocked.is_err(),
        "the server served a second connection past --max-conns 1"
    );

    // The permit is held for the connection's whole life, so it comes back only
    // when that connection ends.
    drop(hog);
    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if rb_transport::connect_source(&transport(port, "second"))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        recovered.is_ok(),
        "the connection permit was never returned to the pool"
    );
}
