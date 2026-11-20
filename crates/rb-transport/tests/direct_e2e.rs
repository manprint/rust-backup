//! Phase 1.3 e2e: the full client direct-path wiring activates over loopback.
//!
//! Both peers gather candidates, offer them on the transport control stream, the
//! server broker exchanges them, and a direct QUIC connection is established —
//! then carries data through the `DataChannel` API. This exercises the REAL
//! `connect_source`/`connect_destination` direct path (not `set_direct` directly).
#![cfg(feature = "udp")]

use rb_core::channel::DataChannel;
use rb_core::config::{ServerConfig, TransportConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Bring both peers up — the destination `destination_delay` after the source —
/// and require the stream between them to ride the direct path, or the relay
/// when the destination does not negotiate one.
async fn round_trip(channel: &str, destination_delay: std::time::Duration, destination_udp: bool) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let server_cfg = ServerConfig {
        bind_addr: "127.0.0.1".to_string(),
        control_port: port,
        secret: None,
        tls_cert: None,
        tls_key: None,
        max_conns: 256,
        udp: true,
    };
    tokio::spawn(async move {
        let _ = rb_transport::run_server(&server_cfg).await;
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    let mk = || TransportConfig {
        to: addr.clone(),
        channel: channel.to_string(),
        secret: None,
        carriers: 1,
        udp: true,
        insecure: true,
        max_rate: None,
    };
    let src_cfg = mk();
    let dst_cfg = TransportConfig {
        udp: destination_udp,
        ..mk()
    };

    let payload: &[u8] = b"end-to-end-direct-quic";
    let source = tokio::spawn(async move {
        let channel = rb_transport::connect_source(&src_cfg)
            .await
            .expect("source connect");
        let mut s = channel.accept_stream().await.expect("accept");
        let mut buf = vec![0u8; payload.len()];
        s.read_exact(&mut buf).await.expect("read");
        (buf, channel.is_direct().await)
    });

    tokio::time::sleep(destination_delay).await;
    let dest_channel = rb_transport::connect_destination(&dst_cfg)
        .await
        .expect("dest connect");
    assert_eq!(
        dest_channel.is_direct().await,
        destination_udp,
        "destination path (direct = {destination_udp})"
    );
    let mut out = dest_channel.open_stream().await.expect("open");
    out.write_all(payload).await.expect("write");
    out.flush().await.expect("flush");

    let (got, source_direct) = tokio::time::timeout(std::time::Duration::from_secs(10), source)
        .await
        .expect("direct exchange timed out")
        .expect("source join");
    assert_eq!(
        source_direct, destination_udp,
        "source path (direct = {destination_udp})"
    );
    assert_eq!(got.as_slice(), payload, "payload must round-trip");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_path_activates_over_loopback() {
    round_trip("direct-e2e", std::time::Duration::ZERO, true).await;
}

/// The source is normally started first and left waiting. It used to give up
/// on the direct path after the 10 s broker window, so any destination arriving
/// later was served through the relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_that_waited_past_the_broker_window_still_goes_direct() {
    round_trip("direct-e2e-late", std::time::Duration::from_secs(13), true).await;
}

/// A destination that does not negotiate a direct path opens on the relay
/// straight away; the source, still negotiating, must take that stream instead
/// of holding out for a direct path that is not coming.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_only_destination_is_served_while_the_source_still_negotiates() {
    round_trip("direct-e2e-relay", std::time::Duration::from_secs(1), false).await;
}
