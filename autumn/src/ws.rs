//! WebSocket support for Autumn applications.
//!
//! This module provides ergonomic WebSocket handling through the [`#[ws]`](macro@crate::ws)
//! macro, Autumn wrappers over Axum's WebSocket types, and re-exports of
//! `Message`, `CloseCode`, `CloseFrame` and `Utf8Bytes`.
//!
//! # Two-function pattern
//!
//! WebSocket handlers in Autumn use a **two-function pattern**: the outer
//! function runs at HTTP upgrade time (before the WebSocket connection is
//! established) and returns a closure that handles the live socket.
//!
//! This split gives you:
//! - **Pre-upgrade access** to Axum extractors (auth, session, state)
//! - **Post-upgrade ownership** of the `WebSocket` + captured values
//! - A natural place for connection rejection (return an error before upgrade)
//!
//! # Limits
//!
//! [`WebSocketUpgrade`] and [`WebSocket`] are Autumn wrappers over the axum
//! types. They apply the `[realtime]` limits (issue #3065): a connection cap,
//! a message size limit (close code `1009`), server pings and an idle timeout
//! (close code `1001`). See [`crate::config::RealtimeConfig`] and
//! `docs/guide/websockets.md`.
//!
//! # Examples
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::ws::{WebSocket, Message};
//!
//! // Simple echo server
//! #[ws("/echo")]
//! async fn echo() -> impl WsHandler {
//!     |mut socket: WebSocket| async move {
//!         while let Some(Ok(msg)) = socket.recv().await {
//!             if let Message::Text(text) = msg {
//!                 socket.send(Message::Text(text)).await.ok();
//!             }
//!         }
//!     }
//! }
//!
//! // With state and graceful shutdown
//! #[ws("/chat")]
//! async fn chat(state: AppState) -> impl WsHandler {
//!     let channels = state.channels();
//!     let tx = channels.sender("lobby");
//!     let mut rx = channels.subscribe("lobby");
//!
//!     |mut socket: WebSocket, shutdown: CancellationToken| async move {
//!         loop {
//!             tokio::select! {
//!                 Some(Ok(Message::Text(text))) = socket.recv() => {
//!                     tx.send(text.to_string()).ok();
//!                 }
//!                 Ok(msg) = rx.recv() => {
//!                     socket.send(Message::Text(msg.into())).await.ok();
//!                 }
//!                 _ = shutdown.cancelled() => {
//!                     socket.send(Message::Close(None)).await.ok();
//!                     break;
//!                 }
//!             }
//!         }
//!     }
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::ws::close_code;
use axum::response::IntoResponse;

pub use axum::extract::ws::{CloseCode, CloseFrame, Message, Utf8Bytes};
pub use tokio_util::sync::CancellationToken;

/// Trait for WebSocket connection handlers.
///
/// Implemented automatically for closures matching the supported signatures.
/// Users never implement this trait directly — they return closures from
/// `#[ws]` handler functions.
///
/// # Supported signatures
///
/// ```rust,ignore
/// // Minimal: just the socket
/// |socket: WebSocket| async move { /* ... */ }
///
/// // With shutdown signal
/// |socket: WebSocket, shutdown: CancellationToken| async move { /* ... */ }
/// ```
pub trait WsHandler: Send + 'static {
    /// Handle an upgraded WebSocket connection.
    fn handle(
        self,
        socket: WebSocket,
        shutdown: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

// ── Blanket impl: closure taking (WebSocket) ───────────────────────

impl<F, Fut> WsHandler for F
where
    F: FnOnce(WebSocket) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn handle(
        self,
        socket: WebSocket,
        _shutdown: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self)(socket))
    }
}

// NOTE: We cannot have a second blanket impl for `FnOnce(WebSocket, CancellationToken)`
// because it conflicts with the above. Instead, we provide a newtype wrapper.

/// Wrapper that enables `|socket, shutdown|` closures as [`WsHandler`].
///
/// Users don't construct this directly. The `#[ws]` macro detects the
/// `CancellationToken` parameter in the closure and wraps it automatically.
/// For manual usage:
///
/// ```rust,ignore
/// use autumn_web::ws::{WithShutdown, WebSocket, CancellationToken};
///
/// let handler = WithShutdown(|socket: WebSocket, shutdown: CancellationToken| async move {
///     // ...
/// });
/// ```
pub struct WithShutdown<F>(pub F);

