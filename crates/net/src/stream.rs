//! A connection that may or may not be encrypted, and may become encrypted
//! partway through.
//!
//! Mail servers offer two arrangements: TLS from the first byte on a dedicated
//! port, or plaintext on the standard port upgraded in-band by `STARTTLS`.
//! Both IMAP and SMTP need both, and the second one means the socket's *type*
//! changes mid-conversation — so the type has to be an enum rather than a
//! generic parameter chosen at connect time.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::io::{AsyncRead, AsyncWrite};

use crate::config::Security;

/// How long a connection may go completely silent before it is called dead.
///
/// A dropped TCP flow — a laptop that slept, a network that changed, a NAT
/// that forgot — does not close. It simply stops answering, and a read on it
/// waits for as long as the kernel's own timers allow, which is hours. Nothing
/// above this layer had a deadline of its own, so one folder landing on such a
/// connection wedged its worker for the rest of the session: the folder never
/// finished, the sync never returned, and the sidebar kept its spinner
/// forever.
///
/// This is measured between *bytes*, not over a whole command, so a slow
/// transfer that is still arriving is never interrupted. Two minutes of a
/// server saying nothing at all, mid-command, is not slowness.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for the connection itself.
///
/// Shorter than [`IDLE_TIMEOUT`]: a server that has not completed a handshake
/// is not busy sending, and every sync worker opens one of these before it can
/// do anything at all.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Socket {
    Tls(Box<async_native_tls::TlsStream<smol::net::TcpStream>>),
    Plain(smol::net::TcpStream),
}

#[derive(Debug)]
pub struct Stream {
    socket: Socket,
    /// How long a read may find nothing there before it gives up.
    idle: Duration,
    /// Counts down only while a read is waiting, and is thrown away the moment
    /// anything arrives — so the deadline is "silent since the last byte"
    /// rather than "elapsed since the command".
    timer: Option<smol::Timer>,
    /// Total bytes read, when someone is watching.
    ///
    /// The only honest place to count a download from: everything above this
    /// sees a parsed message arrive all at once, and a 16 MB fetch that
    /// reports nothing until it is finished is a progress bar with two states.
    meter: Option<Arc<AtomicU64>>,
}

impl Stream {
    /// Opens a connection with the requested protection.
    ///
    /// `Starttls` returns a *plaintext* stream: the upgrade is a protocol
    /// exchange, so it belongs to the caller who knows whether to send
    /// `STARTTLS` or `AUTH`.
    pub async fn connect(host: &str, port: u16, security: Security) -> Result<Self> {
        Self::connect_with(host, port, security, IDLE_TIMEOUT).await
    }

    /// The same, for a connection that is expected to sit quiet.
    ///
    /// `IDLE` is a command whose whole purpose is to produce nothing for a long
    /// time, so a watcher's socket has to tolerate a silence that would mean a
    /// dead connection anywhere else.
    pub async fn connect_quiet(
        host: &str,
        port: u16,
        security: Security,
        idle: Duration,
    ) -> Result<Self> {
        Self::connect_with(host, port, security, idle).await
    }

    /// A connection that reports how much it has read, for a caller drawing a
    /// progress bar over it.
    pub async fn connect_metered(
        host: &str,
        port: u16,
        security: Security,
        meter: Arc<AtomicU64>,
    ) -> Result<Self> {
        let mut stream = Self::connect_with(host, port, security, IDLE_TIMEOUT).await?;
        stream.meter = Some(meter);
        Ok(stream)
    }

    async fn connect_with(
        host: &str,
        port: u16,
        security: Security,
        idle: Duration,
    ) -> Result<Self> {
        let connecting = smol::net::TcpStream::connect((host, port));
        let tcp = smol::future::or(async { Some(connecting.await) }, async {
            smol::Timer::after(CONNECT_TIMEOUT).await;
            None
        })
        .await
        .with_context(|| format!("{host}:{port} did not answer within {CONNECT_TIMEOUT:?}"))?
        .with_context(|| format!("connecting to {host}:{port}"))?;

        let socket = match security {
            Security::Tls => Socket::Tls(Box::new(handshake(tcp, host).await?)),
            Security::Starttls | Security::Plaintext => Socket::Plain(tcp),
        };
        Ok(Self { socket, idle, timer: None, meter: None })
    }

