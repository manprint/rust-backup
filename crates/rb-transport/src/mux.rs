//! Stream multiplexing over a single TCP connection via yamux.
//! Vendored from bore with zero modifications.

use std::future::poll_fn;
use std::io;
use std::task::Poll;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, Mode};

pub type Stream = Compat<yamux::Stream>;

pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Transport for T {}

pub const STREAM_READY: u8 = 0;

#[cfg(target_pointer_width = "64")]
const MAX_NUM_STREAMS: usize = 1 << 16;
#[cfg(not(target_pointer_width = "64"))]
const MAX_NUM_STREAMS: usize = 1 << 13;

const _: () = assert!(
    MAX_NUM_STREAMS
        .checked_mul(yamux::DEFAULT_CREDIT as usize)
        .is_some(),
    "MAX_NUM_STREAMS * yamux::DEFAULT_CREDIT must not overflow usize on this target",
);

fn config() -> Config {
    let mut cfg = Config::default();
    cfg.set_max_connection_receive_window(None);
    cfg.set_max_num_streams(MAX_NUM_STREAMS);
    cfg
}

fn disconnected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "multiplexer connection closed")
}

#[derive(Clone)]
pub struct Opener {
    requests: mpsc::Sender<oneshot::Sender<io::Result<Stream>>>,
}

impl Opener {
    pub async fn open(&self) -> io::Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.requests.send(tx).await.map_err(|_| disconnected())?;
        rx.await.map_err(|_| disconnected())?
    }
}

pub struct Acceptor {
    inbound: mpsc::Receiver<Stream>,
}

impl Acceptor {
    pub async fn accept(&mut self) -> Option<Stream> {
        self.inbound.recv().await
    }
}

pub fn client<S: Transport>(socket: S) -> (Opener, Acceptor) {
    spawn_driver(Connection::new(socket.compat(), config(), Mode::Client))
}

pub fn server<S: Transport>(socket: S) -> (Opener, Acceptor) {
    spawn_driver(Connection::new(socket.compat(), config(), Mode::Server))
}

fn spawn_driver<S: Transport>(conn: Connection<Compat<S>>) -> (Opener, Acceptor) {
    let (open_tx, open_rx) = mpsc::channel(32);
    let (inbound_tx, inbound_rx) = mpsc::channel(32);
    tokio::spawn(drive(conn, open_rx, inbound_tx));
    (
        Opener { requests: open_tx },
        Acceptor {
            inbound: inbound_rx,
        },
    )
}

async fn drive<S: Transport>(
    mut conn: Connection<Compat<S>>,
    mut open_rx: mpsc::Receiver<oneshot::Sender<io::Result<Stream>>>,
    inbound_tx: mpsc::Sender<Stream>,
) {
    enum Step {
        Inbound(yamux::Stream),
        Opened(Result<yamux::Stream, yamux::ConnectionError>),
        Done,
    }

    let mut pending: Option<oneshot::Sender<io::Result<Stream>>> = None;
    let mut openers_gone = false;

    loop {
        let step = poll_fn(|cx| {
            if pending.is_none() && !openers_gone {
                match open_rx.poll_recv(cx) {
                    Poll::Ready(Some(reply)) => pending = Some(reply),
                    Poll::Ready(None) => openers_gone = true,
                    Poll::Pending => {}
                }
            }
            if pending.is_some() {
                if let Poll::Ready(result) = conn.poll_new_outbound(cx) {
                    return Poll::Ready(Step::Opened(result));
                }
            }
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => Poll::Ready(Step::Inbound(stream)),
                Poll::Ready(Some(Err(_)) | None) => Poll::Ready(Step::Done),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;

        match step {
            Step::Opened(result) => {
                if let Some(reply) = pending.take() {
                    let _ = reply.send(
                        result
                            .map(FuturesAsyncReadCompatExt::compat)
                            .map_err(io::Error::other),
                    );
                }
            }
            Step::Inbound(stream) => {
                let _ = inbound_tx.send(stream.compat()).await;
            }
            Step::Done => break,
        }
    }

    let _ = poll_fn(|cx| conn.poll_close(cx)).await;
}
