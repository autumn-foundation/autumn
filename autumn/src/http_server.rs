//! HTTP serve loop with connection limits (issue #3065).
//!
//! [`serve`] replaces `axum::serve`. axum gives no access to the hyper
//! builder. Thus axum cannot set a header-read timeout, a header size limit
//! or an HTTP/2 stream limit. This loop applies [`HttpLimits`] from
//! `[server.http]`. With default limits it behaves like `axum::serve`.
//!
//! # Timers
//!
//! Each connection is in one of three phases:
//!
//! - **Head**: waiting for a request head. `header_read_timeout` applies
//!   (if it is not set, `keep_alive_timeout`). The timer starts when the
//!   connection opens, or at the first byte after an idle period. More bytes
//!   do not restart it, so a slowloris client cannot keep the connection open.
//! - **Busy**: at least one request is in flight. No timer applies. The
//!   request timeout layer limits handlers.
//! - **Idle**: no request in flight. `keep_alive_timeout` applies.
//!
//! On HTTP/2, control frames between requests (for example `PING`) do not
//! start a head timer. An open header block does: a `HEADERS` frame without
//! `END_HEADERS` starts `header_read_timeout`, in any phase, until a frame
//! with `END_HEADERS` arrives. The timers also run during a graceful drain.

use std::convert::Infallible;
use std::fmt::Debug;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;

use crate::config::HttpServerConfig;

/// Resolved `[server.http]` limits. `0` in the config means "off".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HttpLimits {
    /// Maximum time to receive a full request head.
    pub header_read_timeout: Option<Duration>,
    /// Maximum idle time with no request in flight.
    pub keep_alive_timeout: Option<Duration>,
    /// Maximum request head size, in bytes.
    pub max_header_bytes: Option<usize>,
    /// Maximum concurrent HTTP/2 streams per connection.
    pub http2_max_concurrent_streams: Option<u32>,
    /// Maximum open connections.
    pub max_connections: Option<usize>,
}

impl From<&HttpServerConfig> for HttpLimits {
    fn from(config: &HttpServerConfig) -> Self {
        let ms = |value: Option<u64>| value.filter(|v| *v > 0).map(Duration::from_millis);
        Self {
            header_read_timeout: ms(config.header_read_timeout_ms),
            keep_alive_timeout: ms(config.keep_alive_timeout_ms),
            // hyper panics below the minimum. `validate` rejects it; clamp
            // for a config that skipped `validate`.
            max_header_bytes: config
                .max_header_bytes
                .filter(|v| *v > 0)
                .map(|v| v.max(HttpServerConfig::MIN_HEADER_BYTES)),
            http2_max_concurrent_streams: config.http2_max_concurrent_streams,
            max_connections: config.max_connections.filter(|v| *v > 0),
        }
    }
}

impl HttpLimits {
    const fn has_timers(&self) -> bool {
        self.header_read_timeout.is_some() || self.keep_alive_timeout.is_some()
    }

    fn builder(&self) -> Builder<TokioExecutor> {
        let mut builder = Builder::new(TokioExecutor::new());
        // CONNECT protocol for HTTP/2 WebSockets, as `axum::serve` sets it.
        builder.http2().enable_connect_protocol();
        if let Some(bytes) = self.max_header_bytes {
            builder.http1().max_buf_size(bytes);
            builder
                .http2()
                .max_header_list_size(u32::try_from(bytes).unwrap_or(u32::MAX));
        }
        if let Some(streams) = self.http2_max_concurrent_streams {
            builder.http2().max_concurrent_streams(streams);
        }
        builder
    }
}

/// One accepted connection, given to the make-service.
///
/// The same role as `axum::serve::IncomingStream`, which has no public
/// constructor.
pub struct IncomingStream<'a, L: Listener> {
    io: &'a L::Io,
    remote_addr: L::Addr,
}

impl<L: Listener> IncomingStream<'_, L> {
    /// The accepted IO.
    #[must_use]
    pub const fn io(&self) -> &L::Io {
        self.io
    }

    /// The peer address.
    #[must_use]
    pub const fn remote_addr(&self) -> &L::Addr {
        &self.remote_addr
    }
}

