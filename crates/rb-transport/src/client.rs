//! Client for rb-transport.

use rb_core::config::TransportConfig;
use rb_core::error::BackupError;

use crate::auth::Authenticator;
use crate::channel::PairedChannel;
use crate::mux;
use crate::proto::{ClientMsg, Delimited, ServerMsg};
use crate::transport;

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
        let auth = Authenticator::new(secret);
        auth.client_handshake(&mut control).await.map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("auth: {e}"))
        })?;
    }

    match control
        .recv_server()
        .await
        .map_err(|e| BackupError::phase(rb_core::error::Phase::Connect, format!("recv: {e}")))?
    {
        Some(ServerMsg::Ok) => Ok(PairedChannel::source(acceptor, control)),
        Some(ServerMsg::Error(reason)) => Err(BackupError::phase(
            rb_core::error::Phase::Connect,
            format!("server error: {reason}"),
        )),
        other => Err(BackupError::phase(
            rb_core::error::Phase::Connect,
            format!("unexpected response: {other:?}"),
        )),
    }
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
        let auth = Authenticator::new(secret);
        auth.client_handshake(&mut control).await.map_err(|e| {
            BackupError::phase(rb_core::error::Phase::Connect, format!("auth: {e}"))
        })?;
    }

    match control
        .recv_server()
        .await
        .map_err(|e| BackupError::phase(rb_core::error::Phase::Connect, format!("recv: {e}")))?
    {
        Some(ServerMsg::Ok) => Ok(PairedChannel::destination(opener, control)),
        Some(ServerMsg::Error(reason)) => Err(BackupError::phase(
            rb_core::error::Phase::Connect,
            format!("server error: {reason}"),
        )),
        other => Err(BackupError::phase(
            rb_core::error::Phase::Connect,
            format!("unexpected response: {other:?}"),
        )),
    }
}