impl<F, Fut> WsHandler for WithShutdown<F>
where
    F: FnOnce(WebSocket, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn handle(
        self,
        socket: WebSocket,
        shutdown: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self.0)(socket, shutdown))
    }
}

// ── [realtime] limits (issue #3065) ───────────────────────────────

/// Payload of the pings the server sends. Pongs with this payload are not
/// given to the handler.
const PING_PAYLOAD: &[u8] = b"autumn-ping";

/// The limits for one socket, from `[realtime]`.
#[derive(Debug, Clone, Copy, Default)]
struct SocketLimits {
    max_message_bytes: Option<usize>,
    ping_interval: Option<Duration>,
    idle_timeout: Option<Duration>,
}

impl SocketLimits {
    fn from_config(config: &crate::config::RealtimeConfig) -> Self {
        let ms = |value: Option<u64>| value.filter(|v| *v > 0).map(Duration::from_millis);
        Self {
            max_message_bytes: config.max_message_bytes,
            ping_interval: ms(config.ping_interval_ms),
            idle_timeout: ms(config.idle_timeout_ms),
        }
    }
}

/// Count of open sockets, shared by all upgrades of one app.
#[derive(Debug, Default)]
struct OpenSockets(Arc<AtomicUsize>);

/// A slot under `realtime.max_connections`. Dropping it frees the slot.
#[derive(Debug)]
struct SocketPermit(Arc<AtomicUsize>);

impl OpenSockets {
    fn try_acquire(&self, max: usize) -> Option<SocketPermit> {
        let mut open = self.0.load(Ordering::Acquire);
        while open < max {
            match self
                .0
                .compare_exchange_weak(open, open + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(SocketPermit(Arc::clone(&self.0))),
                Err(current) => open = current,
            }
        }
        None
    }
}

impl Drop for SocketPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// WebSocket upgrade extractor that applies the `[realtime]` limits.
///
/// The `#[ws]` macro uses this type. It wraps axum's
/// [`WebSocketUpgrade`](axum::extract::ws::WebSocketUpgrade):
///
/// - Above `realtime.max_connections`, extraction fails with `503` and
///   `Retry-After: 1`. The upgrade does not occur.
/// - [`on_upgrade`](Self::on_upgrade) gives the handler a [`WebSocket`] that
///   applies the message size, ping and idle limits.
pub struct WebSocketUpgrade {
    inner: axum::extract::ws::WebSocketUpgrade,
    limits: SocketLimits,
    permit: Option<SocketPermit>,
    /// Keeps an HTTP/2 connection out of its idle timer while the socket is
    /// open.
    tunnel: Option<crate::http_server::TunnelGuard>,
}

impl std::fmt::Debug for WebSocketUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgrade")
            .field("inner", &self.inner)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl<S> axum::extract::FromRequestParts<S> for WebSocketUpgrade
where
    S: Send + Sync,
    crate::AppState: axum::extract::FromRef<S>,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        use axum::response::IntoResponse as _;

        let inner = axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let app: crate::AppState = axum::extract::FromRef::from_ref(state);
        let config = app.config_arc();
        let realtime = &config.realtime;
        let permit = match realtime.max_connections.filter(|max| *max > 0) {
            Some(max) => {
                let open = app.extension_or_insert_with(OpenSockets::default);
                let Some(permit) = open.try_acquire(max) else {
                    let mut response = crate::AutumnError::service_unavailable_msg(
                        "too many WebSocket connections",
                    )
                    .into_response();
                    response.headers_mut().insert(
                        axum::http::header::RETRY_AFTER,
                        axum::http::HeaderValue::from_static("1"),
                    );
                    return Err(response);
                };
                Some(permit)
            }
            None => None,
        };
        Ok(Self {
            inner,
            limits: SocketLimits::from_config(realtime),
            permit,
            tunnel: parts.extensions.remove::<crate::http_server::TunnelGuard>(),
        })
    }
}

impl WebSocketUpgrade {
    /// Set the maximum received message size for this socket. Overrides
    /// `realtime.max_message_bytes`.
    #[must_use]
    pub const fn max_message_size(mut self, bytes: usize) -> Self {
        self.limits.max_message_bytes = Some(bytes);
        self
    }

    /// Set the maximum received frame size. See axum's
    /// [`max_frame_size`](axum::extract::ws::WebSocketUpgrade::max_frame_size).
    #[must_use]
    pub fn max_frame_size(mut self, bytes: usize) -> Self {
        self.inner = self.inner.max_frame_size(bytes);
        self
    }

