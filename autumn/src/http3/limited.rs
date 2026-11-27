//! The QUIC transport that `h3` reads from, with a byte limit on each stream.
//!
//! `h3` buffers a whole HEADERS frame before it checks
//! `max_field_section_size`. A client can declare a very large frame and send
//! data into it. This wrapper counts the bytes that `h3` reads:
//!
//! - On a request stream, until the server releases the stream (the request
//!   head is parsed). Then the body is not limited here.
//! - On a unidirectional stream (control, QPACK), for the life of the stream.
//!   These streams carry a few small frames.
//!
//! When a stream goes over its limit, the wrapper stops the stream and gives
//! `h3` an error. On the control stream, that closes the connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use bytes::Buf;
use h3::quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf};

/// The byte limit on a unidirectional stream.
pub(super) const UNI_STREAM_LIMIT: usize = 64 * 1024;

/// `H3_EXCESSIVE_LOAD`, sent in `STOP_SENDING` when a stream goes over its limit.
const H3_EXCESSIVE_LOAD: u64 = 0x0107;

/// A stream went over its byte limit.
#[derive(Debug, thiserror::Error)]
#[error("the HTTP/3 stream sent more than {limit} bytes before its request head ended")]
struct OverLimit {
    limit: usize,
}

/// The request streams of one connection whose head is not parsed yet.
#[derive(Debug)]
pub(super) struct HeadBudgets {
    limit: usize,
    open: Mutex<HashMap<StreamId, Arc<AtomicBool>>>,
}

impl HeadBudgets {
    pub(super) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            open: Mutex::new(HashMap::new()),
        })
    }

    /// Remove the limit from `id`: its request head is parsed.
    pub(super) fn release(&self, id: StreamId) {
        if let Some(released) = self.lock().remove(&id) {
            released.store(true, Ordering::Release);
        }
    }

    fn register(&self, id: StreamId) -> Arc<AtomicBool> {
        let released = Arc::new(AtomicBool::new(false));
        self.lock().insert(id, Arc::clone(&released));
        released
    }

    fn forget(&self, id: StreamId) {
        self.lock().remove(&id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<StreamId, Arc<AtomicBool>>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn open_count(&self) -> usize {
        self.lock().len()
    }
}

/// The byte count of one receive stream.
#[derive(Debug)]
struct Gate {
    read: usize,
    limit: usize,
    /// `None` on a unidirectional stream: its limit is never removed.
    released: Option<Arc<AtomicBool>>,
    /// Where to remove the entry of a request stream when it drops.
    registry: Option<(Arc<HeadBudgets>, StreamId)>,
}

impl Gate {
    const fn uni() -> Self {
        Self {
            read: 0,
            limit: UNI_STREAM_LIMIT,
            released: None,
            registry: None,
        }
    }

    fn request(budgets: &Arc<HeadBudgets>, id: StreamId) -> Self {
        Self {
            read: 0,
            limit: budgets.limit,
            released: Some(budgets.register(id)),
            registry: Some((Arc::clone(budgets), id)),
        }
    }

    /// Count `len` bytes. `false` when the stream is over its limit.
    fn admit(&mut self, len: usize) -> bool {
        if self
            .released
            .as_ref()
            .is_some_and(|released| released.load(Ordering::Acquire))
        {
            return true;
        }
        self.read = self.read.saturating_add(len);
        self.read <= self.limit
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        if let Some((budgets, id)) = self.registry.take() {
            budgets.forget(id);
        }
    }
}

fn poll_limited<S: quic::RecvStream>(
    inner: &mut S,
    gate: &mut Gate,
    cx: &mut Context<'_>,
) -> Poll<Result<Option<S::Buf>, StreamErrorIncoming>> {
    let data = ready!(inner.poll_data(cx))?;
    if let Some(buf) = &data
        && !gate.admit(buf.remaining())
    {
        inner.stop_sending(H3_EXCESSIVE_LOAD);
        return Poll::Ready(Err(StreamErrorIncoming::Unknown(Box::new(OverLimit {
            limit: gate.limit,
        }))));
    }
    Poll::Ready(Ok(data))
}

/// A receive stream with a byte limit.
pub(super) struct LimitedRecv<S> {
    inner: S,
    gate: Gate,
}

impl<S: quic::RecvStream> quic::RecvStream for LimitedRecv<S> {
    type Buf = S::Buf;

    fn poll_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        poll_limited(&mut self.inner, &mut self.gate, cx)
    }

    fn stop_sending(&mut self, error_code: u64) {
        self.inner.stop_sending(error_code);
    }

    fn recv_id(&self) -> StreamId {
        self.inner.recv_id()
    }
}

/// A request stream whose receive side has a byte limit.
pub(super) struct LimitedBidi<B: Buf> {
    inner: h3_quinn::BidiStream<B>,
    gate: Gate,
}

