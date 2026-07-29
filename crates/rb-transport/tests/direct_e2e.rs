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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_path_activates_over_loopback() {
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
        channel: "direct-e2e".to_string(),
        secret: None,
        carriers: 1,
        udp: true,
        insecure: true,
        max_rate: None,
    };
    let src_cfg = mk();
    let dst_cfg = mk();

    // Connect both sides CONCURRENTLY so both offer candidates within the broker
    // window — sequential connects would each time out and fall back to relay.
    let src_task = tokio::spawn(async move { rb_transport::connect_source(&src_cfg).await });
    let dst_task = tokio::spawn(async move { rb_transport::connect_destination(&dst_cfg).await });

    let source_channel = src_task
        .await
        .expect("source join")
        .expect("source connect");
    let dest_channel = dst_task.await.expect("dest join").expect("dest connect");

    assert!(
        source_channel.is_direct().await,
        "source must have activated the direct QUIC path"
    );
    assert!(
        dest_channel.is_direct().await,
        "destination must have activated the direct QUIC path"
    );

    // Data must flow over the direct path through the channel API.
    let payload: &[u8] = b"end-to-end-direct-quic";
    let accept = tokio::spawn(async move {
        let mut s = source_channel.accept_stream().await.expect("accept");
        let mut buf = vec![0u8; payload.len()];
        s.read_exact(&mut buf).await.expect("read");
        buf
    });
    let mut out = dest_channel.open_stream().await.expect("open");
    out.write_all(payload).await.expect("write");
    out.flush().await.expect("flush");

    let got = tokio::time::timeout(std::time::Duration::from_secs(10), accept)
        .await
        .expect("direct exchange timed out")
        .expect("accept join");
    assert_eq!(
        got.as_slice(),
        payload,
        "payload must round-trip over direct QUIC"
    );
}
