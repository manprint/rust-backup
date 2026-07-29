//! Integration test: source and destination over a relay.

use rb_core::channel::DataChannel;
use rb_core::config::TransportConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn relay_roundtrip() {
    let port = 9999;
    let addr = format!("127.0.0.1:{port}");

    let cfg = rb_core::config::ServerConfig {
        bind_addr: "127.0.0.1".to_string(),
        control_port: port as u16,
        secret: None,
        tls_cert: None,
        tls_key: None,
        max_conns: 256,
        udp: false,
    };

    tokio::spawn(async move {
        let _ = rb_transport::run_server(&cfg).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let transport = TransportConfig {
        to: addr.clone(),
        channel: "test-channel".to_string(),
        secret: None,
        carriers: 1,
        udp: false,
        insecure: true,
        max_rate: None,
    };

    let source_channel = rb_transport::connect_source(&transport)
        .await
        .expect("source connect failed");

    let dest_channel = rb_transport::connect_destination(&transport)
        .await
        .expect("destination connect failed");

    let mut stream1 = dest_channel
        .open_stream()
        .await
        .expect("open stream failed");
    let mut stream2 = source_channel
        .accept_stream()
        .await
        .expect("accept stream failed");

    stream1
        .write_all(b"hello from dest")
        .await
        .expect("write failed");
    stream1.shutdown().await.expect("shutdown failed");

    let mut buf = Vec::new();
    stream2.read_to_end(&mut buf).await.expect("read failed");
    assert_eq!(&buf, b"hello from dest");

    stream2
        .write_all(b"hello from source")
        .await
        .expect("write failed");
    stream2.shutdown().await.expect("shutdown failed");

    buf.clear();
    stream1.read_to_end(&mut buf).await.expect("read failed");
    assert_eq!(&buf, b"hello from source");
}