impl<B: Buf> quic::SendStream<B> for LimitedBidi<B> {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.inner.poll_ready(cx)
    }

    fn send_data<T: Into<WriteBuf<B>>>(&mut self, data: T) -> Result<(), StreamErrorIncoming> {
        self.inner.send_data(data)
    }

    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.inner.poll_finish(cx)
    }

    fn reset(&mut self, reset_code: u64) {
        self.inner.reset(reset_code);
    }

    fn send_id(&self) -> StreamId {
        self.inner.send_id()
    }
}

impl<B: Buf> quic::RecvStream for LimitedBidi<B> {
    type Buf = Bytes;

    fn poll_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        poll_limited(&mut self.inner, &mut self.gate, cx)
    }

    fn stop_sending(&mut self, error_code: u64) {
        self.inner.stop_sending(error_code);
    }

    fn recv_id(&self) -> StreamId {
        self.inner.recv_id()
    }
}

use bytes::Bytes;

impl<B: Buf> quic::BidiStream<B> for LimitedBidi<B> {
    type SendStream = h3_quinn::SendStream<B>;
    type RecvStream = LimitedRecv<h3_quinn::RecvStream>;

    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        let (send, recv) = self.inner.split();
        (
            send,
            LimitedRecv {
                inner: recv,
                gate: self.gate,
            },
        )
    }
}

/// Opens server-side streams. The server opens only its unidirectional
/// streams; a bidirectional one would carry no client data to limit.
pub(super) struct LimitedOpenStreams {
    inner: h3_quinn::OpenStreams,
}

impl<B: Buf> quic::OpenStreams<B> for LimitedOpenStreams {
    type BidiStream = LimitedBidi<B>;
    type SendStream = h3_quinn::SendStream<B>;

    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let inner = ready!(quic::OpenStreams::<B>::poll_open_bidi(&mut self.inner, cx))?;
        Poll::Ready(Ok(LimitedBidi {
            inner,
            gate: Gate::uni(),
        }))
    }

    fn poll_open_send(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        quic::OpenStreams::<B>::poll_open_send(&mut self.inner, cx)
    }

    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        quic::OpenStreams::<B>::close(&mut self.inner, code, reason);
    }
}

/// An `h3_quinn` connection whose receive streams have byte limits.
pub(super) struct LimitedConnection {
    inner: h3_quinn::Connection,
    budgets: Arc<HeadBudgets>,
}

impl LimitedConnection {
    pub(super) fn new(conn: quinn::Connection, budgets: Arc<HeadBudgets>) -> Self {
        Self {
            inner: h3_quinn::Connection::new(conn),
            budgets,
        }
    }
}

impl<B: Buf> quic::OpenStreams<B> for LimitedConnection {
    type BidiStream = LimitedBidi<B>;
    type SendStream = h3_quinn::SendStream<B>;

    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let inner = ready!(quic::OpenStreams::<B>::poll_open_bidi(&mut self.inner, cx))?;
        Poll::Ready(Ok(LimitedBidi {
            inner,
            gate: Gate::uni(),
        }))
    }

    fn poll_open_send(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        quic::OpenStreams::<B>::poll_open_send(&mut self.inner, cx)
    }

    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        quic::OpenStreams::<B>::close(&mut self.inner, code, reason);
    }
}

impl<B: Buf> quic::Connection<B> for LimitedConnection {
    type RecvStream = LimitedRecv<h3_quinn::RecvStream>;
    type OpenStreams = LimitedOpenStreams;

    fn poll_accept_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        let inner = ready!(quic::Connection::<B>::poll_accept_recv(&mut self.inner, cx))?;
        Poll::Ready(Ok(LimitedRecv {
            inner,
            gate: Gate::uni(),
        }))
    }

    fn poll_accept_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        let inner = ready!(quic::Connection::<B>::poll_accept_bidi(&mut self.inner, cx))?;
        let id = quic::RecvStream::recv_id(&inner);
        Poll::Ready(Ok(LimitedBidi {
            inner,
            gate: Gate::request(&self.budgets, id),
        }))
    }

    fn opener(&self) -> Self::OpenStreams {
        LimitedOpenStreams {
            inner: quic::Connection::<B>::opener(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uni_gate_never_lifts_its_limit() {
        let mut gate = Gate::uni();
        assert!(gate.admit(UNI_STREAM_LIMIT));
        assert!(!gate.admit(1), "one byte over the limit is refused");
    }

    #[test]
    fn a_request_gate_lifts_its_limit_on_release() {
        let budgets = HeadBudgets::new(10);
        let id = StreamId::try_from(0_u64).expect("stream 0");
        let mut gate = Gate::request(&budgets, id);
        assert!(gate.admit(10));
        assert!(!gate.admit(1), "the head may not pass the limit");

        let mut gate = Gate::request(&budgets, id);
        assert!(gate.admit(10));
        budgets.release(id);
        assert!(gate.admit(1_000_000), "the body is not limited here");
    }

    #[test]
    fn a_dropped_request_gate_leaves_no_entry() {
        let budgets = HeadBudgets::new(10);
        let gate = Gate::request(&budgets, StreamId::try_from(4_u64).expect("stream 4"));
        assert_eq!(budgets.open_count(), 1);
        drop(gate);
        assert_eq!(budgets.open_count(), 0);
    }
}