/// Serve `make_service` on `listener` until `signal` resolves, then drain.
///
/// After `signal`, the loop stops accepting, asks every connection to shut
/// down gracefully, and returns when all connections have closed.
///
/// # Errors
///
/// Never returns an error today. The `io::Result` matches `axum::serve`.
pub async fn serve<L, M, S, F>(
    mut listener: L,
    mut make_service: M,
    limits: HttpLimits,
    signal: F,
) -> io::Result<()>
where
    L: Listener,
    L::Addr: Debug,
    M: for<'a> tower::Service<IncomingStream<'a, L>, Error = Infallible, Response = S>
        + Send
        + 'static,
    for<'a> <M as tower::Service<IncomingStream<'a, L>>>::Future: Send,
    S: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use tower::ServiceExt as _;

    let permits = limits.max_connections.map(|n| Arc::new(Semaphore::new(n)));
    let builder = limits.builder();
    let (signal_tx, signal_rx) = watch::channel(());
    tokio::spawn(async move {
        signal.await;
        drop(signal_rx);
    });
    let (close_tx, close_rx) = watch::channel(());

    loop {
        // At the cap, wait for a free slot before `accept`. The kernel queues
        // new connections meanwhile.
        let permit = match &permits {
            Some(permits) => tokio::select! {
                permit = Arc::clone(permits).acquire_owned() => permit.ok(),
                () = signal_tx.closed() => break,
            },
            None => None,
        };
        let (io, remote_addr) = tokio::select! {
            conn = listener.accept() => conn,
            () = signal_tx.closed() => break,
        };

        make_service
            .ready()
            .await
            .unwrap_or_else(|err| match err {});
        let service = make_service
            .call(IncomingStream {
                io: &io,
                remote_addr,
            })
            .await
            .unwrap_or_else(|err| match err {});

        let timers = limits
            .has_timers()
            .then(|| Arc::new(ConnTimers::new(&limits)));
        let service = Tracked {
            inner: service,
            timers: timers.clone(),
        };
        let service = service.map_request(|req: axum::http::Request<hyper::body::Incoming>| {
            req.map(axum::body::Body::new)
        });
        let hyper_service = hyper_util::service::TowerToHyperService::new(service);
        let io = TokioIo::new(ConnIo {
            inner: io,
            timers: timers.clone(),
            _permit: permit,
        });

        let builder = builder.clone();
        let signal_tx = signal_tx.clone();
        let close_rx = close_rx.clone();
        #[allow(
            clippy::significant_drop_tightening,
            reason = "the connection must live until the task ends"
        )]
        let connection = async move {
            let conn = builder.serve_connection_with_upgrades(io, hyper_service);
            let mut conn = std::pin::pin!(conn);
            let mut signal_closed = std::pin::pin!(signal_tx.closed());
            // Set by a graceful shutdown. Until then hyper can finish its
            // GOAWAY sequence and flush the last response bytes.
            let mut grace_until: Option<Instant> = None;
            loop {
                let shutting_down = grace_until.is_some();
                // A slow head closes at once. An idle close waits for the grace.
                let deadline = timers.as_ref().and_then(|t| t.deadline_and_expiry()).map(
                    |(deadline, expiry)| match (expiry, grace_until) {
                        (Expiry::Idle, Some(grace)) => deadline.max(grace),
                        _ => deadline,
                    },
                );
                tokio::select! {
                    _ = conn.as_mut() => break,
                    () = &mut signal_closed, if !shutting_down => {
                        grace_until = Some(Instant::now() + SHUTDOWN_GRACE);
                        conn.as_mut().graceful_shutdown();
                    }
                    // The timers also run during a drain, so a slow client
                    // cannot hold the drain open.
                    () = sleep_until(deadline), if deadline.is_some() => {
                        match timers.as_ref().and_then(|t| t.expired()) {
                            // A slow head: close now. No response is lost.
                            Some(Expiry::Head) => break,
                            // Still open after the grace period: close now.
                            Some(Expiry::Idle) if shutting_down => break,
                            Some(Expiry::Idle) => {
                                grace_until = Some(Instant::now() + SHUTDOWN_GRACE);
                                conn.as_mut().graceful_shutdown();
                            }
                            None => {}
                        }
                    }
                    () = changed(timers.as_deref()) => {}
                }
            }
            drop(close_rx);
        };
        tokio::spawn(connection);
    }

    drop(close_rx);
    drop(listener);
    close_tx.closed().await;
    Ok(())
}