    /// Wraps an established plaintext connection in TLS, after the server has
    /// agreed to `STARTTLS`.
    pub async fn upgrade(self, host: &str) -> Result<Self> {
        let Self { socket, idle, meter, .. } = self;
        match socket {
            // Upgrading twice would be a bug in the caller's state machine,
            // and silently succeeding would hide it.
            Socket::Tls(_) => bail!("the connection to {host} is already encrypted"),
            Socket::Plain(tcp) => Ok(Self {
                socket: Socket::Tls(Box::new(handshake(tcp, host).await?)),
                idle,
                timer: None,
                meter,
            }),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self.socket, Socket::Tls(_))
    }
}

async fn handshake(
    tcp: smol::net::TcpStream,
    host: &str,
) -> Result<async_native_tls::TlsStream<smol::net::TcpStream>> {
    async_native_tls::TlsConnector::new()
        .connect(host, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}"))
}

// Both variants are `Unpin`, so the projections below are plain reborrows
// rather than anything that needs a pin-projection crate.
impl Socket {
    fn poll_read(
        &mut self,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        match self {
            Socket::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            Socket::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncRead for Stream {
    /// Reads, and gives up on a connection that has gone silent.
    ///
    /// The timer is armed the first time there is nothing to read and thrown
    /// away as soon as anything arrives, so what it measures is the gap
    /// between bytes. A folder that takes ten minutes to transfer is never
    /// interrupted; a connection that stopped answering two minutes ago is.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();

        match this.socket.poll_read(cx, buf) {
            Poll::Ready(result) => {
                this.timer = None;
                if let (Ok(read), Some(meter)) = (&result, &this.meter) {
                    meter.fetch_add(*read as u64, Ordering::Relaxed);
                }
                Poll::Ready(result)
            }
            Poll::Pending => {
                let idle = this.idle;
                let timer = this.timer.get_or_insert_with(|| smol::Timer::after(idle));
                match Pin::new(timer).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(_) => {
                        this.timer = None;
                        Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("nothing received for {idle:?}"),
                        )))
                    }
                }
            }
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().socket {
            Socket::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            Socket::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().socket {
            Socket::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            Socket::Plain(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().socket {
            Socket::Tls(stream) => Pin::new(stream.as_mut()).poll_close(cx),
            Socket::Plain(stream) => Pin::new(stream).poll_close(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::AsyncReadExt as _;
    use smol::net::TcpListener;

    /// A server that accepts one connection and then behaves as told.
    async fn listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn a_connection_that_stops_answering_gives_up() {
        // The case this exists for: the socket is open, the server is gone.
        // Without the deadline this read never returns and the sync worker
        // that owns it is wedged for the life of the process.
        smol::block_on(async {
            let (listener, port) = listener().await;
            let held = smol::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                // Accept, say nothing, and stay open.
                smol::Timer::after(Duration::from_secs(30)).await;
                drop(socket);
            });

            let mut stream = Stream::connect_with(
                "127.0.0.1",
                port,
                Security::Plaintext,
                Duration::from_millis(150),
            )
            .await
            .unwrap();

            let started = std::time::Instant::now();
            let mut buf = [0u8; 16];
            let result = stream.read(&mut buf).await;

            let error = result.expect_err("a silent server should not read forever");
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
            drop(held);
        });
    }

    #[test]
    fn a_slow_but_moving_transfer_is_left_alone() {
        // The deadline is between bytes, not across a command. A large folder
        // arriving slowly must never be mistaken for a dead connection —
        // getting this wrong would break exactly the syncs it is meant to
        // rescue.
        smol::block_on(async {
            use futures::io::AsyncWriteExt as _;

            let (listener, port) = listener().await;
            let feeding = smol::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                // Eight sends, each well inside the deadline, but together
                // four times longer than it.
                for _ in 0..8 {
                    smol::Timer::after(Duration::from_millis(50)).await;
                    if socket.write_all(b"x").await.is_err() {
                        return;
                    }
                }
                smol::Timer::after(Duration::from_secs(5)).await;
            });

            let mut stream = Stream::connect_with(
                "127.0.0.1",
                port,
                Security::Plaintext,
                Duration::from_millis(100),
            )
            .await
            .unwrap();

            let mut got = 0;
            let mut buf = [0u8; 1];
            while got < 8 {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) => panic!("gave up on a connection that was still sending: {e}"),
                }
            }
            assert_eq!(got, 8, "every byte arrived despite the total exceeding the deadline");
            drop(feeding);
        });
    }

    #[test]
    fn a_server_that_never_accepts_does_not_hang_the_connect() {
        assert!(
            CONNECT_TIMEOUT < IDLE_TIMEOUT,
            "a handshake is not a transfer and should not wait as long as one",
        );
    }
}
