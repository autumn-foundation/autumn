//! Stop accepting before draining, so a just-accepted connection is served.
//!
//! `axum::serve(..).with_graceful_shutdown(signal)` does two things the moment
//! `signal` resolves: it stops calling `accept()`, and it tells every live
//! connection to shut down gracefully. hyper's HTTP/1 graceful shutdown
//! (`Conn::disable_keep_alive`) closes a connection *immediately* when it is
//! idle — and a connection accepted a few microseconds earlier, whose request
//! bytes are sitting unread in the socket buffer, is idle. So a request that
//! raced the shutdown signal was accepted, then closed with no response: the
//! client reads a clean EOF and zero bytes.
//!
//! That is rare on a normal SIGTERM, but an in-place upgrade (#1674) drains
//! the predecessor while the load keeps arriving on the shared listening
//! socket, so it is exactly the window every cutover opens. The hot-upgrade
//! live test caught it under coverage instrumentation, which widens the gap
//! between `accept()` and the connection task's first read.
//!
//! The fix separates the two steps. [`StopAcceptingOnShutdown`] stops handing
//! out connections the moment shutdown begins; [`drain_signal`] only then
//! waits [`ACCEPT_SETTLE`] before starting hyper's drain, long enough for a
//! connection accepted just before the stop to have its request read, which
//! turns it from idle into in-flight so the drain serves it. During an
//! upgrade the successor shares the socket, so connections arriving in that
//! window go to the successor; on a plain shutdown they wait in the kernel
//! queue exactly as they would have once `accept()` stopped anyway.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// How long [`drain_signal`] waits, after shutdown begins, before starting the
/// graceful drain. Long enough for an already-accepted connection's request to
/// be read even on a starved runtime, short enough not to matter against a
/// shutdown budget measured in seconds.
pub const ACCEPT_SETTLE: Duration = Duration::from_millis(100);

/// A listener that hands out no further connections once `stop` is cancelled.
///
/// The inner `accept()` is dropped when `stop` fires; every listener this
/// wraps (`tokio::net::TcpListener`, `UnixListener`, and the TLS listener's
/// channel receive) is cancel-safe, so a connection the kernel has queued but
/// not yet handed over stays queued rather than being lost.
pub struct StopAcceptingOnShutdown<L> {
    inner: L,
    stop: CancellationToken,
}

impl<L> StopAcceptingOnShutdown<L> {
    pub(crate) const fn new(inner: L, stop: CancellationToken) -> Self {
        Self { inner, stop }
    }
}

impl<L> axum::serve::Listener for StopAcceptingOnShutdown<L>
where
    L: axum::serve::Listener,
{
    type Io = L::Io;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        tokio::select! {
            biased;
            () = self.stop.cancelled() => std::future::pending().await,
            conn = self.inner.accept() => conn,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// The TCP peer address as connect info for a [`StopAcceptingOnShutdown`]
/// listener.
///
/// axum implements `Connected` for `SocketAddr` only on a bare
/// `TcpListener`, and the orphan rules forbid that impl for the wrapper, so
/// the TCP serve path carries this instead and [`stamp_tcp_peer`] re-stamps
/// the `ConnectInfo<SocketAddr>` everything downstream reads — the same
/// pattern the TLS path uses for its `TlsConnectInfo`.
#[derive(Clone, Copy, Debug)]
pub struct TcpPeer(std::net::SocketAddr);

impl
    axum::extract::connect_info::Connected<
        axum::serve::IncomingStream<'_, StopAcceptingOnShutdown<tokio::net::TcpListener>>,
    > for TcpPeer
{
    fn connect_info(
        stream: axum::serve::IncomingStream<'_, StopAcceptingOnShutdown<tokio::net::TcpListener>>,
    ) -> Self {
        Self(*stream.remote_addr())
    }
}

impl
    axum::extract::connect_info::Connected<
        crate::http_server::IncomingStream<'_, StopAcceptingOnShutdown<tokio::net::TcpListener>>,
    > for TcpPeer
{
    fn connect_info(
        stream: crate::http_server::IncomingStream<
            '_,
            StopAcceptingOnShutdown<tokio::net::TcpListener>,
        >,
    ) -> Self {
        Self(*stream.remote_addr())
    }
}

/// Re-stamp `ConnectInfo<SocketAddr>` from [`TcpPeer`], so the TCP serve path
/// presents exactly the connect info it did before [`StopAcceptingOnShutdown`]
/// wrapped its listener. Installed inside the connect-info layer, before
/// `TrustedProxiesLayer`.
pub async fn stamp_tcp_peer(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(axum::extract::ConnectInfo(TcpPeer(peer))) = req
        .extensions()
        .get::<axum::extract::ConnectInfo<TcpPeer>>()
        .copied()
    {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
    }
    next.run(req).await
}

/// The graceful-shutdown signal for `axum::serve`: resolves [`ACCEPT_SETTLE`]
/// after `stop` is cancelled, so the listener has already stopped accepting
/// and every connection it did accept has had time to read its request.
pub async fn drain_signal(stop: CancellationToken) {
    stop.cancelled().await;
    tokio::time::sleep(ACCEPT_SETTLE).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Accept a connection, begin shutdown while its request is still unsent,
    /// then send the request. Returns what the client read back.
    ///
    /// `settle` selects the shutdown wiring: `true` is the production wiring
    /// ([`StopAcceptingOnShutdown`] + [`drain_signal`]); `false` is the plain
    /// `with_graceful_shutdown(stop.cancelled())` it replaced.
    async fn request_racing_shutdown(settle: bool) -> Vec<u8> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stop = CancellationToken::new();
        let router = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));

        let server = {
            let stop = stop.clone();
            tokio::spawn(async move {
                if settle {
                    axum::serve(StopAcceptingOnShutdown::new(listener, stop.clone()), router)
                        .with_graceful_shutdown(drain_signal(stop))
                        .await
                } else {
                    axum::serve(listener, router)
                        .with_graceful_shutdown(async move { stop.cancelled().await })
                        .await
                }
            })
        };

        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        // Let the server accept the connection and spawn its task: the
        // connection is now accepted but idle (no request bytes yet).
        tokio::time::sleep(Duration::from_millis(30)).await;
        stop.cancel();
        // The request arrives after shutdown began, well inside the settle
        // window.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = client
            .write_all(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .await;
        let mut raw = Vec::new();
        let _ = client.read_to_end(&mut raw).await;
        server.await.expect("server task").expect("serve");
        raw
    }

    #[tokio::test]
    async fn an_accepted_connection_racing_shutdown_is_served() {
        let raw = request_racing_shutdown(true).await;
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "the connection was accepted before shutdown, so its request must be \
             served, got {text:?}"
        );
    }

    /// Pins the defect the settle window fixes, so the regression test above
    /// cannot pass vacuously: without it, hyper closes the idle connection.
    #[tokio::test]
    async fn without_the_settle_window_the_same_connection_is_dropped() {
        let raw = request_racing_shutdown(false).await;
        assert!(
            raw.is_empty(),
            "hyper closes an accepted-but-idle connection at graceful shutdown; \
             got {:?}",
            String::from_utf8_lossy(&raw)
        );
    }

    #[tokio::test]
    async fn no_connection_is_accepted_after_shutdown_begins() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let stop = CancellationToken::new();
        stop.cancel();
        let mut wrapped = StopAcceptingOnShutdown::new(listener, stop);
        let accepted = tokio::time::timeout(
            Duration::from_millis(50),
            axum::serve::Listener::accept(&mut wrapped),
        )
        .await;
        assert!(accepted.is_err(), "accept must stay pending once stopped");
    }
}