/// Resolves when the connection phase changes. Never without timers.
async fn changed(timers: Option<&ConnTimers>) {
    match timers {
        Some(timers) => timers.changed.notified().await,
        None => std::future::pending().await,
    }
}

/// Time a connection gets to close by itself after a graceful shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

// ── Per-connection timers ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Phase {
    Head { since: Instant },
    Busy { in_flight: usize },
    Idle { since: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expiry {
    Head,
    Idle,
}

#[derive(Debug)]
struct PhaseState {
    phase: Phase,
    scan: Scan,
    /// Set while an HTTP/2 header block is open.
    header_block_since: Option<Instant>,
}

/// The HTTP/2 client preface.
const H2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_FRAME_HEADERS: u8 = 0x1;
const H2_FRAME_CONTINUATION: u8 = 0x9;
const H2_FLAG_END_HEADERS: u8 = 0x4;

/// A scan of the bytes the client sends: HTTP/1, or HTTP/2 frame headers.
#[derive(Debug)]
enum Scan {
    /// Comparing the first bytes with the HTTP/2 preface.
    Preface {
        matched: usize,
    },
    Http1,
    /// Between HTTP/2 frames: `have` bytes of the 9-byte frame header, then
    /// `skip` payload bytes. `ends_block` is set while the payload of the
    /// last frame of a header block (`END_HEADERS`) is still to come.
    Frames {
        header: [u8; 9],
        have: usize,
        skip: usize,
        ends_block: bool,
    },
}

impl PhaseState {
    const fn http2(&self) -> bool {
        matches!(self.scan, Scan::Frames { .. })
    }

    /// Advance the scan over `bytes`. Opens or closes the HTTP/2 header block.
    fn scan(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match &mut self.scan {
                Scan::Http1 => return,
                Scan::Preface { matched } => {
                    let rest = &H2_PREFACE[*matched..];
                    let n = rest.len().min(bytes.len());
                    if bytes[..n] != rest[..n] {
                        self.scan = Scan::Http1;
                        return;
                    }
                    *matched += n;
                    bytes = &bytes[n..];
                    if *matched == H2_PREFACE.len() {
                        self.scan = Scan::Frames {
                            header: [0; 9],
                            have: 0,
                            skip: 0,
                            ends_block: false,
                        };
                    }
                }
                Scan::Frames {
                    header,
                    have,
                    skip,
                    ends_block,
                } => {
                    if *skip > 0 {
                        let n = (*skip).min(bytes.len());
                        *skip -= n;
                        bytes = &bytes[n..];
                        // The block ends only when its last payload byte is in.
                        if *skip == 0 && std::mem::take(ends_block) {
                            self.header_block_since = None;
                        }
                        continue;
                    }
                    let n = (9 - *have).min(bytes.len());
                    header[*have..*have + n].copy_from_slice(&bytes[..n]);
                    *have += n;
                    bytes = &bytes[n..];
                    if *have < 9 {
                        return;
                    }
                    *have = 0;
                    *skip = usize::from(header[0]) << 16
                        | usize::from(header[1]) << 8
                        | usize::from(header[2]);
                    let (kind, flags) = (header[3], header[4]);
                    if kind == H2_FRAME_HEADERS || kind == H2_FRAME_CONTINUATION {
                        self.header_block_since.get_or_insert_with(Instant::now);
                        *ends_block = flags & H2_FLAG_END_HEADERS != 0;
                        if *ends_block && *skip == 0 {
                            *ends_block = false;
                            self.header_block_since = None;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct ConnTimers {
    state: Mutex<PhaseState>,
    changed: Notify,
    header_read_timeout: Option<Duration>,
    keep_alive_timeout: Option<Duration>,
}

impl ConnTimers {
    fn new(limits: &HttpLimits) -> Self {
        Self {
            state: Mutex::new(PhaseState {
                phase: Phase::Head {
                    since: Instant::now(),
                },
                scan: Scan::Preface { matched: 0 },
                header_block_since: None,
            }),
            changed: Notify::new(),
            header_read_timeout: limits.header_read_timeout,
            keep_alive_timeout: limits.keep_alive_timeout,
        }
    }

    fn update(&self, f: impl FnOnce(&mut PhaseState)) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&mut state);
        }
        self.changed.notify_one();
    }

    /// Bytes arrived. On HTTP/1, the first byte after an idle period starts
    /// the head timer. On HTTP/2, an open header block starts it.
    fn on_read(&self, bytes: &[u8]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let block_before = state.header_block_since.is_some();
        state.scan(bytes);
        let head_starts = matches!(state.phase, Phase::Idle { .. }) && !state.http2();
        if head_starts {
            state.phase = Phase::Head {
                since: Instant::now(),
            };
        }
        let changed = head_starts || block_before != state.header_block_since.is_some();
        drop(state);
        if changed {
            self.changed.notify_one();
        }
    }

    fn request_started(&self) {
        self.update(|state| {
            state.phase = match state.phase {
                Phase::Busy { in_flight } => Phase::Busy {
                    in_flight: in_flight + 1,
                },
                Phase::Head { .. } | Phase::Idle { .. } => Phase::Busy { in_flight: 1 },
            };
        });
    }

    fn request_finished(&self) {
        self.update(|state| {
            if let Phase::Busy { in_flight } = state.phase {
                state.phase = if in_flight > 1 {
                    Phase::Busy {
                        in_flight: in_flight - 1,
                    }
                } else {
                    Phase::Idle {
                        since: Instant::now(),
                    }
                };
            }
        });
    }

    fn deadline_and_expiry(&self) -> Option<(Instant, Expiry)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let phase = match state.phase {
            // With no head timeout, the idle timeout bounds the wait.
            Phase::Head { since } => self
                .header_read_timeout
                .or(self.keep_alive_timeout)
                .map(|timeout| (since + timeout, Expiry::Head)),
            // An open HTTP/2 header block suspends the idle timer: the new
            // request gets the full header timeout.
            Phase::Idle { .. }
                if state.header_block_since.is_some() && self.header_read_timeout.is_some() =>
            {
                None
            }
            Phase::Idle { since } => self
                .keep_alive_timeout
                .map(|timeout| (since + timeout, Expiry::Idle)),
            Phase::Busy { .. } => None,
        };
        // An open HTTP/2 header block, also next to other streams in flight.
        let block = state
            .header_block_since
            .zip(self.header_read_timeout)
            .map(|(since, timeout)| (since + timeout, Expiry::Head));
        match (phase, block) {
            (Some(a), Some(b)) => Some(if b.0 < a.0 { b } else { a }),
            (a, b) => a.or(b),
        }
    }

    /// The expired timer, if any. The loop checks it again on wake, because
    /// the phase can change while the timer sleeps.
    fn expired(&self) -> Option<Expiry> {
        self.deadline_and_expiry()
            .filter(|(deadline, _)| *deadline <= Instant::now())
            .map(|(_, expiry)| expiry)
    }
}

/// The accepted IO, with the connection permit and the read hook.
struct ConnIo<I> {
    inner: I,
    timers: Option<Arc<ConnTimers>>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl<I: AsyncRead + Unpin> AsyncRead for ConnIo<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before
            && let Some(timers) = &self.timers
        {
            timers.on_read(&buf.filled()[before..]);
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for ConnIo<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Marks a request in flight from `call` until its response body is done.
#[derive(Clone)]
struct Tracked<S> {
    inner: S,
    timers: Option<Arc<ConnTimers>>,
}

struct InFlight(Arc<ConnTimers>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.request_finished();
    }
}

impl<S> tower::Service<axum::extract::Request> for Tracked<S>
where
    S: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = Infallible,
        >,
{
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = TrackedFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        // An HTTP/2 CONNECT stream (a WebSocket, RFC 8441) stays open after
        // its response body is gone. Keep it in flight for the connection's
        // life, so the idle timer cannot close the tunnel.
        let tunnel = req.method() == axum::http::Method::CONNECT;
        let guard = self.timers.as_ref().and_then(|timers| {
            timers.request_started();
            (!tunnel).then(|| InFlight(Arc::clone(timers)))
        });
        TrackedFuture {
            inner: self.inner.call(req),
            guard,
        }
    }
}

pin_project_lite::pin_project! {
    /// The response future of [`Tracked`]. Moves the in-flight mark into the
    /// response body.
    struct TrackedFuture<F> {
        #[pin]
        inner: F,
        guard: Option<InFlight>,
    }
}

impl<F> std::future::Future for TrackedFuture<F>
where
    F: std::future::Future<Output = Result<axum::response::Response, Infallible>>,
{
    type Output = Result<axum::response::Response, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let response = std::task::ready!(this.inner.poll(cx))?;
        Poll::Ready(Ok(match this.guard.take() {
            Some(guard) => response.map(|body| {
                axum::body::Body::new(TrackedBody {
                    inner: body,
                    _guard: guard,
                })
            }),
            None => response,
        }))
    }
}

/// A response body that ends the in-flight mark when dropped.
struct TrackedBody {
    inner: axum::body::Body,
    _guard: InFlight,
}

impl http_body::Body for TrackedBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PhaseState {
        PhaseState {
            phase: Phase::Busy { in_flight: 0 },
            scan: Scan::Preface { matched: 0 },
            header_block_since: None,
        }
    }

    fn frame(kind: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
        let len = u8::try_from(payload.len()).expect("small test payload");
        let mut out = vec![0, 0, len, kind, flags, 0, 0, 0, 1];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn http1_bytes_end_the_scan() {
        let mut state = state();
        state.scan(b"GET / HTTP/1.1\r\n");
        assert!(matches!(state.scan, Scan::Http1));
        assert!(!state.http2());
    }

    #[tokio::test]
    async fn open_header_block_is_tracked_across_split_reads() {
        let mut state = state();
        let mut bytes = H2_PREFACE.to_vec();
        bytes.extend(frame(0x4, 0, &[0; 6])); // SETTINGS with a payload
        bytes.extend(frame(H2_FRAME_HEADERS, 0, &[0x82, 0x86])); // no END_HEADERS
        for chunk in bytes.chunks(5) {
            state.scan(chunk);
        }
        assert!(state.http2());
        assert!(state.header_block_since.is_some(), "block is open");

        state.scan(&frame(H2_FRAME_CONTINUATION, H2_FLAG_END_HEADERS, &[0x84]));
        assert!(state.header_block_since.is_none(), "END_HEADERS closes it");

        state.scan(&frame(H2_FRAME_HEADERS, H2_FLAG_END_HEADERS | 0x1, &[0x82]));
        assert!(
            state.header_block_since.is_none(),
            "a full block stays closed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_header_block_suspends_the_idle_deadline() {
        let timers = ConnTimers::new(&HttpLimits {
            header_read_timeout: Some(Duration::from_secs(10)),
            keep_alive_timeout: Some(Duration::from_secs(75)),
            ..HttpLimits::default()
        });
        timers.on_read(H2_PREFACE);
        timers.request_started();
        timers.request_finished();
        tokio::time::advance(Duration::from_secs(74)).await;

        // A request starts 1 s before the idle deadline: it gets the full
        // header timeout, not the 1 s that is left of the idle period.
        let full = frame(H2_FRAME_HEADERS, H2_FLAG_END_HEADERS, &[0x82, 0x86, 0x84]);
        timers.on_read(&full[..9]);
        let (deadline, expiry) = timers.deadline_and_expiry().expect("a deadline");
        assert_eq!(expiry, Expiry::Head);
        assert_eq!(deadline, Instant::now() + Duration::from_secs(10));

        timers.on_read(&full[9..]);
        let (_, expiry) = timers.deadline_and_expiry().expect("idle again");
        assert_eq!(expiry, Expiry::Idle);
    }

    #[tokio::test]
    async fn end_headers_closes_the_block_only_after_its_payload() {
        let mut state = state();
        state.scan(H2_PREFACE);
        let full = frame(H2_FRAME_HEADERS, H2_FLAG_END_HEADERS, &[0x82, 0x86, 0x84]);
        // Only the 9-byte frame header and one payload byte.
        state.scan(&full[..10]);
        assert!(
            state.header_block_since.is_some(),
            "the head is not complete until the payload is in"
        );
        state.scan(&full[10..]);
        assert!(state.header_block_since.is_none());
    }
}
