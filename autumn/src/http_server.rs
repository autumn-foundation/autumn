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
//! - **Head**: the connection opened and no request arrived yet.
//!   `header_read_timeout` applies (if it is not set, `keep_alive_timeout`).
//! - **Busy**: at least one request is in flight. No timer applies. The
//!   request timeout layer limits handlers.
//! - **Idle**: no request in flight. `keep_alive_timeout` applies.
//!
//! In every phase, an open head also starts `header_read_timeout`, and
//! suspends the idle timer. The timer starts at the first byte of the head.
//! More bytes do not restart it, so a slowloris client cannot keep the
//! connection open. To find heads, the read path scans the client bytes:
//!
//! - HTTP/1: a small framer skips bodies (`Content-Length`, chunked). A head
//!   runs from its first byte to its blank line, also when it is pipelined
//!   behind another request. CRLF between messages does not open a head.
//! - HTTP/2: a head is a header block (`HEADERS` and `CONTINUATION` frames,
//!   up to the payload of the `END_HEADERS` frame). Control frames (for
//!   example `PING`) do not open one.
//!
//! The timers also run during a graceful drain.

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
            let _close_timers = CloseOnDrop(timers.clone());
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

/// Closes the timers when the connection task ends, also on abort.
struct CloseOnDrop(Option<Arc<ConnTimers>>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let Some(timers) = &self.0 {
            timers.close();
        }
    }
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
    /// Set while a request head (HTTP/1) or a header block (HTTP/2) is open.
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
    Http1(H1Scan),
    /// Between HTTP/2 frames: `have` bytes of the 9-byte frame header, then
    /// `skip` payload bytes. `ends_block` is set while the payload of the
    /// last frame of a header block (`END_HEADERS`) is still to come.
    Frames {
        header: [u8; 9],
        have: usize,
        skip: usize,
        ends_block: bool,
        /// The header timer started at the first byte of a frame header that
        /// is not complete yet. Control traffic cancels it.
        provisional: bool,
    },
}

/// A minimal HTTP/1 framer. It finds where each request head starts and
/// ends, and skips request bodies (`Content-Length` or chunked). hyper parses
/// the same bytes; this scan only times the heads, so a pipelined partial
/// head is timed too.
#[derive(Debug, Default)]
struct H1Scan {
    state: H1State,
    /// What the scan keeps of the current line.
    line: LineScan,
    content_length: u64,
    chunked: bool,
    position: HeadPosition,
}

/// The parts of the current line that frame a message. The scan reads
/// values byte by byte and does not keep the line, so a long valid line
/// (optional whitespace, leading zeros, chunk extensions) still frames.
#[derive(Debug, Default)]
struct LineScan {
    /// Bytes in the line, without CR.
    len: usize,
    /// The field name, lowercase. Only the first bytes: a longer name is
    /// not a name the scan reads.
    name: Vec<u8>,
    in_value: bool,
    field: Field,
    /// `Content-Length` (decimal) or the chunk size (hexadecimal).
    number: Number,
    /// The chunk size is complete; the rest is a chunk extension.
    in_extension: bool,
    /// The last bytes of a `Transfer-Encoding` value, lowercase, without
    /// spaces and tabs.
    tail: Vec<u8>,
}

/// A field this scan reads.
#[derive(Debug, Default, Clone, Copy)]
enum Field {
    #[default]
    Other,
    ContentLength,
    TransferEncoding,
}

/// A number read one digit at a time. A comma-separated list is one value
/// if all items are equal, as hyper reads `Content-Length`.
#[derive(Debug, Default)]
struct Number {
    item: Option<u64>,
    first: Option<u64>,
    bad: bool,
}

impl Number {
    fn push(&mut self, byte: u8, radix: u32) {
        match char::from(byte).to_digit(radix) {
            Some(digit) => {
                let next = self
                    .item
                    .unwrap_or(0)
                    .checked_mul(u64::from(radix))
                    .and_then(|v| v.checked_add(u64::from(digit)));
                self.bad |= next.is_none();
                self.item = next;
            }
            None if byte == b',' && radix == 10 => self.end_item(),
            None => self.bad = true,
        }
    }

    const fn end_item(&mut self) {
        match (self.item.take(), self.first) {
            (Some(item), None) => self.first = Some(item),
            (Some(item), Some(first)) if item == first => {}
            (Some(_), Some(_)) => self.bad = true,
            (None, _) => {}
        }
    }