    /// Set the subprotocols the server supports. See axum's
    /// [`protocols`](axum::extract::ws::WebSocketUpgrade::protocols).
    #[must_use]
    pub fn protocols<I>(mut self, protocols: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<std::borrow::Cow<'static, str>>,
    {
        self.inner = self.inner.protocols(protocols);
        self
    }

    /// The subprotocol the server selected, if any.
    #[must_use]
    pub fn selected_protocol(&self) -> Option<&axum::http::HeaderValue> {
        self.inner.selected_protocol()
    }

    /// See axum's
    /// [`read_buffer_size`](axum::extract::ws::WebSocketUpgrade::read_buffer_size).
    #[must_use]
    pub fn read_buffer_size(mut self, size: usize) -> Self {
        self.inner = self.inner.read_buffer_size(size);
        self
    }

    /// See axum's
    /// [`write_buffer_size`](axum::extract::ws::WebSocketUpgrade::write_buffer_size).
    #[must_use]
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.inner = self.inner.write_buffer_size(size);
        self
    }

    /// See axum's
    /// [`max_write_buffer_size`](axum::extract::ws::WebSocketUpgrade::max_write_buffer_size).
    #[must_use]
    pub fn max_write_buffer_size(mut self, max: usize) -> Self {
        self.inner = self.inner.max_write_buffer_size(max);
        self
    }

    /// See axum's
    /// [`accept_unmasked_frames`](axum::extract::ws::WebSocketUpgrade::accept_unmasked_frames).
    #[must_use]
    pub fn accept_unmasked_frames(mut self, accept: bool) -> Self {
        self.inner = self.inner.accept_unmasked_frames(accept);
        self
    }

    /// Finish the upgrade and run `callback` with the limited socket.
    pub fn on_upgrade<C, Fut>(self, callback: C) -> axum::response::Response
    where
        C: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Self {
            inner,
            limits,
            permit,
            tunnel,
        } = self;
        let inner = match limits.max_message_bytes {
            Some(bytes) => inner.max_message_size(bytes),
            None => inner,
        };
        let hold = ConnectionHold {
            _permit: permit,
            _tunnel: tunnel,
        };
        inner.on_upgrade(move |socket| callback(WebSocket::new(socket, limits, hold)))
    }

    /// The axum upgrade, without the `[realtime]` limits. The connection slot
    /// is released. To keep it, use [`into_parts`](Self::into_parts).
    pub fn into_inner(self) -> axum::extract::ws::WebSocketUpgrade {
        self.inner
    }

    /// The axum upgrade, without the `[realtime]` limits, and the
    /// [`ConnectionHold`]. Keep the hold for the life of the socket, for
    /// example by moving it into the `on_upgrade` callback.
    pub fn into_parts(self) -> (axum::extract::ws::WebSocketUpgrade, ConnectionHold) {
        let hold = ConnectionHold {
            _permit: self.permit,
            _tunnel: self.tunnel,
        };
        (self.inner, hold)
    }
}

/// The resources an open WebSocket holds: its `realtime.max_connections`
/// slot and, on HTTP/2, the mark that keeps the connection out of its idle
/// timer. Dropping the hold releases both.
pub struct ConnectionHold {
    _permit: Option<SocketPermit>,
    _tunnel: Option<crate::http_server::TunnelGuard>,
}

impl std::fmt::Debug for ConnectionHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionHold").finish_non_exhaustive()
    }
}

/// What the stream yields after the close frame is sent.
enum AfterClose {
    Error(axum::Error),
    /// The peer's close frame. The reply is already queued.
    Message(Message),
    End,
}

/// A close frame waiting to be sent (or, for a peer close, only flushed).
struct Closing {
    frame: Option<Message>,
    then: Option<AfterClose>,
    /// A peer that does not read cannot block the close forever.
    deadline: Pin<Box<tokio::time::Sleep>>,
}

