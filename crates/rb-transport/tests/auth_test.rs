//! Authentication protocol and handshake regression tests.

use std::time::Duration;

use rb_transport::auth::Authenticator;
use rb_transport::proto::{ClientMsg, Delimited, ServerMsg};
use tokio::io::duplex;
use tokio::time::timeout;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn authentication_frames_round_trip() {
    let challenge =
        Uuid::parse_str("12345678-1234-5678-1234-567812345678").expect("fixed challenge UUID");
    let client = ClientMsg::Authenticate {
        tag: "deadbeef".into(),
    };
    let server = ServerMsg::Challenge { challenge };
    let error = ServerMsg::Error {
        reason: "authentication failed".into(),
    };

    let client_json = serde_json::to_vec(&client).expect("serialize authenticate");
    let server_json = serde_json::to_vec(&server).expect("serialize challenge");
    let error_json = serde_json::to_vec(&error).expect("serialize error");

    assert_eq!(
        serde_json::from_slice::<ClientMsg>(&client_json).expect("deserialize authenticate"),
        client
    );
    assert_eq!(
        serde_json::from_slice::<ServerMsg>(&server_json).expect("deserialize challenge"),
        server
    );
    assert_eq!(
        serde_json::from_slice::<ServerMsg>(&error_json).expect("deserialize error"),
        error
    );
}

#[tokio::test]
async fn matching_secret_completes_handshake() {
    let (server_io, client_io) = duplex(4096);
    let server_auth = Authenticator::new("shared-secret").expect("server authenticator");
    let client_auth = Authenticator::new("shared-secret").expect("client authenticator");
    let server = tokio::spawn(async move {
        let mut stream = Delimited::new(server_io);
        server_auth.server_handshake(&mut stream).await
    });
    let mut client = Delimited::new(client_io);

    timeout(TEST_TIMEOUT, client_auth.client_handshake(&mut client))
        .await
        .expect("client handshake timed out")
        .expect("client handshake");
    timeout(TEST_TIMEOUT, server)
        .await
        .expect("server handshake timed out")
        .expect("server task")
        .expect("server handshake");
}

#[tokio::test]
async fn wrong_secret_is_rejected_with_protocol_error() {
    let (server_io, client_io) = duplex(4096);
    let server_auth = Authenticator::new("expected-secret").expect("server authenticator");
    let client_auth = Authenticator::new("wrong-secret").expect("client authenticator");
    let server = tokio::spawn(async move {
        let mut stream = Delimited::new(server_io);
        server_auth.server_handshake(&mut stream).await
    });
    let mut client = Delimited::new(client_io);

    timeout(TEST_TIMEOUT, client_auth.client_handshake(&mut client))
        .await
        .expect("client handshake timed out")
        .expect("client sends authentication response");
    let response = timeout(TEST_TIMEOUT, client.recv_server())
        .await
        .expect("authentication result timed out")
        .expect("read authentication result");
    assert_eq!(
        response,
        Some(ServerMsg::Error {
            reason: "authentication failed".into()
        })
    );

    let server_error = timeout(TEST_TIMEOUT, server)
        .await
        .expect("server handshake timed out")
        .expect("server task")
        .expect_err("wrong secret must fail");
    assert!(server_error.to_string().contains("invalid secret"));
}

#[tokio::test]
async fn missing_authentication_response_is_rejected() {
    let (server_io, client_io) = duplex(4096);
    let server_auth = Authenticator::new("required-secret").expect("server authenticator");
    let server = tokio::spawn(async move {
        let mut stream = Delimited::new(server_io);
        server_auth.server_handshake(&mut stream).await
    });
    let mut client = Delimited::new(client_io);

    let challenge = timeout(TEST_TIMEOUT, client.recv_server())
        .await
        .expect("challenge timed out")
        .expect("read challenge");
    assert!(matches!(challenge, Some(ServerMsg::Challenge { .. })));
    client
        .send_client(ClientMsg::Heartbeat)
        .await
        .expect("send non-authentication response");

    let server_error = timeout(TEST_TIMEOUT, server)
        .await
        .expect("server handshake timed out")
        .expect("server task")
        .expect_err("missing authentication must fail");
    assert!(server_error.to_string().contains("server requires secret"));
}
