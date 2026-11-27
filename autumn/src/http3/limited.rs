//! The QUIC transport that `h3` reads from, with byte limits.
//!
//! `h3` buffers a whole frame before it parses it, except DATA, which it
//! streams. It checks `max_field_section_size` only after it has the whole
//! HEADERS frame. A client can declare a very large frame and send data into
//! it. This wrapper reads the frame headers as `h3` receives them:
//!
//! - On a request stream, a frame that is not DATA (HEADERS, trailers, an
//!   unknown type) may not declare more than the head limit. DATA frames
//!   pass: the app's body limits apply to them.
//! - On a unidirectional stream (control, QPACK), the limit applies to the
//!   bytes of the whole stream. These streams carry a few small frames.
//!
//! When a stream goes over its limit, the wrapper stops the stream and gives
//! `h3` an error. On the control stream, that closes the connection.

use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use h3::quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf};

/// The byte limit on a unidirectional stream.
pub(super) const UNI_STREAM_LIMIT: u64 = 64 * 1024;

/// The HTTP/3 DATA frame type.
const DATA_FRAME: u64 = 0x00;

/// `H3_EXCESSIVE_LOAD`, sent in `STOP_SENDING` when a stream goes over its limit.
const H3_EXCESSIVE_LOAD: u64 = 0x0107;

/// A stream went over its byte limit.
#[derive(Debug, thiserror::Error)]
#[error("the HTTP/3 stream sent a frame larger than the limit of {limit} bytes")]
struct OverLimit {
    limit: u64,
}

/// A QUIC variable-length integer, read one byte at a time.
#[derive(Debug, Clone, Copy, Default)]
struct VarInt {
    value: u64,
    /// Bytes still to read. `0` before the first byte.
    left: u8,
    started: bool,
}

impl VarInt {
    /// Add one byte. `Some(value)` when the integer is complete.
    const fn push(&mut self, byte: u8) -> Option<u64> {
        if self.started {
            self.value = (self.value << 8) | byte as u64;
            self.left -= 1;
        } else {
            self.started = true;
            // The two high bits give the length: 1, 2, 4 or 8 bytes.
            self.left = (1u8 << (byte >> 6)) - 1;
            self.value = (byte & 0x3f) as u64;
        }
        if self.left == 0 {
            let value = self.value;
            *self = Self {
                value: 0,
                left: 0,
                started: false,
            };
            Some(value)
        } else {
            None
        }
    }
}

/// Where the frame reader is in the request stream.
#[derive(Debug, Clone, Copy)]
enum FrameState {
    Type(VarInt),
    Length { frame_type: u64, length: VarInt },
    Payload { left: u64 },
}

/// The byte limit of one receive stream.
#[derive(Debug)]
enum Gate {
    /// A request stream: each frame that is not DATA has a size limit.
    Frames { limit: u64, state: FrameState },
    /// A unidirectional stream: the whole stream has a size limit.
    Total { limit: u64, read: u64 },
}

impl Gate {
    const fn request(limit: u64) -> Self {
        Self::Frames {
            limit,
            state: FrameState::Type(VarInt {
                value: 0,
                left: 0,
                started: false,
            }),
        }
    }

    const fn uni() -> Self {
        Self::Total {
            limit: UNI_STREAM_LIMIT,
            read: 0,
        }
    }

    const fn limit(&self) -> u64 {
        match self {
            Self::Frames { limit, .. } | Self::Total { limit, .. } => *limit,
        }
    }

    /// Read `bytes`. `false` when the stream is over its limit.
    fn admit(&mut self, mut bytes: &[u8]) -> bool {
        match self {
            Self::Total { limit, read } => {
                *read = read.saturating_add(bytes.len() as u64);
                *read <= *limit
            }
            Self::Frames { limit, state } => {
                while !bytes.is_empty() {
                    match state {
                        FrameState::Payload { left } => {
                            let take = usize::try_from(*left)
                                .map_or(bytes.len(), |left| left.min(bytes.len()));
                            bytes = &bytes[take..];
                            *left -= take as u64;
                            if *left == 0 {
                                *state = FrameState::Type(VarInt::default());
                            }
                        }
                        FrameState::Type(varint) => {
                            let byte = bytes[0];
                            bytes = &bytes[1..];
                            if let Some(frame_type) = varint.push(byte) {
                                *state = FrameState::Length {
                                    frame_type,
                                    length: VarInt::default(),
                                };
                            }
                        }
                        FrameState::Length { frame_type, length } => {
                            let byte = bytes[0];
                            bytes = &bytes[1..];
                            if let Some(length) = length.push(byte) {
                                if *frame_type != DATA_FRAME && length > *limit {
                                    return false;
                                }
                                *state = if length == 0 {
                                    FrameState::Type(VarInt::default())
                                } else {
                                    FrameState::Payload { left: length }
                                };
                            }
                        }
                    }
                }
                true
            }
        }
    }
}

fn poll_limited<S: quic::RecvStream<Buf = Bytes>>(
    inner: &mut S,
    gate: &mut Gate,
    cx: &mut Context<'_>,
) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
    let data = ready!(inner.poll_data(cx))?;
    if let Some(buf) = &data
        && !gate.admit(buf.chunk())
    {
        inner.stop_sending(H3_EXCESSIVE_LOAD);
        return Poll::Ready(Err(StreamErrorIncoming::Unknown(Box::new(OverLimit {
            limit: gate.limit(),
        }))));
    }
    Poll::Ready(Ok(data))
}

/// A receive stream with a byte limit.
pub(super) struct LimitedRecv<S> {
    inner: S,
    gate: Gate,
}