    /// The value, or `None` when it is not a valid number.
    const fn finish(mut self) -> Option<u64> {
        self.end_item();
        if self.bad { None } else { self.first }
    }
}

/// Where the scan is inside the current head.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum HeadPosition {
    /// No byte of the head yet (CRLF between messages does not count).
    #[default]
    Before,
    RequestLine,
    Fields,
}

/// Field names longer than this are not names the scan reads.
const H1_NAME_KEEP: usize = 20;

#[derive(Debug, Default, Clone, Copy)]
enum H1State {
    #[default]
    Head,
    Body(u64),
    ChunkSize,
    ChunkData(u64),
    ChunkDataEnd(u8),
    Trailers,
    /// A value hyper rejects. Heads are not timed after it.
    Stopped,
}

impl H1Scan {
    fn scan(&mut self, mut bytes: &[u8], head_since: &mut Option<Instant>) {
        while !bytes.is_empty() {
            match self.state {
                H1State::Stopped => return,
                H1State::Body(n) | H1State::ChunkData(n) => {
                    let k = usize::try_from(n).unwrap_or(usize::MAX).min(bytes.len());
                    bytes = &bytes[k..];
                    let left = n - k as u64;
                    self.state = match (self.state, left) {
                        (H1State::Body(_), 0) => self.next_message(),
                        (H1State::Body(_), _) => H1State::Body(left),
                        (_, 0) => H1State::ChunkDataEnd(2),
                        _ => H1State::ChunkData(left),
                    };
                }
                H1State::ChunkDataEnd(n) => {
                    let k = usize::from(n).min(bytes.len());
                    bytes = &bytes[k..];
                    self.state = match n.saturating_sub(u8::try_from(k).unwrap_or(u8::MAX)) {
                        0 => H1State::ChunkSize,
                        left => H1State::ChunkDataEnd(left),
                    };
                }
                H1State::Head | H1State::ChunkSize | H1State::Trailers => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    if matches!(self.state, H1State::Head) && self.position == HeadPosition::Before
                    {
                        // hyper skips CRLF between messages.
                        if byte == b'\r' || byte == b'\n' {
                            continue;
                        }
                        self.position = HeadPosition::RequestLine;
                        head_since.get_or_insert_with(Instant::now);
                    }
                    if byte == b'\n' {
                        self.on_line(head_since);
                    } else if byte != b'\r' {
                        self.on_line_byte(byte);
                    }
                }
            }
        }
    }

    /// Stop the scan at a value hyper rejects. Fail open: the head timer
    /// does not close the connection.
    const fn stop(&mut self, head_since: &mut Option<Instant>) {
        self.state = H1State::Stopped;
        *head_since = None;
    }

    const fn next_message(&mut self) -> H1State {
        self.content_length = 0;
        self.chunked = false;
        self.position = HeadPosition::Before;
        H1State::Head
    }

    fn on_line_byte(&mut self, byte: u8) {
        let line = &mut self.line;
        line.len = line.len.saturating_add(1);
        let blank = byte == b' ' || byte == b'\t';
        match self.state {
            H1State::Head if self.position == HeadPosition::Fields => {
                if line.in_value {
                    match line.field {
                        // Optional whitespace can be long; values do not need it.
                        _ if blank => {}
                        Field::ContentLength => line.number.push(byte, 10),
                        Field::TransferEncoding => {
                            if line.tail.len() == b"chunked".len() {
                                line.tail.remove(0);
                            }
                            line.tail.push(byte.to_ascii_lowercase());
                        }
                        Field::Other => {}
                    }
                } else if byte == b':' {
                    line.in_value = true;
                    line.field = match line.name.as_slice() {
                        b"content-length" => Field::ContentLength,
                        b"transfer-encoding" => Field::TransferEncoding,
                        _ => Field::Other,
                    };
                } else if line.name.len() <= H1_NAME_KEEP {
                    line.name.push(byte.to_ascii_lowercase());
                }
            }
            H1State::ChunkSize if !line.in_extension && !blank => {
                if byte == b';' {
                    line.in_extension = true;
                } else {
                    line.number.push(byte, 16);
                }
            }
            _ => {}
        }
    }

    fn on_line(&mut self, head_since: &mut Option<Instant>) {
        let line = std::mem::take(&mut self.line);
        let empty = line.len == 0;
        match self.state {
            H1State::Head if empty => {
                *head_since = None;
                // An upgrade (`Upgrade`, `CONNECT`) needs no special case:
                // a rejected one stays HTTP/1, and an accepted one ends the
                // connection task, which closes the timers.
                self.state = if self.chunked {
                    H1State::ChunkSize
                } else if self.content_length > 0 {
                    H1State::Body(self.content_length)
                } else {
                    self.next_message()
                };
            }
            H1State::Head if self.position == HeadPosition::RequestLine => {
                self.position = HeadPosition::Fields;
            }
            H1State::Head => match line.field {
                Field::ContentLength => match line.number.finish() {
                    Some(len) => self.content_length = len,
                    // hyper rejects the head and closes the connection.
                    None => self.stop(head_since),
                },
                // `chunked` must be the last coding.
                Field::TransferEncoding => self.chunked = line.tail == b"chunked",
                Field::Other => {}
            },
            H1State::ChunkSize => match line.number.finish() {
                Some(0) => self.state = H1State::Trailers,
                Some(n) => self.state = H1State::ChunkData(n),
                None => self.stop(head_since),
            },
            H1State::Trailers if empty => self.state = self.next_message(),
            _ => {}
        }
    }
}