/// Maximum time to send a close frame the server starts.
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// A WebSocket that applies the `[realtime]` limits.
///
/// It has the same `recv` / `send` / `close` methods as axum's
/// [`WebSocket`](axum::extract::ws::WebSocket), and implements `Stream` and
/// `Sink` the same way, so `split()` works.
///
/// - A message over `max_message_bytes` sends close code `1009`. The stream
///   then yields the error and ends. Any other receive error also ends it,
///   and so does a send error.
/// - Every `ping_interval_ms` the socket sends a ping while the handler
///   reads. The pong is not given to the handler.
/// - When no complete message arrives for `idle_timeout_ms`, the socket
///   sends close code `1001` and the stream ends. Pings, pongs and close
///   frames count. The fragments of one message count only when the message
///   is complete, so a client that sends a long message slowly must also send
///   pings.
///
/// The timers run only while the handler reads (`recv` or `next`). A
/// handler that never reads gets no pings and no idle timeout.
///
/// When the stream ends, the socket drops its transport and its connection
/// slot, also when a `split()` sink half lives on. A later send fails.
pub struct WebSocket {
    /// `None` after the stream ends: the transport is dropped.
    inner: Option<axum::extract::ws::WebSocket>,
    ping: Option<(Duration, Pin<Box<tokio::time::Sleep>>)>,
    idle: Option<(Duration, Pin<Box<tokio::time::Sleep>>)>,
    ping_due: bool,
    flush_due: bool,
    closing: Option<Closing>,
    done: bool,
    /// The waker of a `Sink` call that returned `Pending`. The read side
    /// sends pings and close frames with its own waker, which replaces this
    /// one inside tungstenite. So after each such send, the read side wakes
    /// it. Without this, a `split()` writer task can wait forever.
    writer_waker: Option<std::task::Waker>,
    /// The waker of a parked `poll_next`. A sink-side close ends the socket,
    /// and the reader must then see the end.
    reader_waker: Option<std::task::Waker>,
    /// Set when the handler sends a Close frame (or calls `close()`). When
    /// that flush ends the socket is done, also when the peer never answers.
    /// The deadline bounds the flush: a peer that does not read cannot keep
    /// the socket.
    close_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Released when the socket drops.
    hold: ConnectionHold,
}

impl std::fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket").finish_non_exhaustive()
    }
}

fn timer(period: Duration) -> (Duration, Pin<Box<tokio::time::Sleep>>) {
    (period, Box::pin(tokio::time::sleep(period)))
}

const fn close_frame(code: CloseCode, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: Utf8Bytes::from_static(reason),
    }))
}

/// tungstenite reports an oversized message as a capacity error. axum hides
/// the error type, so match its text: "Space limit exceeded: Message too
/// long: N > M". The 1009 integration test guards this match.
fn is_message_too_big(error: &axum::Error) -> bool {
    error.to_string().contains("Message too long")
}

impl WebSocket {
    fn new(
        inner: axum::extract::ws::WebSocket,
        limits: SocketLimits,
        hold: ConnectionHold,
    ) -> Self {
        Self {
            inner: Some(inner),
            ping: limits.ping_interval.map(timer),
            idle: limits.idle_timeout.map(timer),
            ping_due: false,
            flush_due: false,
            closing: None,
            done: false,
            writer_waker: None,
            reader_waker: None,
            close_deadline: None,
            hold,
        }
    }

    /// Receive the next message. `None` when the socket is closed.
    pub async fn recv(&mut self) -> Option<Result<Message, axum::Error>> {
        futures::StreamExt::next(self).await
    }

    /// Send a message.
    ///
    /// # Errors
    ///
    /// Returns the transport error when the send fails.
    pub async fn send(&mut self, msg: Message) -> Result<(), axum::Error> {
        futures::SinkExt::send(self, msg).await
    }

    /// Close the socket.
    ///
    /// # Errors
    ///
    /// Returns the transport error when the close fails.
    pub async fn close(mut self) -> Result<(), axum::Error> {
        futures::SinkExt::close(&mut self).await
    }

    /// The subprotocol the server selected, if any.
    #[must_use]
    pub fn protocol(&self) -> Option<&axum::http::HeaderValue> {
        self.inner.as_ref().and_then(|socket| socket.protocol())
    }

    /// The axum socket, without the `[realtime]` limits. The connection slot
    /// is released. To keep it, use [`into_parts`](Self::into_parts).
    ///
    /// # Panics
    ///
    /// Panics after the stream ended. The socket is then closed.
    #[must_use]
    pub fn into_inner(self) -> axum::extract::ws::WebSocket {
        self.inner.expect("the WebSocket is closed")
    }

    /// The axum socket, without the `[realtime]` limits, and the
    /// [`ConnectionHold`]. Keep the hold for the life of the socket.
    ///
    /// # Panics
    ///
    /// Panics after the stream ended. The socket is then closed.
    #[must_use]
    pub fn into_parts(self) -> (axum::extract::ws::WebSocket, ConnectionHold) {
        (self.inner.expect("the WebSocket is closed"), self.hold)
    }