impl<S: quic::RecvStream<Buf = Bytes>> quic::RecvStream for LimitedRecv<S> {
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

/// Opens server-side streams.
pub(super) struct LimitedOpenStreams {
    inner: h3_quinn::OpenStreams,
    limit: u64,
}

fn open_bidi<B: Buf, O: quic::OpenStreams<B, BidiStream = h3_quinn::BidiStream<B>>>(
    opener: &mut O,
    limit: u64,
    cx: &mut Context<'_>,
) -> Poll<Result<LimitedBidi<B>, StreamErrorIncoming>> {
    let inner = ready!(opener.poll_open_bidi(cx))?;
    Poll::Ready(Ok(LimitedBidi {
        inner,
        gate: Gate::request(limit),
    }))
}

impl<B: Buf> quic::OpenStreams<B> for LimitedOpenStreams {
    type BidiStream = LimitedBidi<B>;
    type SendStream = h3_quinn::SendStream<B>;

    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        open_bidi(&mut self.inner, self.limit, cx)
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
    limit: u64,
}

impl LimitedConnection {
    /// Wrap `conn`. `limit` is the largest frame, other than DATA, that a
    /// request stream may declare.
    pub(super) fn new(conn: quinn::Connection, limit: u64) -> Self {
        Self {
            inner: h3_quinn::Connection::new(conn),
            limit,
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
        open_bidi(&mut self.inner, self.limit, cx)
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
        Poll::Ready(Ok(LimitedBidi {
            inner,
            gate: Gate::request(self.limit),
        }))
    }

    fn opener(&self) -> Self::OpenStreams {
        LimitedOpenStreams {
            inner: quic::Connection::<B>::opener(&self.inner),
            limit: self.limit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `value` as a QUIC variable-length integer.
    fn varint(value: u64) -> Vec<u8> {
        match value {
            0..=0x3f => vec![u8::try_from(value).unwrap()],
            0x40..=0x3fff => (u16::try_from(value).unwrap() | 0x4000)
                .to_be_bytes()
                .to_vec(),
            0x4000..=0x3fff_ffff => (u32::try_from(value).unwrap() | 0x8000_0000)
                .to_be_bytes()
                .to_vec(),
            _ => (value | 0xc000_0000_0000_0000).to_be_bytes().to_vec(),
        }
    }

    /// A frame header: type, then declared length.
    fn header(frame_type: u64, length: u64) -> Vec<u8> {
        let mut out = varint(frame_type);
        out.extend(varint(length));
        out
    }

    #[test]
    fn varints_of_every_length_decode() {
        for value in [
            0,
            37,
            0x3f,
            0x40,
            15_293,
            0x3fff,
            0x4000,
            494_878_333,
            0x4000_0000,
        ] {
            let mut reader = VarInt::default();
            let bytes = varint(value);
            let (last, rest) = bytes.split_last().unwrap();
            for byte in rest {
                assert_eq!(reader.push(*byte), None);
            }
            assert_eq!(reader.push(*last), Some(value), "{value}");
        }
    }

    #[test]
    fn a_head_inside_the_limit_passes() {
        let mut gate = Gate::request(100);
        let mut bytes = header(0x01, 100);
        bytes.extend([0u8; 100]);
        assert!(gate.admit(&bytes));
    }

    #[test]
    fn a_head_that_declares_more_than_the_limit_is_refused() {
        let mut gate = Gate::request(100);
        assert!(!gate.admit(&header(0x01, 101)));
    }

    #[test]
    fn a_head_and_its_body_in_one_chunk_count_only_the_head() {
        let mut gate = Gate::request(16);
        let mut bytes = header(0x01, 16);
        bytes.extend([0u8; 16]);
        bytes.extend(header(DATA_FRAME, 100_000));
        bytes.extend(vec![0u8; 100_000]);
        assert!(
            gate.admit(&bytes),
            "DATA bytes do not count against the head"
        );
    }

    #[test]
    fn oversized_trailers_after_a_body_are_refused() {
        let mut gate = Gate::request(16);
        let mut bytes = header(0x01, 8);
        bytes.extend([0u8; 8]);
        bytes.extend(header(DATA_FRAME, 1_000));
        bytes.extend(vec![0u8; 1_000]);
        assert!(gate.admit(&bytes));
        assert!(
            !gate.admit(&header(0x01, 1_000_000)),
            "trailers are a HEADERS frame"
        );
    }

    #[test]
    fn an_oversized_unknown_frame_is_refused() {
        let mut gate = Gate::request(16);
        assert!(!gate.admit(&header(0x21, 17)));
    }

    #[test]
    fn a_frame_header_split_across_chunks_is_read() {
        let mut gate = Gate::request(100);
        let bytes = header(0x01, 0x4000);
        for byte in &bytes[..bytes.len() - 1] {
            assert!(gate.admit(std::slice::from_ref(byte)));
        }
        assert!(
            !gate.admit(&bytes[bytes.len() - 1..]),
            "the length ends in the last chunk"
        );
    }

    #[test]
    fn a_large_data_frame_split_across_chunks_passes() {
        let mut gate = Gate::request(16);
        assert!(gate.admit(&header(DATA_FRAME, 300_000)));
        for _ in 0..3 {
            assert!(gate.admit(&vec![0u8; 100_000]));
        }
        assert!(
            !gate.admit(&header(0x01, 17)),
            "the next frame is read again"
        );
    }

    #[test]
    fn a_uni_stream_has_a_lifetime_limit() {
        let mut gate = Gate::uni();
        let limit = usize::try_from(UNI_STREAM_LIMIT).unwrap();
        assert!(gate.admit(&vec![0u8; limit]));
        assert!(!gate.admit(&[0]), "one byte over the limit is refused");
    }
}