impl PhaseState {
    /// Advance the scan over `bytes`. Opens or closes the HTTP/2 header block.
    fn scan(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match &mut self.scan {
                Scan::Http1(h1) => {
                    h1.scan(bytes, &mut self.header_block_since);
                    return;
                }
                Scan::Preface { matched } => {
                    let rest = &H2_PREFACE[*matched..];
                    let n = rest.len().min(bytes.len());
                    if bytes[..n] != rest[..n] {
                        // HTTP/1: replay the bytes that matched the preface.
                        let mut h1 = H1Scan::default();
                        h1.scan(&H2_PREFACE[..*matched], &mut self.header_block_since);
                        h1.scan(bytes, &mut self.header_block_since);
                        self.scan = Scan::Http1(h1);
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
                            provisional: false,
                        };
                    }
                }
                Scan::Frames {
                    header,
                    have,
                    skip,
                    ends_block,
                    provisional,
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
                        // Until the type byte is in, the frame can be a head.
                        let head = *have < 4
                            || header[3] == H2_FRAME_HEADERS
                            || header[3] == H2_FRAME_CONTINUATION;
                        if head && self.header_block_since.is_none() {
                            self.header_block_since = Some(Instant::now());
                            *provisional = true;
                        } else if !head && std::mem::take(provisional) {
                            self.header_block_since = None;
                        }
                        return;
                    }
                    *have = 0;
                    *skip = usize::from(header[0]) << 16
                        | usize::from(header[1]) << 8
                        | usize::from(header[2]);
                    let (kind, flags) = (header[3], header[4]);
                    if kind == H2_FRAME_HEADERS || kind == H2_FRAME_CONTINUATION {
                        *provisional = false;
                        self.header_block_since.get_or_insert_with(Instant::now);
                        *ends_block = flags & H2_FLAG_END_HEADERS != 0;
                        if *ends_block && *skip == 0 {
                            *ends_block = false;
                            self.header_block_since = None;
                        }
                    } else if std::mem::take(provisional) {
                        self.header_block_since = None;
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct ConnTimers {
    /// Set when the connection task ends. After an upgrade the IO lives on
    /// (a WebSocket), but no timer applies, so reads skip the scan.
    closed: std::sync::atomic::AtomicBool,
    state: Mutex<PhaseState>,
    changed: Notify,
    header_read_timeout: Option<Duration>,
    keep_alive_timeout: Option<Duration>,
}

impl ConnTimers {
    fn new(limits: &HttpLimits) -> Self {
        Self {
            closed: std::sync::atomic::AtomicBool::new(false),
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
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let block_before = state.header_block_since.is_some();
        state.scan(bytes);
        let changed = block_before != state.header_block_since.is_some();
        drop(state);
        if changed {
            self.changed.notify_one();
        }
    }

    /// The connection task ended. Stop scanning reads.
    fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
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

/// The in-flight mark of an HTTP/2 `CONNECT` stream (a WebSocket, RFC 8441).
///
/// The tunnel stays open after its response, so the mark rides in the
/// request extensions instead of the response body. The WebSocket upgrade
/// keeps it until the socket closes. When no handler keeps it (for example a
/// `404`), it drops with the request.
#[derive(Clone)]
pub(crate) struct TunnelGuard {
    _mark: Arc<InFlight>,
}

/// Keeps an HTTP/2 WebSocket connection out of its idle timer.
///
/// Over HTTP/2, a WebSocket is a `CONNECT` stream. The server counts the
/// connection as busy while the request holds a tunnel mark. A handler on
/// axum's own `WebSocketUpgrade` drops that mark after the handshake, so
/// `keep_alive_timeout_ms` closes the connection while the socket is open.
/// Extract `KeepTunnel` and keep it for the life of the socket.
/// `autumn_web::ws::WebSocketUpgrade` does this for you.
///
/// It never rejects. On HTTP/1, or for a request that is not `CONNECT`, it
/// holds nothing.
///
/// ```rust,ignore
/// use autumn_web::http_server::KeepTunnel;
///
/// async fn raw(tunnel: KeepTunnel, ws: axum::extract::ws::WebSocketUpgrade)
///     -> axum::response::Response
/// {
///     ws.on_upgrade(move |mut socket| async move {
///         let _tunnel = tunnel; // drop it when the socket closes
///         while let Some(Ok(_)) = socket.recv().await {}
///     })
/// }
/// ```
pub struct KeepTunnel {
    guard: Option<TunnelGuard>,
}

impl std::fmt::Debug for KeepTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepTunnel")
            .field("holds_tunnel", &self.guard.is_some())
            .finish()
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for KeepTunnel {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Infallible> {
        Ok(Self {
            guard: parts.extensions.remove::<TunnelGuard>(),
        })
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

    fn call(&mut self, mut req: axum::extract::Request) -> Self::Future {
        let guard = self.timers.as_ref().map(|timers| {
            // A CONNECT stream gets two marks: one on the response body, as
            // for every request, and one for the tunnel, which can outlive
            // the response.
            if req.method() == axum::http::Method::CONNECT {
                timers.request_started();
                req.extensions_mut().insert(TunnelGuard {
                    _mark: Arc::new(InFlight(Arc::clone(timers))),
                });
            }
            timers.request_started();
            InFlight(Arc::clone(timers))
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
        assert!(matches!(state.scan, Scan::Http1(_)));
        assert!(!matches!(state.scan, Scan::Frames { .. }));
    }

    #[test]
    fn http1_head_opens_at_the_first_byte_and_closes_at_the_blank_line() {
        let mut state = state();
        state.scan(b"\r\n"); // a stray CRLF does not start a head
        assert!(state.header_block_since.is_none());
        state.scan(b"POST /x HTTP/1.1\r\nHost: t\r\n");
        assert!(state.header_block_since.is_some());
        state.scan(b"Content-Length: 12\r\n\r\n");
        assert!(state.header_block_since.is_none(), "head complete");
        state.scan(b"GET / HTTP/1"); // the 12-byte body, not a head
        assert!(state.header_block_since.is_none());
        state.scan(b"GET /next");
        assert!(state.header_block_since.is_some(), "a pipelined head");
    }

    #[test]
    fn http1_pipelined_partial_head_in_one_read_is_open() {
        let mut state = state();
        state.scan(b"GET / HTTP/1.1\r\nHost: t\r\n\r\nGET /2 HTTP/1.1\r\nHo");
        assert!(state.header_block_since.is_some());
    }

    #[test]
    fn http1_chunked_body_is_skipped() {
        let mut state = state();
        state.scan(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n");
        state.scan(b"5\r\nGET /\r\n0\r\n\r\n");
        assert!(
            state.header_block_since.is_none(),
            "chunk data is not a head"
        );
        state.scan(b"G");
        assert!(state.header_block_since.is_some());
    }

    #[test]
    fn http1_long_whitespace_in_a_field_keeps_the_length() {
        let mut state = state();
        let ows = " ".repeat(300);
        state.scan(format!("POST / HTTP/1.1\r\nContent-Length:{ows}5\r\n\r\n").as_bytes());
        assert!(state.header_block_since.is_none(), "head complete");
        state.scan(b"GET /"); // the 5-byte body, not a head
        assert!(state.header_block_since.is_none());
        state.scan(b"G");
        assert!(state.header_block_since.is_some(), "the next head is timed");
    }

    /// Scan `head`, then `body`, then one byte of the next head. The head
    /// must close, the body must not open a head, and the next head must.
    fn assert_frames(head: &[u8], body: &[u8]) {
        let mut state = state();
        state.scan(head);
        assert!(state.header_block_since.is_none(), "head complete");
        state.scan(body);
        assert!(state.header_block_since.is_none(), "the body is not a head");
        state.scan(b"G");
        assert!(state.header_block_since.is_some(), "the next head is timed");
    }

    #[test]
    fn http1_long_valid_framing_values_still_frame() {
        let zeros = "0".repeat(300);
        assert_frames(
            format!("POST / HTTP/1.1\r\nContent-Length: {zeros}5\r\n\r\n").as_bytes(),
            b"GET /",
        );
        let codings = "gzip, ".repeat(60);
        assert_frames(
            format!("POST / HTTP/1.1\r\nTransfer-Encoding: {codings}chunked\r\n\r\n").as_bytes(),
            b"5\r\nGET /\r\n0\r\n\r\n",
        );
        let ext = "x".repeat(300);
        assert_frames(
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            format!("5;{ext}\r\nGET /\r\n0\r\n\r\n").as_bytes(),
        );
        assert_frames(b"POST / HTTP/1.1\r\nContent-Length: 5, 5\r\n\r\n", b"GET /");
    }

    #[test]
    fn http1_chunked_must_be_the_last_coding() {
        // hyper rejects this head; the body is not chunked.
        let mut scan = H1Scan::default();
        let mut since = None;
        scan.scan(
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
            &mut since,
        );
        assert!(!scan.chunked);
    }

    #[test]
    fn http1_a_value_the_scan_cannot_read_stops_head_timing() {
        let mut state = state();
        state.scan(b"POST / HTTP/1.1\r\nContent-Length: abc\r\n");
        // Fail open: hyper rejects this head and closes the connection.
        assert!(state.header_block_since.is_none());
        state.scan(b"\r\nGET /");
        assert!(state.header_block_since.is_none());
    }

    #[test]
    fn a_rejected_http1_upgrade_keeps_the_scan() {
        let mut state = state();
        state.scan(b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\n\r\n");
        // The server answered 400, so the next bytes are a new request.
        state.scan(b"GET /next");
        assert!(state.header_block_since.is_some(), "the next head is timed");
    }

    #[test]
    fn a_closed_connection_stops_reading_bytes() {
        let timers = ConnTimers::new(&HttpLimits {
            header_read_timeout: Some(Duration::from_secs(10)),
            ..HttpLimits::default()
        });
        timers.request_started();
        timers.close();
        // After an upgrade, WebSocket frames are not scanned.
        timers.on_read(b"GET /not-a-head");
        assert!(
            timers.state.lock().unwrap().header_block_since.is_none(),
            "a closed connection ignores the bytes"
        );
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
        assert!(matches!(state.scan, Scan::Frames { .. }));
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

    fn connect_request() -> axum::extract::Request {
        axum::http::Request::builder()
            .method(axum::http::Method::CONNECT)
            .uri("/ws")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn idle_timers() -> Arc<ConnTimers> {
        let timers = Arc::new(ConnTimers::new(&HttpLimits {
            keep_alive_timeout: Some(Duration::from_secs(60)),
            ..HttpLimits::default()
        }));
        timers.request_started();
        timers.request_finished();
        timers
    }

    fn expiry(timers: &ConnTimers) -> Option<Expiry> {
        timers.deadline_and_expiry().map(|(_, expiry)| expiry)
    }

    #[tokio::test]
    async fn a_rejected_connect_releases_its_in_flight_mark() {
        use tower::Service as _;
        let timers = idle_timers();
        let mut service = Tracked {
            inner: tower::service_fn(|_req: axum::extract::Request| async {
                Ok::<_, Infallible>(
                    axum::response::Response::builder()
                        .status(404)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
            }),
            timers: Some(Arc::clone(&timers)),
        };
        drop(service.call(connect_request()).await.unwrap());
        assert_eq!(expiry(&timers), Some(Expiry::Idle));
    }

    #[tokio::test]
    async fn a_kept_tunnel_guard_holds_the_connection_until_dropped() {
        use tower::Service as _;
        let timers = idle_timers();
        let (tx, rx) = std::sync::mpsc::channel::<TunnelGuard>();
        let mut service = Tracked {
            inner: tower::service_fn(move |mut req: axum::extract::Request| {
                // What the WebSocket upgrade does: keep the guard.
                let guard = req.extensions_mut().remove::<TunnelGuard>();
                tx.send(guard.expect("CONNECT carries a guard")).unwrap();
                async {
                    Ok::<_, Infallible>(axum::response::Response::new(axum::body::Body::empty()))
                }
            }),
            timers: Some(Arc::clone(&timers)),
        };
        drop(service.call(connect_request()).await.unwrap());
        let guard = rx.recv().unwrap();
        assert_eq!(expiry(&timers), None, "the tunnel is still open");
        drop(guard);
        assert_eq!(expiry(&timers), Some(Expiry::Idle));
    }

    #[tokio::test]
    async fn keep_tunnel_holds_the_connection_for_a_raw_upgrade() {
        use axum::extract::FromRequestParts as _;
        use tower::Service as _;
        let timers = idle_timers();
        let (tx, rx) = std::sync::mpsc::channel::<KeepTunnel>();
        let mut service = Tracked {
            inner: tower::service_fn(move |req: axum::extract::Request| {
                let tx = tx.clone();
                async move {
                    let (mut parts, _) = req.into_parts();
                    let keep = KeepTunnel::from_request_parts(&mut parts, &())
                        .await
                        .unwrap();
                    tx.send(keep).unwrap();
                    Ok::<_, Infallible>(axum::response::Response::new(axum::body::Body::empty()))
                }
            }),
            timers: Some(Arc::clone(&timers)),
        };
        drop(service.call(connect_request()).await.unwrap());
        let keep = rx.recv().unwrap();
        assert_eq!(expiry(&timers), None, "the raw tunnel is still open");
        drop(keep);
        assert_eq!(expiry(&timers), Some(Expiry::Idle));
    }

    #[tokio::test]
    async fn a_connect_response_body_stays_in_flight() {
        use tower::Service as _;
        let timers = idle_timers();
        let mut service = Tracked {
            // A CONNECT handler that is not a WebSocket: it drops the guard
            // and streams a body.
            inner: tower::service_fn(|_req: axum::extract::Request| async {
                Ok::<_, Infallible>(axum::response::Response::new(axum::body::Body::from(
                    "tunnel bytes",
                )))
            }),
            timers: Some(Arc::clone(&timers)),
        };
        let response = service.call(connect_request()).await.unwrap();
        assert_eq!(expiry(&timers), None, "the response body is still open");
        drop(response);
        assert_eq!(expiry(&timers), Some(Expiry::Idle));
    }

    #[tokio::test]
    async fn a_partial_frame_header_starts_the_head_timer() {
        let mut state = state();
        state.scan(H2_PREFACE);
        let headers = frame(H2_FRAME_HEADERS, 0, &[0x82]);
        state.scan(&headers[..3]);
        let since = state
            .header_block_since
            .expect("timer starts at the first byte");
        state.scan(&headers[3..]);
        assert_eq!(
            state.header_block_since,
            Some(since),
            "a HEADERS frame keeps the time of its first byte"
        );

        let mut state = state_after_preface();
        let ping = frame(0x6, 0, &[0; 8]);
        state.scan(&ping[..3]); // the type byte is not in yet
        assert!(state.header_block_since.is_some());
        state.scan(&ping[3..]);
        assert!(
            state.header_block_since.is_none(),
            "control traffic cancels the timer"
        );
    }

    #[test]
    fn a_known_control_frame_type_cancels_the_provisional_timer() {
        let mut state = state_after_preface();
        let ping = frame(0x6, 0, &[0; 8]); // PING
        state.scan(&ping[..4]); // the type byte is in; the flags are not
        assert!(
            state.header_block_since.is_none(),
            "a PING is not a head, so no head timer"
        );
        let mut state = state_after_preface();
        let headers = frame(H2_FRAME_HEADERS, H2_FLAG_END_HEADERS, &[0x82]);
        state.scan(&headers[..4]);
        assert!(
            state.header_block_since.is_some(),
            "a HEADERS frame is a head"
        );
    }

    fn state_after_preface() -> PhaseState {
        let mut state = state();
        state.scan(H2_PREFACE);
        state
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
