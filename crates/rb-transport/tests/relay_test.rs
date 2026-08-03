//! Integration test: source and destination over a relay.

use rb_core::channel::DataChannel;
use rb_core::config::TransportConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

#[tokio::test]
async fn relay_roundtrip() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let cfg = rb_core::config::ServerConfig {
        bind_addr: "127.0.0.1".to_string(),
        control_port: port,
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

#[tokio::test]
async fn authenticated_relay_accepts_matching_and_rejects_invalid_secrets() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = rb_core::config::ServerConfig {
        bind_addr: "127.0.0.1".to_string(),
        control_port: port,
        secret: Some("shared-secret".into()),
        tls_cert: None,
        tls_key: None,
        max_conns: 256,
        udp: false,
    };
    let server = tokio::spawn(async move { rb_transport::run_server(&cfg).await });
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let transport = TransportConfig {
        to: addr.clone(),
        channel: "authenticated-relay".into(),
        secret: Some("shared-secret".into()),
        carriers: 1,
        udp: false,
        insecure: true,
        max_rate: None,
    };
    let source = rb_transport::connect_source(&transport)
        .await
        .expect("authenticated source");
    let destination = rb_transport::connect_destination(&transport)
        .await
        .expect("authenticated destination");

    let mut destination_stream = destination.open_stream().await.expect("open stream");
    let mut source_stream = source.accept_stream().await.expect("accept stream");
    destination_stream
        .write_all(b"authenticated relay")
        .await
        .expect("write payload");
    destination_stream
        .shutdown()
        .await
        .expect("shutdown payload");
    let mut received = Vec::new();
    source_stream
        .read_to_end(&mut received)
        .await
        .expect("read payload");
    assert_eq!(received, b"authenticated relay");

    let wrong_secret = TransportConfig {
        channel: "wrong-secret".into(),
        secret: Some("wrong-secret".into()),
        ..transport.clone()
    };
    let wrong_error = match rb_transport::connect_source(&wrong_secret).await {
        Ok(_) => panic!("wrong secret must be rejected"),
        Err(error) => error,
    };
    assert!(wrong_error.to_string().contains("authentication failed"));

    let missing_secret = TransportConfig {
        channel: "missing-secret".into(),
        secret: None,
        ..transport
    };
    let missing_error = match rb_transport::connect_source(&missing_secret).await {
        Ok(_) => panic!("missing secret must be rejected"),
        Err(error) => error,
    };
    assert!(missing_error
        .to_string()
        .contains("server requires a secret"));
    server.abort();
}

#[tokio::test]
async fn destination_can_arrive_before_provider_registration() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = rb_core::config::ServerConfig {
        bind_addr: "127.0.0.1".to_string(),
        control_port: port,
        secret: None,
        tls_cert: None,
        tls_key: None,
        max_conns: 256,
        udp: false,
    };
    let server = tokio::spawn(async move { rb_transport::run_server(&cfg).await });
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    let transport = TransportConfig {
        to: addr,
        channel: "consumer-first".into(),
        secret: None,
        carriers: 1,
        udp: false,
        insecure: true,
        max_rate: None,
    };

    let destination_transport = transport.clone();
    let destination_task =
        tokio::spawn(
            async move { rb_transport::connect_destination(&destination_transport).await },
        );
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert!(
        !destination_task.is_finished(),
        "destination waits for provider registration"
    );
    let source = rb_transport::connect_source(&transport)
        .await
        .expect("provider registers later");
    let destination = tokio::time::timeout(tokio::time::Duration::from_secs(2), destination_task)
        .await
        .expect("destination registration must be bounded")
        .expect("destination join")
        .expect("destination connects after provider");
    let mut destination_stream = destination
        .open_stream()
        .await
        .expect("consumer opens stream");
    let mut source_stream =
        tokio::time::timeout(tokio::time::Duration::from_secs(2), source.accept_stream())
            .await
            .expect("provider accept must be bounded")
            .expect("provider accepts waiting consumer stream");

    destination_stream
        .write_all(b"consumer-first")
        .await
        .unwrap();
    destination_stream.shutdown().await.unwrap();
    let mut received = Vec::new();
    source_stream.read_to_end(&mut received).await.unwrap();
    assert_eq!(received, b"consumer-first");
    server.abort();
}