    /// The axum socket. An error after the stream ended.
    fn socket(&mut self) -> Result<&mut axum::extract::ws::WebSocket, axum::Error> {
        self.inner.as_mut().ok_or_else(|| {
            axum::Error::new(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the WebSocket is closed",
            ))
        })
    }

    /// The stream ended. Drop the transport and release the hold, also when
    /// a `split()` sink half lives on. A waiting writer gets an error.
    fn finish(&mut self) {
        self.done = true;
        self.inner = None;
        self.hold = ConnectionHold {
            _permit: None,
            _tunnel: None,
        };
        self.wake_writer();
        if let Some(waker) = self.reader_waker.take() {
            waker.wake();
        }
    }

    fn start_close(&mut self, frame: Option<Message>, then: AfterClose) {
        self.closing = Some(Closing {
            frame,
            then: Some(then),
            deadline: Box::pin(tokio::time::sleep(CLOSE_FLUSH_TIMEOUT)),
        });
    }

    fn wake_writer(&mut self) {
        if let Some(waker) = self.writer_waker.take() {
            waker.wake();
        }
    }

    /// A Close frame the handler sent is being flushed. When the flush ends
    /// (or fails, or takes over [`CLOSE_FLUSH_TIMEOUT`]), the socket is done:
    /// release it, also while a `split()` half lives on.
    fn settle_close(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<Result<(), axum::Error>>,
    ) -> Poll<Result<(), axum::Error>> {
        if poll.is_ready() {
            self.finish();
            return poll;
        }
        let deadline = self
            .close_deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(CLOSE_FLUSH_TIMEOUT)));
        if deadline.as_mut().poll(cx).is_ready() {
            self.finish();
            return Poll::Ready(Ok(()));
        }
        self.note_writer(cx, poll)
    }

    /// A send error ends the socket (tungstenite does not recover), so
    /// release it, also while the handler keeps the sink and nothing polls
    /// the stream.
    fn settle_send_error(
        &mut self,
        poll: Poll<Result<(), axum::Error>>,
    ) -> Poll<Result<(), axum::Error>> {
        if matches!(poll, Poll::Ready(Err(_))) {
            self.finish();
        }
        poll
    }

    /// Keep the waker of a `Sink` call that returned `Pending`.
    fn note_writer<T>(&mut self, cx: &Context<'_>, poll: Poll<T>) -> Poll<T> {
        if poll.is_pending() {
            self.writer_waker = Some(cx.waker().clone());
        }
        poll
    }

    /// Send the pending close frame. `Ready` when it is sent or failed.
    fn poll_close_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Message, axum::Error>>> {
        use futures::Sink as _;
        let (Some(closing), Some(inner)) = (self.closing.as_mut(), self.inner.as_mut()) else {
            self.finish();
            return Poll::Ready(None);
        };
        let mut sent = true;
        if let Some(frame) = closing.frame.take() {
            match Pin::new(&mut *inner).poll_ready(cx) {
                Poll::Pending => {
                    closing.frame = Some(frame);
                    sent = false;
                }
                Poll::Ready(Ok(())) => {
                    let _ = Pin::new(&mut *inner).start_send(frame);
                }
                Poll::Ready(Err(_)) => {}
            }
        }
        let flushed = sent && Pin::new(inner).poll_flush(cx).is_ready();
        let timed_out = !flushed && closing.deadline.as_mut().poll(cx).is_ready();
        let then = if flushed || timed_out {
            closing.then.take()
        } else {
            self.wake_writer();
            return Poll::Pending;
        };
        self.finish();
        Poll::Ready(match then {
            Some(AfterClose::Error(error)) => Some(Err(error)),
            Some(AfterClose::Message(msg)) => Some(Ok(msg)),
            Some(AfterClose::End) | None => None,
        })
    }

    /// Send a due ping and flush it. A send or flush error is returned: the
    /// transport failed, so the caller ends the socket, also while the read
    /// side waits for a message that cannot come.
    fn poll_ping(&mut self, cx: &mut Context<'_>) -> Option<axum::Error> {
        use futures::Sink as _;
        let inner = self.inner.as_mut()?;
        if self.ping_due {
            match Pin::new(&mut *inner).poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    let ping = Message::Ping(axum::body::Bytes::from_static(PING_PAYLOAD));
                    if let Err(error) = Pin::new(&mut *inner).start_send(ping) {
                        return Some(error);
                    }
                    self.flush_due = true;
                    self.ping_due = false;
                }
                Poll::Ready(Err(error)) => return Some(error),
                Poll::Pending => {}
            }
        }
        if self.flush_due {
            match Pin::new(inner).poll_flush(cx) {
                Poll::Ready(Ok(())) => self.flush_due = false,
                Poll::Ready(Err(error)) => return Some(error),
                Poll::Pending => {}
            }
        }
        self.wake_writer();
        None
    }
}

