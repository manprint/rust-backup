//! Control-channel endpoint parsing and TLS connection.
//! Vendored from bore, simplified for rb-transport (no vhost/test-udp routing).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub const CONTROL_PORT: u16 = 7835;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// Build a control-channel TLS acceptor from PEM files.
///
/// This intentionally accepts no client certificate: peers are authenticated by
/// the optional shared secret after the yamux control stream is established.
pub fn load_server_tls(cert_file: &str, key_file: &str) -> Result<TlsAcceptor> {
    let cert_pem = std::fs::read(cert_file)
        .with_context(|| format!("failed to read TLS certificate {cert_file}"))?;
    let key_pem = std::fs::read(key_file)
        .with_context(|| format!("failed to read TLS private key {key_file}"))?;
    server_tls_from_pem(&cert_pem, &key_pem)
}

/// Build a control-channel TLS acceptor from PEM bytes.
pub fn server_tls_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<TlsAcceptor> {
    let mut cert_reader = std::io::BufReader::new(cert_pem);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()
        .context("failed to parse TLS certificate PEM")?;
    anyhow::ensure!(
        !certs.is_empty(),
        "no certificates found in TLS certificate PEM"
    );
    let mut key_reader = std::io::BufReader::new(key_pem);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("failed to parse TLS private-key PEM")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in TLS private-key PEM"))?;
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("failed to configure TLS protocol versions")?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("invalid TLS certificate/key pair")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub enum ControlStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for ControlStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ControlStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_flush(cx),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            ControlStream::Plain(s) => s.is_write_vectored(),
            ControlStream::Tls(s) => s.is_write_vectored(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Endpoint {
    pub fn parse(to: &str) -> Self {
        let (tls, default_port, rest) = if let Some(rest) = to.strip_prefix("https://") {
            (true, 443, rest)
        } else if let Some(rest) = to.strip_prefix("http://") {
            (false, 80, rest)
        } else {
            (false, CONTROL_PORT, to)
        };
        let authority = rest.split('/').next().unwrap_or(rest);

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
                Ok(port) => (host, port),
                Err(_) => (authority, default_port),
            },
            _ => (authority, default_port),
        };
        Endpoint {
            host: host.to_string(),
            port,
            tls,
        }
    }
}

pub async fn connect(endpoint: &Endpoint, insecure: bool) -> Result<ControlStream> {
    let tcp = connect_with_timeout(&endpoint.host, endpoint.port).await?;
    if !endpoint.tls {
        return Ok(ControlStream::Plain(tcp));
    }
    let tls = if insecure {
        insecure_client_config()
    } else {
        default_client_config()
    }?;
    let connector = TlsConnector::from(Arc::new(tls));
    let server_name = ServerName::try_from(endpoint.host.as_str())
        .context("invalid server name")?
        .to_owned();
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| anyhow::anyhow!("tls handshake failed: {e}"))?;
    Ok(ControlStream::Tls(Box::new(tls_stream)))
}

pub async fn connect_with_timeout(host: &str, port: u16) -> Result<TcpStream> {
    timeout(NETWORK_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .context("connection timeout")?
        .context("failed to connect")
}

fn default_client_config() -> Result<ClientConfig> {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("failed to configure TLS protocol versions")?
    .with_root_certificates(root_store)
    .with_no_client_auth())
}

fn insecure_client_config() -> Result<ClientConfig> {
    #[derive(Debug)]
    struct InsecureVerifier;

    impl ServerCertVerifier for InsecureVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, TlsError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            tokio_rustls::rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    Ok(ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("failed to configure TLS protocol versions")?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(InsecureVerifier))
    .with_no_client_auth())
}