impl futures::Stream for WebSocket {
    type Item = Result<Message, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            if this.closing.is_some() {
                return this.poll_close_frame(cx);
            }
            let Some(inner) = this.inner.as_mut() else {
                this.done = true;
                return Poll::Ready(None);
            };
            match Pin::new(inner).poll_next(cx) {
                Poll::Ready(Some(Ok(msg))) => {
                    if let Some((period, sleep)) = this.idle.as_mut() {
                        sleep.as_mut().reset(tokio::time::Instant::now() + *period);
                    }
                    if matches!(&msg, Message::Pong(payload) if payload.as_ref() == PING_PAYLOAD) {
                        continue;
                    }
                    if matches!(msg, Message::Close(_)) {
                        // A peer close ends the socket. Flush the reply that
                        // tungstenite queued, release the socket, then yield
                        // the frame: a handler that stops here leaks nothing.
                        this.start_close(None, AfterClose::Message(msg));
                        continue;
                    }
                    return Poll::Ready(Some(Ok(msg)));
                }
                Poll::Ready(Some(Err(error))) if is_message_too_big(&error) => {
                    this.start_close(
                        Some(close_frame(close_code::SIZE, "message too big")),
                        AfterClose::Error(error),
                    );
                    continue;
                }
                // A receive error ends the socket (tungstenite does not
                // recover), so release it before the error is yielded.
                Poll::Ready(Some(Err(error))) => {
                    this.finish();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    this.finish();
                    return Poll::Ready(None);
                }
                Poll::Pending => {}
            }
            if let Some((_, sleep)) = this.idle.as_mut()
                && sleep.as_mut().poll(cx).is_ready()
            {
                this.start_close(
                    Some(close_frame(close_code::AWAY, "idle timeout")),
                    AfterClose::End,
                );
                continue;
            }
            if let Some((period, sleep)) = this.ping.as_mut()
                && sleep.as_mut().poll(cx).is_ready()
            {
                sleep.as_mut().reset(tokio::time::Instant::now() + *period);
                this.ping_due = true;
                // Poll the reset timer, so it wakes this task again.
                let _ = sleep.as_mut().poll(cx);
            }
            if let Some(error) = this.poll_ping(cx) {
                this.finish();
                return Poll::Ready(Some(Err(error)));
            }
            this.reader_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
    }
}

impl futures::Sink<Message> for WebSocket {
    type Error = axum::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let poll = match self.socket() {
            Ok(socket) => Pin::new(socket).poll_ready(cx),
            Err(error) => return Poll::Ready(Err(error)),
        };
        let poll = self.settle_send_error(poll);
        self.note_writer(cx, poll)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        let close = matches!(item, Message::Close(_));
        let sent = Pin::new(self.socket()?).start_send(item);
        if sent.is_err() {
            self.finish();
        }
        sent?;
        if close && self.close_deadline.is_none() {
            self.close_deadline = Some(Box::pin(tokio::time::sleep(CLOSE_FLUSH_TIMEOUT)));
        }
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let poll = match self.socket() {
            Ok(socket) => Pin::new(socket).poll_flush(cx),
            Err(error) => return Poll::Ready(Err(error)),
        };
        if self.close_deadline.is_some() {
            return self.settle_close(cx, poll);
        }
        let poll = self.settle_send_error(poll);
        self.note_writer(cx, poll)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // A socket that the stream already closed is closed.
        let poll = match self.inner.as_mut() {
            Some(socket) => Pin::new(socket).poll_close(cx),
            None => return Poll::Ready(Ok(())),
        };
        self.settle_close(cx, poll)
    }
}

impl futures::stream::FusedStream for WebSocket {
    fn is_terminated(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_handler_is_object_safe_enough() {
        // Verify the trait can be used as a generic bound
        fn accept_handler<H: WsHandler>(_h: H) {}

        let handler = |_socket: WebSocket| async {};
        accept_handler(handler);
    }

    #[test]
    fn with_shutdown_compiles() {
        fn accept_handler<H: WsHandler>(_h: H) {}

        let handler = WithShutdown(|_socket: WebSocket, _shutdown: CancellationToken| async {});
        accept_handler(handler);
    }
}
