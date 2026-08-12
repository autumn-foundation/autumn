//! `PostgreSQL` frontend/backend protocol 3.0 framing, parsing and building.
//!
//! This module is pure: it never performs I/O, never blocks and holds no
//! runtime state beyond the per-direction byte accumulator inside
//! [`FrameSplitter`]. It exists so the capture stage
//! (`capsule::record_db`) can tee a live connection into discrete protocol
//! frames, and the replay stage (`capsule::replay_db`) can answer a real
//! `tokio-postgres` client with hand-built backend frames.
//!
//! # Framing rules honoured here
//!
//! * A regular message is `[tag: u8][len: i32 big-endian, INCLUDING the four
//!   length bytes][payload]`. The frame therefore occupies `1 + len` bytes.
//! * The startup packet and `SSLRequest`/`GSSENCRequest`/`CancelRequest` carry
//!   **no tag**: `[len: i32][payload]`, the frame occupying `len` bytes.
//!   Those are surfaced with the [`TAG_UNTAGGED`] sentinel.
//! * The backend's answer to an `SSLRequest` is a single **unframed** byte,
//!   `S` (accept) or `N` (refuse). Autumn always connects with SSL disabled,
//!   but a stray refusal byte is tolerated: it is surfaced as a one-byte
//!   [`TAG_UNTAGGED`] frame. A stray `S` additionally marks the splitter
//!   unrecordable, because everything after it is TLS ciphertext (F7).
//!
//! Anything the splitter cannot make sense of — a length field below the
//! minimum, a frame larger than [`MAX_FRAME_ACCUM`], a `CopyInResponse` /
//! `CopyOutResponse` / `CopyBothResponse` that inverts flow control (F9) —
//! flips the splitter to *unrecordable* instead of guessing. The recorder
//! drops the connection's tape when that happens; it never emits a corrupt
//! one.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]
// `pub` throughout this file is crate-visible only: the enclosing `wire` module
// is itself `pub(crate)`, so nothing here escapes the crate
// (clippy::redundant_pub_crate).

use bytes::{Bytes, BytesMut};

/// Upper bound on bytes buffered while waiting for one frame to complete.
///
/// A declared frame length above this (or an accumulator that somehow grows
/// past it) marks the connection unrecordable rather than letting a hostile or
/// corrupt length field drive an unbounded allocation (F6).
pub const MAX_FRAME_ACCUM: usize = 8 * 1024 * 1024;

/// Sentinel [`Frame::tag`] for messages that carry no tag byte: the startup
/// packet, `SSLRequest`, and the backend's bare SSL answer byte.
pub const TAG_UNTAGGED: u8 = 0;

/// `ReadyForQuery` — terminates every exchange, simple or extended.
pub const TAG_READY_FOR_QUERY: u8 = b'Z';
/// `ErrorResponse`.
pub const TAG_ERROR_RESPONSE: u8 = b'E';
/// `ParameterStatus` (backend).
pub const TAG_PARAMETER_STATUS: u8 = b'S';
/// `BackendKeyData`.
pub const TAG_BACKEND_KEY_DATA: u8 = b'K';
/// `RowDescription`.
#[allow(
    dead_code,
    reason = "tag-table completeness: the recorder matches RowDescription frames by \
              position inside an exchange rather than by tag, so only the builder and \
              this module's tests name it"
)]
pub const TAG_ROW_DESCRIPTION: u8 = b'T';
/// `DataRow`.
pub const TAG_DATA_ROW: u8 = b'D';
/// `CommandComplete`.
pub const TAG_COMMAND_COMPLETE: u8 = b'C';
/// `CopyInResponse` (backend).
pub const TAG_COPY_IN_RESPONSE: u8 = b'G';
/// `CopyOutResponse` (backend). Note this collides with the *frontend* `Flush`
/// tag, which is why the copy check is direction-aware.
pub const TAG_COPY_OUT_RESPONSE: u8 = b'H';
/// `CopyBothResponse` (backend).
pub const TAG_COPY_BOTH_RESPONSE: u8 = b'W';

/// `SSLRequest` request code (the payload of an untagged 8-byte packet).
pub const SSL_REQUEST_CODE: u32 = 80_877_103;
/// `GSSENCRequest` request code.
pub const GSSENC_REQUEST_CODE: u32 = 80_877_104;
/// `CancelRequest` request code.
pub const CANCEL_REQUEST_CODE: u32 = 80_877_102;
/// Protocol version 3.0, as sent in the startup packet.
#[allow(
    dead_code,
    reason = "startup-code table completeness: the recorder only needs to tell a startup \
              packet from the SSL/GSS/cancel codes, so the version itself is named by \
              this module's tests"
)]
pub const PROTOCOL_VERSION_3: u32 = 196_608;

/// The GUC Autumn sets to bind a pooled connection to a capsule scope.
pub const MARKER_GUC: &str = "autumn.capsule_request";

/// [`MARKER_GUC`] upper-cased, for matching against a normalised statement.
/// Pinned to the real GUC by `marker_guc_upper_matches_the_guc`.
const MARKER_GUC_UPPER: &str = "AUTUMN.CAPSULE_REQUEST";

/// Smallest legal `len` field of a tagged message (the four length bytes with
/// an empty payload).
const MIN_TAGGED_LEN: usize = 4;
/// Smallest legal `len` field of an untagged startup-style packet
/// (`SSLRequest` is exactly eight bytes: length plus request code).
const MIN_UNTAGGED_LEN: usize = 8;

/// Which half of the connection a [`FrameSplitter`] is reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Client to server.
    Frontend,
    /// Server to client.
    Backend,
}

/// One complete protocol message, tag and length bytes included.
///
/// `bytes` is exactly what crossed the wire, so a recorder can concatenate
/// frames and a replayer can write them back verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Message tag, or [`TAG_UNTAGGED`] for startup-phase / SSL-answer frames.
    pub tag: u8,
    /// Full frame bytes including the tag and length prefix.
    pub bytes: Bytes,
}

impl Frame {
    /// Bytes after the tag and length prefix.
    ///
    /// Empty for the one-byte SSL answer frame, which has no prefix at all.
    pub fn payload(&self) -> &[u8] {
        let skip = if self.tag == TAG_UNTAGGED { 4 } else { 5 };
        self.bytes.get(skip..).unwrap_or(&[])
    }

    /// `true` for startup-phase and SSL-answer frames.
    pub const fn is_untagged(&self) -> bool {
        self.tag == TAG_UNTAGGED
    }

    /// The bare `S`/`N` byte when this frame is a backend SSL answer.
    #[allow(
        dead_code,
        reason = "the splitter reacts to an SSL answer where it is read (an `S` marks the \
                  connection unrecordable) rather than through this accessor, which \
                  documents the frame shape and is exercised by this module's tests"
    )]
    pub fn ssl_answer(&self) -> Option<u8> {
        if self.is_untagged() && self.bytes.len() == 1 {
            self.bytes.first().copied()
        } else {
            None
        }
    }

    /// The request code / protocol version of an untagged startup packet.
    pub fn startup_code(&self) -> Option<u32> {
        if !self.is_untagged() {
            return None;
        }
        be_u32(self.payload())
    }
}

/// Incremental frame extractor for one direction of one connection.
///
/// Feed it whatever bytes the tee observed; it returns the frames that are now
/// complete and keeps the partial tail for the next call (F6).
#[derive(Debug)]
pub struct FrameSplitter {
    direction: Direction,
    phase: Phase,
    buf: BytesMut,
    unrecordable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Frontend startup phase: untagged packets until the real startup message.
    Untagged,
    /// Backend, first byte of the stream not yet classified.
    BackendFirstByte,
    /// Ordinary tagged protocol 3.0 messages.
    Tagged,
}

enum Step {
    Frame(Frame),
    Need,
    Fail,
}

impl FrameSplitter {
    /// Splitter for the client-to-server half (starts in the untagged startup
    /// phase).
    pub fn new_frontend() -> Self {
        Self {
            direction: Direction::Frontend,
            phase: Phase::Untagged,
            buf: BytesMut::new(),
            unrecordable: false,
        }
    }

    /// Splitter for the server-to-client half (tagged from the first byte,
    /// except for a possible bare SSL answer byte).
    pub fn new_backend() -> Self {
        Self {
            direction: Direction::Backend,
            phase: Phase::BackendFirstByte,
            buf: BytesMut::new(),
            unrecordable: false,
        }
    }

    /// Which half of the connection this splitter reads.
    #[allow(
        dead_code,
        reason = "the recorder owns one splitter per direction and knows which is which \
                  from the field it reads it out of; the accessor keeps the type \
                  self-describing and is exercised by this module's tests"
    )]
    pub const fn direction(&self) -> Direction {
        self.direction
    }

    /// `true` once the splitter has seen something it refuses to model.
    pub const fn is_unrecordable(&self) -> bool {
        self.unrecordable
    }

    /// Bytes currently held back as a partial frame.
    #[allow(
        dead_code,
        reason = "the accumulator bound is enforced inside `push`, so production code never \
                  asks; the partial-frame tests assert on it"
    )]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Mark the connection unrecordable and drop any buffered bytes.
    pub fn mark_unrecordable(&mut self) {
        self.unrecordable = true;
        self.buf = BytesMut::new();
    }

    /// Feed observed bytes; returns every frame that completed.
    ///
    /// Bytes that do not yet form a whole frame are retained for the next
    /// call, so the caller may hand over arbitrary chunk boundaries. Once the
    /// splitter is unrecordable it consumes nothing and returns nothing.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        if self.unrecordable {
            return frames;
        }
        self.buf.extend_from_slice(bytes);
        while !self.unrecordable {
            match self.next_frame() {
                Step::Frame(frame) => frames.push(frame),
                Step::Need => break,
                Step::Fail => self.mark_unrecordable(),
            }
        }
        // Belt and braces: a partial frame can never legitimately exceed the
        // budget (the length check below refuses first), but an accumulator
        // that somehow grows past it is treated the same way.
        if self.buf.len() > MAX_FRAME_ACCUM {
            self.mark_unrecordable();
        }
        frames
    }

    fn next_frame(&mut self) -> Step {
        match self.phase {
            Phase::Untagged => self.next_untagged(),
            Phase::BackendFirstByte => self.next_backend_first_byte(),
            Phase::Tagged => self.next_tagged(),
        }
    }

    /// Frontend startup phase: `[len][payload]`, no tag byte.
    fn next_untagged(&mut self) -> Step {
        let Some(len) = be_i32(&self.buf) else {
            return Step::Need;
        };
        let Ok(total) = usize::try_from(len) else {
            return Step::Fail;
        };
        if !(MIN_UNTAGGED_LEN..=MAX_FRAME_ACCUM).contains(&total) {
            return Step::Fail;
        }
        if self.buf.len() < total {
            return Step::Need;
        }
        let frame = Frame {
            tag: TAG_UNTAGGED,
            bytes: self.buf.split_to(total).freeze(),
        };
        // `SSLRequest`/`GSSENCRequest` are answered with a single byte and are
        // followed by another untagged packet; the real startup message is the
        // last untagged thing the frontend ever sends.
        let still_untagged = matches!(
            frame.startup_code(),
            Some(SSL_REQUEST_CODE | GSSENC_REQUEST_CODE | CANCEL_REQUEST_CODE)
        );
        if !still_untagged {
            self.phase = Phase::Tagged;
        }
        Step::Frame(frame)
    }

    /// Backend: classify a possible bare `S`/`N` SSL answer byte, which is not
    /// framed at all, before falling through to ordinary tagged framing.
    fn next_backend_first_byte(&mut self) -> Step {
        let Some(&first) = self.buf.first() else {
            return Step::Need;
        };
        if first == b'S' || first == b'N' {
            // `S` is also `ParameterStatus` and `N` also `NoticeResponse`, so
            // disambiguate on the four bytes that would be the length field: a
            // real message declares a plausible length, whereas the byte after
            // a bare answer is the tag of the next message.
            let plausible_len = be_i32(self.buf.get(1..).unwrap_or_default()).is_some_and(|len| {
                usize::try_from(len)
                    .is_ok_and(|len| (MIN_TAGGED_LEN..=MAX_FRAME_ACCUM).contains(&len))
            });
            let decidable = self.buf.len() >= 5 || self.buf.len() == 1;
            if !decidable {
                return Step::Need;
            }
            if !plausible_len {
                let frame = Frame {
                    tag: TAG_UNTAGGED,
                    bytes: self.buf.split_to(1).freeze(),
                };
                self.phase = Phase::Tagged;
                if first == b'S' {
                    // SSL was accepted: everything after this byte is TLS
                    // ciphertext we cannot frame (F7).
                    self.mark_unrecordable();
                }
                return Step::Frame(frame);
            }
        }
        self.phase = Phase::Tagged;
        self.next_tagged()
    }

    /// Ordinary protocol 3.0 message: `[tag][len][payload]`.
    fn next_tagged(&mut self) -> Step {
        let Some(&tag) = self.buf.first() else {
            return Step::Need;
        };
        let Some(len) = be_i32(self.buf.get(1..).unwrap_or_default()) else {
            return Step::Need;
        };
        let Ok(len) = usize::try_from(len) else {
            return Step::Fail;
        };
        if !(MIN_TAGGED_LEN..=MAX_FRAME_ACCUM).contains(&len) {
            return Step::Fail;
        }
        let Some(total) = len.checked_add(1) else {
            return Step::Fail;
        };
        if self.buf.len() < total {
            return Step::Need;
        }
        let frame = Frame {
            tag,
            bytes: self.buf.split_to(total).freeze(),
        };
        if self.direction == Direction::Backend && is_copy_start(tag) {
            // The copy sub-protocol inverts flow control; the recorder refuses
            // to model it rather than producing a tape that cannot replay (F9).
            self.mark_unrecordable();
        }
        Step::Frame(frame)
    }
}

/// A parsed frontend message.
///
/// Only the parts the capture and replay stages act on are modelled; anything
/// else (including a message whose payload does not parse) becomes
/// [`FrontendMessage::Other`] carrying the raw tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendMessage {
    /// Untagged startup packet.
    Startup,
    /// Untagged `SSLRequest`.
    SslRequest,
    /// `Parse` — prepared statement creation.
    Parse {
        /// Statement name (empty for the unnamed statement).
        name: String,
        /// The SQL text.
        sql: String,
        /// Explicitly specified parameter type OIDs (may be shorter than the
        /// parameter count; zero means "infer").
        param_oids: Vec<u32>,
    },
    /// `Bind` — portal creation.
    Bind {
        /// Portal name (empty for the unnamed portal).
        portal: String,
        /// Source prepared statement name.
        statement: String,
        /// Parameter values in wire form; `None` is SQL NULL (length `-1`).
        params: Vec<Option<Vec<u8>>>,
    },
    /// `Describe` — `S` for statement, `P` for portal.
    Describe {
        /// `b'S'` or `b'P'`.
        kind: u8,
        /// Statement or portal name.
        name: String,
    },
    /// `Execute`.
    Execute,
    /// Simple-protocol `Query` with its (possibly multi-statement) SQL text.
    Query(String),
    /// `Sync`.
    Sync,
    /// `Flush`.
    Flush,
    /// `Close` — drops a prepared statement (`S`) or a portal (`P`).
    Close {
        /// `b'S'` or `b'P'`.
        kind: u8,
        /// Statement or portal name.
        name: String,
    },
    /// `Terminate`.
    Terminate,
    /// Any other tag, or a message whose payload did not parse.
    Other(u8),
}

/// Parse a frontend frame produced by [`FrameSplitter::new_frontend`].
///
/// A payload that does not parse yields [`FrontendMessage::Other`]: this
/// module never panics on malformed input, it degrades.
pub fn parse_frontend(frame: &Frame) -> FrontendMessage {
    if frame.is_untagged() {
        return match frame.startup_code() {
            Some(SSL_REQUEST_CODE) => FrontendMessage::SslRequest,
            _ => FrontendMessage::Startup,
        };
    }
    let mut reader = Reader::new(frame.payload());
    let parsed = match frame.tag {
        b'P' => parse_parse(&mut reader),
        b'B' => parse_bind(&mut reader),
        b'D' => parse_describe(&mut reader),
        b'Q' => reader.cstr().map(FrontendMessage::Query),
        b'E' => Some(FrontendMessage::Execute),
        b'S' => Some(FrontendMessage::Sync),
        b'H' => Some(FrontendMessage::Flush),
        b'C' => parse_close(&mut reader),
        b'X' => Some(FrontendMessage::Terminate),
        _ => None,
    };
    parsed.unwrap_or(FrontendMessage::Other(frame.tag))
}

/// `Parse`: cstring name, cstring SQL, `i16` count, then that many `u32` OIDs.
fn parse_parse(reader: &mut Reader<'_>) -> Option<FrontendMessage> {
    let name = reader.cstr()?;
    let sql = reader.cstr()?;
    let count = reader.count()?;
    let mut param_oids = Vec::with_capacity(count.min(SANE_COUNT));
    for _ in 0..count {
        param_oids.push(reader.u32()?);
    }
    Some(FrontendMessage::Parse {
        name,
        sql,
        param_oids,
    })
}

/// `Bind`: cstring portal, cstring statement, format codes, parameter values,
/// then result format codes (which the recorder does not need).
fn parse_bind(reader: &mut Reader<'_>) -> Option<FrontendMessage> {
    let portal = reader.cstr()?;
    let statement = reader.cstr()?;
    let formats = reader.count()?;
    for _ in 0..formats {
        reader.i16()?;
    }
    let count = reader.count()?;
    let mut params = Vec::with_capacity(count.min(SANE_COUNT));
    for _ in 0..count {
        let len = reader.i32()?;
        if len < 0 {
            params.push(None);
        } else {
            let len = usize::try_from(len).ok()?;
            params.push(Some(reader.take(len)?.to_vec()));
        }
    }
    Some(FrontendMessage::Bind {
        portal,
        statement,
        params,
    })
}

/// `Describe`: one kind byte (`S` or `P`) then a cstring name.
fn parse_describe(reader: &mut Reader<'_>) -> Option<FrontendMessage> {
    let kind = reader.u8()?;
    let name = reader.cstr()?;
    Some(FrontendMessage::Describe { kind, name })
}

/// `Close`: same shape as `Describe` — one kind byte then a cstring name.
///
/// The recorder needs the name: closing a statement retires the
/// name-to-SQL entry it learnt from the matching `Parse`.
fn parse_close(reader: &mut Reader<'_>) -> Option<FrontendMessage> {
    let kind = reader.u8()?;
    let name = reader.cstr()?;
    Some(FrontendMessage::Close { kind, name })
}

/// The outcome of scanning SQL for the capsule attribution marker.
///
/// `None` from [`marker_request_id`] means "no marker statement here at all";
/// the variants distinguish the three kinds of marker we can find (F24).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkerId {
    /// `SET autumn.capsule_request = '<id>'` with an id that passed validation.
    Set(String),
    /// `SET autumn.capsule_request = ''` — unbind the connection.
    Clear,
    /// A marker statement whose value is not a valid id. The recorder must not
    /// bind the connection; it notes the capsule instead.
    Invalid,
}

/// Scan a (possibly multi-statement) SQL string for the capsule marker.
///
/// The marker rides along in the same batch as `SET statement_timeout`, so the
/// scan is statement-aware and quote-aware. When several markers appear, the
/// last one wins.
pub fn marker_request_id(sql: &str) -> Option<MarkerId> {
    let mut found = None;
    for statement in split_statements(sql) {
        if let Some(marker) = parse_marker_statement(statement) {
            found = Some(marker);
        }
    }
    found
}

/// Maximum length of a capsule request id.
const MAX_MARKER_ID_LEN: usize = 64;

/// `true` when `id` matches `[A-Za-z0-9_-]{1,64}` (F24).
pub fn is_valid_marker_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MARKER_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Render the marker statement for `id`, or `None` when `id` is not safe to
/// interpolate. Pass an empty string to build the clearing form.
pub fn marker_set_sql(id: &str) -> Option<String> {
    if id.is_empty() || is_valid_marker_id(id) {
        Some(format!("SET {MARKER_GUC} = '{id}'"))
    } else {
        None
    }
}

/// Whether every statement in `sql` is session housekeeping — a setting the
/// framework issues on its own behalf, which the replay stub answers
/// synthetically rather than from the tape.
///
/// One definition, used by both halves: the recorder keeps these statements out
/// of a capsule's ordered `exchanges` (a recorded copy would leave replay's
/// cursor a step ahead of the client for the rest of the tape) and the stub
/// answers exactly the same set. Two spellings of "housekeeping" would mean a
/// statement dropped at record time and demanded at replay time.
///
/// A batch counts only if *every* statement in it does: `SET statement_timeout
/// = 5000; SELECT 1` is the request's work. An empty batch is **not**
/// housekeeping — there is nothing there to be the framework's own — so a
/// caller cannot pass a blank string and have arbitrary handling applied.
pub fn is_session_housekeeping(sql: &str) -> bool {
    let mut saw_statement = false;
    for statement in split_statements(sql) {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        saw_statement = true;
        if !is_housekeeping_statement(statement) {
            return false;
        }
    }
    saw_statement
}

/// Whether one statement is a session setting replay reproduces on its own.
///
/// Whitespace-tolerant on both sides of the setting name, so `SET  TIME ZONE`
/// is classified identically by the recorder and the stub.
fn is_housekeeping_statement(statement: &str) -> bool {
    let statement = statement.trim().to_ascii_uppercase();
    let Some(setting) = strip_keyword(&statement, "SET") else {
        return false;
    };
    let setting = setting.trim_start();
    [
        "TIME ZONE",
        "CLIENT_ENCODING",
        "STATEMENT_TIMEOUT",
        MARKER_GUC_UPPER,
    ]
    .iter()
    .any(|name| setting.starts_with(name))
}

/// Split a SQL batch at top-level semicolons, ignoring semicolons inside
/// single-quoted literals and double-quoted identifiers.
pub fn split_statements(sql: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    for (index, byte) in sql.bytes().enumerate() {
        match byte {
            // `''` inside a literal toggles twice, which is a no-op — exactly
            // the behaviour an escaped quote needs.
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b';' if !in_single && !in_double => {
                if let Some(statement) = sql.get(start..index) {
                    out.push(statement);
                }
                start = index.saturating_add(1);
            }
            _ => {}
        }
    }
    if let Some(statement) = sql.get(start..) {
        out.push(statement);
    }
    out
}

/// Recognise `SET [SESSION|LOCAL] autumn.capsule_request (=|TO) '<value>'`.
fn parse_marker_statement(statement: &str) -> Option<MarkerId> {
    let rest = strip_keyword(statement.trim_start(), "set")?;
    let rest = strip_keyword(rest, "session")
        .or_else(|| strip_keyword(rest, "local"))
        .unwrap_or(rest);
    let rest = strip_keyword_exact(rest, MARKER_GUC)?;
    let rest = match rest.strip_prefix('=') {
        Some(rest) => rest,
        None => strip_keyword(rest, "to")?,
    };
    let Some(value) = single_quoted_literal(rest.trim_start()) else {
        // A marker statement we recognise but whose value we cannot read (a
        // bare word, `DEFAULT`, an unterminated literal) must not bind.
        return Some(MarkerId::Invalid);
    };
    if value.is_empty() {
        Some(MarkerId::Clear)
    } else if is_valid_marker_id(&value) {
        Some(MarkerId::Set(value))
    } else {
        Some(MarkerId::Invalid)
    }
}

/// Strip a leading ASCII-case-insensitive keyword that is followed by
/// whitespace, returning the trimmed remainder.
fn strip_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = strip_prefix_ignore_ascii_case(input, keyword)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start())
}

/// Strip a leading ASCII-case-insensitive token that must be followed by
/// whitespace or `=`, returning the trimmed remainder.
fn strip_keyword_exact<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = strip_prefix_ignore_ascii_case(input, keyword)?;
    match rest.chars().next() {
        None => Some(rest),
        Some(next) if next.is_whitespace() || next == '=' => Some(rest.trim_start()),
        Some(_) => None,
    }
}

fn strip_prefix_ignore_ascii_case<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    if !input.get(..prefix.len())?.eq_ignore_ascii_case(prefix) {
        return None;
    }
    input.get(prefix.len()..)
}

/// Read one single-quoted SQL literal (with `''` escapes) that makes up the
/// whole remainder of the statement.
fn single_quoted_literal(input: &str) -> Option<String> {
    let inner = input.strip_prefix('\'')?;
    let mut value = String::new();
    let mut chars = inner.char_indices();
    while let Some((index, ch)) = chars.next() {
        if ch != '\'' {
            value.push(ch);
            continue;
        }
        let after = inner.get(index.saturating_add(1)..).unwrap_or_default();
        if after.starts_with('\'') {
            value.push('\'');
            chars.next();
            continue;
        }
        return after.trim().is_empty().then_some(value);
    }
    None
}

/// Heuristic: does this SQL probe the system catalogs?
///
/// Catalog round trips (`tokio-postgres` type-info lookups, Diesel's schema
/// probes) are cached per connection in the [`ConnectionMemo`] rather than
/// recorded as request exchanges (F3/F4).
///
/// [`ConnectionMemo`]: ../record_db/struct.ConnectionMemo.html
pub fn is_catalog_sql(sql: &str) -> bool {
    let lowered = sql.to_ascii_lowercase();
    CATALOG_TOKENS
        .iter()
        .any(|token| contains_token(&lowered, token))
}

/// System-catalog relations whose presence marks a statement as a driver probe
/// rather than application work.
const CATALOG_TOKENS: &[&str] = &[
    "pg_catalog",
    "information_schema",
    "pg_type",
    "pg_range",
    "pg_enum",
    "pg_attribute",
    "pg_class",
    "pg_namespace",
    "pg_proc",
    "pg_settings",
    "pg_database",
];

/// Substring search that requires identifier boundaries on both sides, so a
/// user table named `pg_types_of_wine` is not mistaken for `pg_type`.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(offset) = haystack.get(from..).and_then(|tail| tail.find(needle)) {
        let start = from.saturating_add(offset);
        let end = start.saturating_add(needle.len());
        let boundary_before = start == 0
            || !bytes
                .get(start.wrapping_sub(1))
                .copied()
                .is_some_and(is_ident);
        let boundary_after = !bytes.get(end).copied().is_some_and(is_ident);
        if boundary_before && boundary_after {
            return true;
        }
        from = start.saturating_add(1);
    }
    false
}

const fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `true` when this backend frame closes an exchange (`ReadyForQuery`).
pub const fn terminates_exchange(frame: &Frame) -> bool {
    frame.tag == TAG_READY_FOR_QUERY
}

/// Transaction status byte (`I`, `T`, `E`) of a `ReadyForQuery` frame.
#[allow(
    dead_code,
    reason = "recording and replay both treat ReadyForQuery as an opaque terminator and \
              replay its recorded bytes verbatim, so the status byte is only read by this \
              module's tests; keeping it documents what the terminator carries"
)]
pub fn ready_for_query_state(frame: &Frame) -> Option<u8> {
    if frame.tag != TAG_READY_FOR_QUERY {
        return None;
    }
    frame.payload().first().copied()
}

/// `true` for backend tags that hand flow control to the copy sub-protocol,
/// which the recorder refuses to model (F9).
pub const fn is_copy_start(tag: u8) -> bool {
    matches!(
        tag,
        TAG_COPY_IN_RESPONSE | TAG_COPY_OUT_RESPONSE | TAG_COPY_BOTH_RESPONSE
    )
}

/// Key/value of a `ParameterStatus` frame.
#[allow(
    dead_code,
    reason = "the stub server replays a recorded handshake verbatim and otherwise sends a \
              canned parameter set, so it never has to take one apart; the accessor is \
              exercised by this module's tests and is the tool for reading one back"
)]
pub fn parameter_status_pair(frame: &Frame) -> Option<(String, String)> {
    if frame.tag != TAG_PARAMETER_STATUS {
        return None;
    }
    let mut reader = Reader::new(frame.payload());
    Some((reader.cstr()?, reader.cstr()?))
}

/// `SQLSTATE` and message of an `ErrorResponse` frame.
///
/// The body is a sequence of `[field code: u8][value cstring]` pairs
/// terminated by a zero byte; only `C` (code) and `M` (message) are extracted.
pub fn error_response_fields(frame: &Frame) -> Option<(String, String)> {
    if frame.tag != TAG_ERROR_RESPONSE {
        return None;
    }
    let mut reader = Reader::new(frame.payload());
    let mut code = None;
    let mut message = None;
    loop {
        let field = reader.u8()?;
        if field == 0 {
            break;
        }
        let value = reader.cstr()?;
        match field {
            b'C' => code = Some(value),
            b'M' => message = Some(value),
            _ => {}
        }
    }
    Some((code?, message?))
}

/// Backend frame builders used by the replay stub server.
///
/// Every builder returns one complete frame — tag, length and payload — ready
/// to be written to the client half of the duplex stream. Interior NUL bytes
/// in text arguments are dropped, since they would truncate a cstring field.
pub mod build {
    /// Assemble `[tag][len][payload]`.
    fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len().saturating_add(5));
        out.push(tag);
        let len = i32::try_from(payload.len().saturating_add(4)).unwrap_or(i32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Append `text` as a NUL-terminated cstring.
    fn push_cstr(out: &mut Vec<u8>, text: &str) {
        out.extend(text.bytes().filter(|byte| *byte != 0));
        out.push(0);
    }

    /// Append a count as a big-endian `i16`, saturating on overflow.
    fn push_count(out: &mut Vec<u8>, count: usize) {
        let count = i16::try_from(count).unwrap_or(i16::MAX);
        out.extend_from_slice(&count.to_be_bytes());
    }

    /// `AuthenticationOk`.
    pub fn authentication_ok() -> Vec<u8> {
        frame(b'R', &0i32.to_be_bytes())
    }

    /// `ParameterStatus`.
    pub fn parameter_status(key: &str, value: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        push_cstr(&mut payload, key);
        push_cstr(&mut payload, value);
        frame(super::TAG_PARAMETER_STATUS, &payload)
    }

    /// `BackendKeyData`.
    pub fn backend_key_data(pid: i32, secret: i32) -> Vec<u8> {
        let mut payload = pid.to_be_bytes().to_vec();
        payload.extend_from_slice(&secret.to_be_bytes());
        frame(super::TAG_BACKEND_KEY_DATA, &payload)
    }

    /// `ReadyForQuery` with the given transaction status (`I`, `T` or `E`).
    pub fn ready_for_query(state: u8) -> Vec<u8> {
        frame(super::TAG_READY_FOR_QUERY, &[state])
    }

    /// `CommandComplete`.
    pub fn command_complete(tag: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        push_cstr(&mut payload, tag);
        frame(super::TAG_COMMAND_COMPLETE, &payload)
    }

    /// `ParseComplete`.
    pub fn parse_complete() -> Vec<u8> {
        frame(b'1', &[])
    }

    /// `BindComplete`.
    pub fn bind_complete() -> Vec<u8> {
        frame(b'2', &[])
    }

    /// `RowDescription` from `(name, type_oid)` pairs.
    ///
    /// Table OID and column attnum are reported as zero (the column is not
    /// identifiable as a table column), type length as `-1` and atttypmod as
    /// `-1`; every field is declared in text format.
    #[allow(
        dead_code,
        reason = "the stub server answers from recorded response bytes rather than \
                  synthesising result frames, so the row builders serve this module's \
                  tests and any future synthetic tape"
    )]
    pub fn row_description(cols: &[(String, u32)]) -> Vec<u8> {
        let mut payload = Vec::new();
        push_count(&mut payload, cols.len());
        for (name, type_oid) in cols {
            push_cstr(&mut payload, name);
            payload.extend_from_slice(&0i32.to_be_bytes()); // table OID
            payload.extend_from_slice(&0i16.to_be_bytes()); // column attnum
            payload.extend_from_slice(&type_oid.to_be_bytes());
            payload.extend_from_slice(&(-1i16).to_be_bytes()); // type length
            payload.extend_from_slice(&(-1i32).to_be_bytes()); // atttypmod
            payload.extend_from_slice(&0i16.to_be_bytes()); // text format
        }
        frame(super::TAG_ROW_DESCRIPTION, &payload)
    }

    /// `DataRow`; `None` is SQL NULL.
    #[allow(
        dead_code,
        reason = "companion to `row_description`: recorded tapes carry their own result \
                  bytes, so this builds rows for tests and synthetic tapes"
    )]
    pub fn data_row(fields: &[Option<Vec<u8>>]) -> Vec<u8> {
        let mut payload = Vec::new();
        push_count(&mut payload, fields.len());
        for field in fields {
            match field {
                Some(value) => {
                    let len = i32::try_from(value.len()).unwrap_or(i32::MAX);
                    payload.extend_from_slice(&len.to_be_bytes());
                    payload.extend_from_slice(value);
                }
                None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        frame(super::TAG_DATA_ROW, &payload)
    }

    /// `ParameterDescription`.
    pub fn parameter_description(oids: &[u32]) -> Vec<u8> {
        let mut payload = Vec::new();
        push_count(&mut payload, oids.len());
        for oid in oids {
            payload.extend_from_slice(&oid.to_be_bytes());
        }
        frame(b't', &payload)
    }

    /// `NoData`.
    pub fn no_data() -> Vec<u8> {
        frame(b'n', &[])
    }

    /// `EmptyQueryResponse`.
    #[allow(
        dead_code,
        reason = "an empty query is recorded like any other exchange and replayed from its \
                  own bytes; the builder completes the backend vocabulary and is exercised \
                  by this module's tests"
    )]
    pub fn empty_query_response() -> Vec<u8> {
        frame(b'I', &[])
    }

    /// `ErrorResponse` with severity `ERROR`.
    ///
    /// Carries the localised (`S`) and non-localised (`V`) severity fields,
    /// the `SQLSTATE` code (`C`) and the primary message (`M`), which is the
    /// minimum `tokio-postgres` needs to surface a `DbError`.
    pub fn error_response(code: &str, message: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(b'S');
        push_cstr(&mut payload, "ERROR");
        payload.push(b'V');
        push_cstr(&mut payload, "ERROR");
        payload.push(b'C');
        push_cstr(&mut payload, code);
        payload.push(b'M');
        push_cstr(&mut payload, message);
        payload.push(0);
        frame(super::TAG_ERROR_RESPONSE, &payload)
    }
}

/// Cap on pre-allocation driven by a wire-supplied count, so a bogus count
/// cannot make us reserve megabytes before the payload is even read.
const SANE_COUNT: usize = 64;

/// Bounds-checked forward reader over a message payload.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (head, _) = self.buf.get(self.pos..)?.split_at_checked(len)?;
        self.pos = self.pos.checked_add(len)?;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn i16(&mut self) -> Option<i16> {
        let head: [u8; 2] = self.take(2)?.try_into().ok()?;
        Some(i16::from_be_bytes(head))
    }

    /// An `i16` element count; negative counts are rejected.
    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.i16()?).ok()
    }

    fn i32(&mut self) -> Option<i32> {
        let head: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(i32::from_be_bytes(head))
    }

    fn u32(&mut self) -> Option<u32> {
        let head: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_be_bytes(head))
    }

    /// A NUL-terminated string, decoded lossily (the wire is not trusted to be
    /// valid UTF-8).
    fn cstr(&mut self) -> Option<String> {
        let rest = self.buf.get(self.pos..)?;
        let nul = rest.iter().position(|byte| *byte == 0)?;
        let (text, _) = rest.split_at_checked(nul)?;
        self.pos = self.pos.checked_add(nul)?.checked_add(1)?;
        Some(String::from_utf8_lossy(text).into_owned())
    }
}

/// Read a big-endian `u32` from the first four bytes of `bytes`.
fn be_u32(bytes: &[u8]) -> Option<u32> {
    let head: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(head))
}

/// Read a big-endian `i32` from the first four bytes of `bytes`.
fn be_i32(bytes: &[u8]) -> Option<i32> {
    let head: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(i32::from_be_bytes(head))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fixture encoders -------------------------------------------------

    fn tagged(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 5);
        out.push(tag);
        let len = i32::try_from(payload.len() + 4).unwrap();
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn cstr(s: &str) -> Vec<u8> {
        let mut out = s.as_bytes().to_vec();
        out.push(0);
        out
    }

    fn untagged(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 4);
        let len = i32::try_from(payload.len() + 4).unwrap();
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn startup_packet() -> Vec<u8> {
        let mut payload = PROTOCOL_VERSION_3.to_be_bytes().to_vec();
        payload.extend_from_slice(&cstr("user"));
        payload.extend_from_slice(&cstr("postgres"));
        payload.extend_from_slice(&cstr("database"));
        payload.extend_from_slice(&cstr("autumn"));
        payload.push(0);
        untagged(&payload)
    }

    fn ssl_request() -> Vec<u8> {
        untagged(&SSL_REQUEST_CODE.to_be_bytes())
    }

    fn parse_msg(name: &str, sql: &str, oids: &[u32]) -> Vec<u8> {
        let mut p = cstr(name);
        p.extend_from_slice(&cstr(sql));
        p.extend_from_slice(&i16::try_from(oids.len()).unwrap().to_be_bytes());
        for oid in oids {
            p.extend_from_slice(&oid.to_be_bytes());
        }
        tagged(b'P', &p)
    }

    fn bind_msg(portal: &str, statement: &str, params: &[Option<&[u8]>]) -> Vec<u8> {
        let mut p = cstr(portal);
        p.extend_from_slice(&cstr(statement));
        // One format code covering every parameter: binary.
        p.extend_from_slice(&1i16.to_be_bytes());
        p.extend_from_slice(&1i16.to_be_bytes());
        p.extend_from_slice(&i16::try_from(params.len()).unwrap().to_be_bytes());
        for param in params {
            match param {
                Some(value) => {
                    p.extend_from_slice(&i32::try_from(value.len()).unwrap().to_be_bytes());
                    p.extend_from_slice(value);
                }
                None => p.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        // One result format code: text.
        p.extend_from_slice(&1i16.to_be_bytes());
        p.extend_from_slice(&0i16.to_be_bytes());
        tagged(b'B', &p)
    }

    fn query_msg(sql: &str) -> Vec<u8> {
        tagged(b'Q', &cstr(sql))
    }

    fn describe_msg(kind: u8, name: &str) -> Vec<u8> {
        let mut p = vec![kind];
        p.extend_from_slice(&cstr(name));
        tagged(b'D', &p)
    }

    fn execute_msg(portal: &str) -> Vec<u8> {
        let mut p = cstr(portal);
        p.extend_from_slice(&0i32.to_be_bytes());
        tagged(b'E', &p)
    }

    fn copy_response(tag: u8) -> Vec<u8> {
        // overall format (text), one column, that column's format.
        let mut p = vec![0u8];
        p.extend_from_slice(&1i16.to_be_bytes());
        p.extend_from_slice(&0i16.to_be_bytes());
        tagged(tag, &p)
    }

    fn backend_fixture() -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(&build::authentication_ok());
        s.extend_from_slice(&build::parameter_status("server_version", "16.2"));
        s.extend_from_slice(&build::parameter_status("client_encoding", "UTF8"));
        s.extend_from_slice(&build::backend_key_data(4242, 99));
        s.extend_from_slice(&build::ready_for_query(b'I'));
        s.extend_from_slice(&build::parse_complete());
        s.extend_from_slice(&build::parameter_description(&[23]));
        s.extend_from_slice(&build::row_description(&[
            ("id".to_owned(), 23),
            ("name".to_owned(), 25),
        ]));
        s.extend_from_slice(&build::bind_complete());
        s.extend_from_slice(&build::data_row(&[Some(b"1".to_vec()), None]));
        s.extend_from_slice(&build::data_row(&[
            Some(b"2".to_vec()),
            Some("héllo".as_bytes().to_vec()),
        ]));
        s.extend_from_slice(&build::command_complete("SELECT 2"));
        s.extend_from_slice(&build::no_data());
        s.extend_from_slice(&build::empty_query_response());
        s.extend_from_slice(&build::error_response("58000", "autumn replay divergence"));
        s.extend_from_slice(&build::ready_for_query(b'E'));
        s
    }

    fn frontend_fixture() -> Vec<u8> {
        let mut s = startup_packet();
        s.extend_from_slice(&parse_msg(
            "s1",
            "SELECT id, name FROM users WHERE id = $1",
            &[23],
        ));
        s.extend_from_slice(&describe_msg(b'S', "s1"));
        s.extend_from_slice(&bind_msg("", "s1", &[Some(&[0, 0, 0, 7]), None]));
        s.extend_from_slice(&describe_msg(b'P', ""));
        s.extend_from_slice(&execute_msg(""));
        s.extend_from_slice(&tagged(b'S', &[]));
        s.extend_from_slice(&query_msg("BEGIN"));
        s.extend_from_slice(&tagged(b'X', &[]));
        s
    }

    fn tags(frames: &[Frame]) -> Vec<u8> {
        frames.iter().map(|f| f.tag).collect()
    }

    // ---- R3 -----------------------------------------------------------------

    #[test]
    fn splits_backend_frames_on_tag_and_length() {
        let mut stream = build::authentication_ok();
        stream.extend_from_slice(&build::parameter_status("client_encoding", "UTF8"));
        stream.extend_from_slice(&build::backend_key_data(4242, 99));
        stream.extend_from_slice(&build::ready_for_query(b'I'));

        let mut splitter = FrameSplitter::new_backend();
        let frames = splitter.push(&stream);

        assert_eq!(tags(&frames), vec![b'R', b'S', b'K', b'Z']);
        assert!(!splitter.is_unrecordable());
        assert_eq!(splitter.buffered(), 0);
        // Frames carry the full on-wire bytes, so concatenating them
        // reproduces the stream verbatim.
        let rejoined: Vec<u8> = frames.iter().flat_map(|f| f.bytes.to_vec()).collect();
        assert_eq!(rejoined, stream);
        assert_eq!(
            parameter_status_pair(frames.get(1).unwrap()),
            Some(("client_encoding".to_owned(), "UTF8".to_owned()))
        );
    }

    #[test]
    fn partial_frame_is_buffered_until_complete() {
        let mut stream = build::row_description(&[("id".to_owned(), 23)]);
        stream.extend_from_slice(&build::data_row(&[Some(b"1".to_vec())]));

        let mut splitter = FrameSplitter::new_backend();
        // A bare tag byte cannot be decoded yet.
        assert!(splitter.push(stream.get(..1).unwrap()).is_empty());
        // Neither can a tag plus a truncated length field.
        assert!(splitter.push(stream.get(1..4).unwrap()).is_empty());
        // Nor a complete header with a truncated payload.
        assert!(splitter.push(stream.get(4..8).unwrap()).is_empty());
        assert!(!splitter.is_unrecordable());
        assert!(splitter.buffered() > 0);

        let frames = splitter.push(stream.get(8..).unwrap());
        assert_eq!(tags(&frames), vec![TAG_ROW_DESCRIPTION, TAG_DATA_ROW]);
        assert_eq!(splitter.buffered(), 0);
    }

    #[test]
    fn frame_split_at_every_byte_boundary_yields_same_frames() {
        for (label, stream, backend) in [
            ("backend", backend_fixture(), true),
            ("frontend", frontend_fixture(), false),
        ] {
            let mut whole = if backend {
                FrameSplitter::new_backend()
            } else {
                FrameSplitter::new_frontend()
            };
            let expected = whole.push(&stream);
            assert!(
                !expected.is_empty(),
                "{label}: fixture produced no frames at all"
            );
            assert!(!whole.is_unrecordable(), "{label}: fixture unrecordable");
            assert_eq!(whole.buffered(), 0, "{label}: fixture left a partial frame");

            for split in 0..=stream.len() {
                let mut splitter = if backend {
                    FrameSplitter::new_backend()
                } else {
                    FrameSplitter::new_frontend()
                };
                let mut got = splitter.push(stream.get(..split).unwrap());
                got.extend(splitter.push(stream.get(split..).unwrap()));
                assert_eq!(got, expected, "{label}: mismatch splitting at {split}");
                assert_eq!(
                    splitter.buffered(),
                    0,
                    "{label}: leftover bytes splitting at {split}"
                );
            }

            // And one byte at a time.
            let mut splitter = if backend {
                FrameSplitter::new_backend()
            } else {
                FrameSplitter::new_frontend()
            };
            let mut got = Vec::new();
            for byte in &stream {
                got.extend(splitter.push(&[*byte]));
            }
            assert_eq!(
                got, expected,
                "{label}: mismatch feeding one byte at a time"
            );
        }
    }

    #[test]
    fn parses_parse_message_name_sql_and_param_oids() {
        let mut splitter = FrameSplitter::new_frontend();
        let _ = splitter.push(&startup_packet());
        let frames = splitter.push(&parse_msg("s3", "SELECT $1::int4, $2::text", &[23, 25]));
        let frame = frames.first().expect("one Parse frame");

        assert_eq!(
            parse_frontend(frame),
            FrontendMessage::Parse {
                name: "s3".to_owned(),
                sql: "SELECT $1::int4, $2::text".to_owned(),
                param_oids: vec![23, 25],
            }
        );
    }

    #[test]
    fn parses_bind_statement_name_and_parameter_values() {
        let mut splitter = FrameSplitter::new_frontend();
        let _ = splitter.push(&startup_packet());
        let frames = splitter.push(&bind_msg(
            "",
            "s3",
            &[Some(&[0, 0, 0, 42]), None, Some(b"")],
        ));
        let frame = frames.first().expect("one Bind frame");

        assert_eq!(
            parse_frontend(frame),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: "s3".to_owned(),
                params: vec![Some(vec![0, 0, 0, 42]), None, Some(Vec::new())],
            }
        );
    }

    #[test]
    fn parses_simple_query_text() {
        let mut splitter = FrameSplitter::new_frontend();
        let _ = splitter.push(&startup_packet());
        let frames = splitter.push(&query_msg("BEGIN; SELECT 1; COMMIT"));
        let frame = frames.first().expect("one Query frame");

        assert_eq!(
            parse_frontend(frame),
            FrontendMessage::Query("BEGIN; SELECT 1; COMMIT".to_owned())
        );
    }

    #[test]
    fn recognizes_ready_for_query_as_exchange_terminator() {
        let mut splitter = FrameSplitter::new_backend();
        let mut stream = build::command_complete("SELECT 1");
        stream.extend_from_slice(&build::ready_for_query(b'T'));
        let frames = splitter.push(&stream);

        let complete = frames.first().expect("CommandComplete");
        let ready = frames.get(1).expect("ReadyForQuery");
        assert!(!terminates_exchange(complete));
        assert!(terminates_exchange(ready));
        assert_eq!(ready.tag, TAG_READY_FOR_QUERY);
        assert_eq!(ready_for_query_state(ready), Some(b'T'));
        assert_eq!(ready_for_query_state(complete), None);
    }

    #[test]
    fn recognizes_capsule_marker_and_extracts_request_id() {
        assert_eq!(
            marker_request_id("SET autumn.capsule_request = 'req-4f2a_01'"),
            Some(MarkerId::Set("req-4f2a_01".to_owned()))
        );
        // The marker rides in the same batch as the statement timeout.
        assert_eq!(
            marker_request_id(
                "SET statement_timeout = 5000; SET autumn.capsule_request = 'abc123'"
            ),
            Some(MarkerId::Set("abc123".to_owned()))
        );
        // Case-insensitive keywords and the `TO` spelling.
        assert_eq!(
            marker_request_id("set AUTUMN.CAPSULE_REQUEST to 'Zz9'"),
            Some(MarkerId::Set("Zz9".to_owned()))
        );
        // Reached through the frontend parser, as the recorder will.
        let mut splitter = FrameSplitter::new_frontend();
        let _ = splitter.push(&startup_packet());
        let frames = splitter.push(&query_msg(
            "SET statement_timeout = 5000; SET autumn.capsule_request = 'r-1'",
        ));
        let FrontendMessage::Query(sql) = parse_frontend(frames.first().expect("Query frame"))
        else {
            panic!("expected a simple Query message");
        };
        assert_eq!(
            marker_request_id(&sql),
            Some(MarkerId::Set("r-1".to_owned()))
        );
        // Unrelated SQL carries no marker.
        assert_eq!(marker_request_id("SELECT * FROM users"), None);
        assert_eq!(marker_request_id("SET statement_timeout = 5000"), None);
    }

    #[test]
    fn empty_marker_clears_binding() {
        assert_eq!(
            marker_request_id("SET autumn.capsule_request = ''"),
            Some(MarkerId::Clear)
        );
        assert_eq!(
            marker_request_id("SET statement_timeout = 5000; SET autumn.capsule_request = ''"),
            Some(MarkerId::Clear)
        );
        // A later clear wins over an earlier binding.
        assert_eq!(
            marker_request_id("SET autumn.capsule_request = 'a1'; SET autumn.capsule_request = ''"),
            Some(MarkerId::Clear)
        );
    }

    #[test]
    fn rejects_unsafe_marker_id() {
        // Quote-escaped injection attempt inside the literal.
        assert_eq!(
            marker_request_id("SET autumn.capsule_request = 'a''; DROP TABLE users; --'"),
            Some(MarkerId::Invalid)
        );
        assert_eq!(
            marker_request_id("SET autumn.capsule_request = 'has space'"),
            Some(MarkerId::Invalid)
        );
        let long = "x".repeat(65);
        assert_eq!(
            marker_request_id(&format!("SET autumn.capsule_request = '{long}'")),
            Some(MarkerId::Invalid)
        );

        assert!(is_valid_marker_id("abc-123_XYZ"));
        assert!(is_valid_marker_id(&"x".repeat(64)));
        assert!(!is_valid_marker_id(""));
        assert!(!is_valid_marker_id(&"x".repeat(65)));
        assert!(!is_valid_marker_id("a'b"));
        assert!(!is_valid_marker_id("a b"));
        assert!(!is_valid_marker_id("naïve"));

        // The builder refuses to interpolate anything it would then reject.
        assert_eq!(
            marker_set_sql("r-1").as_deref(),
            Some("SET autumn.capsule_request = 'r-1'")
        );
        assert_eq!(
            marker_set_sql("").as_deref(),
            Some("SET autumn.capsule_request = ''")
        );
        assert_eq!(marker_set_sql("a'; DROP TABLE users; --"), None);
        // Whatever the builder emits, the scanner reads back identically.
        let sql = marker_set_sql("r-1").expect("valid id");
        assert_eq!(
            marker_request_id(&sql),
            Some(MarkerId::Set("r-1".to_owned()))
        );
    }

    #[test]
    fn oversized_accumulator_marks_connection_unrecordable() {
        // A length field claiming far more than the accumulator budget must be
        // refused outright rather than buffered.
        let mut splitter = FrameSplitter::new_backend();
        let mut header = vec![TAG_DATA_ROW];
        header.extend_from_slice(&i32::try_from(MAX_FRAME_ACCUM + 1).unwrap().to_be_bytes());
        let frames = splitter.push(&header);

        assert!(frames.is_empty());
        assert!(splitter.is_unrecordable());
        assert_eq!(splitter.buffered(), 0);
        // Once unrecordable it stays that way and emits nothing further.
        assert!(splitter.push(&build::ready_for_query(b'I')).is_empty());
        assert!(splitter.is_unrecordable());

        // A frontend startup packet with an absurd length is refused too.
        let mut fe = FrameSplitter::new_frontend();
        assert!(
            fe.push(&i32::try_from(MAX_FRAME_ACCUM + 1).unwrap().to_be_bytes())
                .is_empty()
        );
        assert!(fe.is_unrecordable());
    }

    #[test]
    fn copy_in_response_marks_connection_unrecordable() {
        for tag in [
            TAG_COPY_IN_RESPONSE,
            TAG_COPY_OUT_RESPONSE,
            TAG_COPY_BOTH_RESPONSE,
        ] {
            assert!(is_copy_start(tag), "tag {tag} should start a copy");
            let mut splitter = FrameSplitter::new_backend();
            let _ = splitter.push(&build::authentication_ok());
            assert!(!splitter.is_unrecordable());
            let frames = splitter.push(&copy_response(tag));
            assert_eq!(tags(&frames), vec![tag]);
            assert!(
                splitter.is_unrecordable(),
                "backend tag {tag} must mark the connection unrecordable"
            );
        }

        // The *frontend* Flush tag collides with CopyOutResponse and must not
        // trip the guard.
        let mut fe = FrameSplitter::new_frontend();
        let _ = fe.push(&startup_packet());
        let frames = fe.push(&tagged(b'H', &[]));
        assert_eq!(tags(&frames), vec![b'H']);
        assert_eq!(
            parse_frontend(frames.first().unwrap()),
            FrontendMessage::Flush
        );
        assert!(!fe.is_unrecordable());
    }

    // ---- supporting coverage ----------------------------------------------

    #[test]
    fn frontend_startup_phase_is_untagged_then_tagged_forever() {
        let mut splitter = FrameSplitter::new_frontend();

        let frames = splitter.push(&ssl_request());
        let ssl = frames.first().expect("SSLRequest frame");
        assert_eq!(ssl.tag, TAG_UNTAGGED);
        assert_eq!(ssl.bytes.len(), 8);
        assert_eq!(ssl.startup_code(), Some(SSL_REQUEST_CODE));
        assert_eq!(parse_frontend(ssl), FrontendMessage::SslRequest);

        // Still untagged: the real startup packet follows the SSL refusal.
        let frames = splitter.push(&startup_packet());
        let startup = frames.first().expect("startup frame");
        assert_eq!(startup.tag, TAG_UNTAGGED);
        assert_eq!(startup.startup_code(), Some(PROTOCOL_VERSION_3));
        assert_eq!(parse_frontend(startup), FrontendMessage::Startup);

        // Tagged from here on: a 'P' is a Parse, not another startup packet.
        let frames = splitter.push(&parse_msg("", "SELECT 1", &[]));
        assert_eq!(
            parse_frontend(frames.first().expect("Parse frame")),
            FrontendMessage::Parse {
                name: String::new(),
                sql: "SELECT 1".to_owned(),
                param_oids: Vec::new(),
            }
        );
    }

    #[test]
    fn bare_ssl_refusal_byte_is_its_own_backend_frame() {
        let mut splitter = FrameSplitter::new_backend();
        let frames = splitter.push(b"N");
        let refusal = frames.first().expect("SSL refusal frame");
        assert_eq!(refusal.tag, TAG_UNTAGGED);
        assert_eq!(refusal.ssl_answer(), Some(b'N'));
        assert!(!splitter.is_unrecordable());

        // The stream is ordinary tagged traffic afterwards.
        let frames = splitter.push(&build::authentication_ok());
        assert_eq!(tags(&frames), vec![b'R']);

        // Coalesced into one read, the refusal is still split off correctly.
        let mut coalesced = b"N".to_vec();
        coalesced.extend_from_slice(&build::authentication_ok());
        coalesced.extend_from_slice(&build::ready_for_query(b'I'));
        let mut splitter = FrameSplitter::new_backend();
        let frames = splitter.push(&coalesced);
        assert_eq!(tags(&frames), vec![TAG_UNTAGGED, b'R', b'Z']);

        // A genuine NoticeResponse first message is NOT mistaken for a refusal.
        let mut splitter = FrameSplitter::new_backend();
        let notice = tagged(b'N', &[b'S', b'W', b'A', b'R', b'N', 0, 0]);
        let frames = splitter.push(&notice);
        let frame = frames.first().expect("NoticeResponse frame");
        assert_eq!(frame.tag, b'N');
        assert_eq!(frame.bytes.len(), notice.len());

        // A bare 'S' means TLS ciphertext follows, which we cannot model.
        let mut splitter = FrameSplitter::new_backend();
        let _ = splitter.push(b"S");
        assert!(splitter.is_unrecordable());
    }

    #[test]
    fn builders_round_trip_through_the_splitter() {
        let mut splitter = FrameSplitter::new_backend();
        let frames = splitter.push(&backend_fixture());
        assert_eq!(
            tags(&frames),
            vec![
                b'R', b'S', b'S', b'K', b'Z', b'1', b't', b'T', b'2', b'D', b'D', b'C', b'n', b'I',
                b'E', b'Z'
            ]
        );

        // AuthenticationOk: length 8, subtype 0.
        let auth = frames.first().unwrap();
        assert_eq!(auth.bytes.as_ref(), &[b'R', 0, 0, 0, 8, 0, 0, 0, 0]);

        // BackendKeyData: pid then secret.
        let key = frames.get(3).unwrap();
        assert_eq!(key.payload().len(), 8);
        assert_eq!(be_u32(key.payload()), Some(4242));

        // ReadyForQuery: one status byte.
        assert_eq!(ready_for_query_state(frames.get(4).unwrap()), Some(b'I'));

        // ParameterDescription: count then OIDs.
        let params = frames.get(6).unwrap();
        assert_eq!(params.payload(), &[0, 1, 0, 0, 0, 23]);

        // RowDescription: two fields, each 18 bytes of metadata after its name.
        let row_desc = frames.get(7).unwrap();
        let payload = row_desc.payload();
        assert_eq!(payload.get(..2), Some([0u8, 2].as_slice()));
        assert_eq!(payload.get(2..5), Some(b"id\0".as_slice()));
        // table oid 0, attnum 0, type oid 23, typlen -1, atttypmod -1, format 0
        assert_eq!(
            payload.get(5..23),
            Some(
                [
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 23, 255, 255, 255, 255, 255, 255, 0, 0
                ]
                .as_slice()
            )
        );

        // DataRow with a NULL: field count 2, then len -1 for the NULL.
        let null_row = frames.get(9).unwrap();
        assert_eq!(
            null_row.payload(),
            &[0, 2, 0, 0, 0, 1, b'1', 255, 255, 255, 255]
        );

        // CommandComplete carries a cstring tag.
        assert_eq!(frames.get(11).unwrap().payload(), b"SELECT 2\0");

        // NoData and EmptyQueryResponse are payload-free.
        assert!(frames.get(12).unwrap().payload().is_empty());
        assert!(frames.get(13).unwrap().payload().is_empty());

        // ErrorResponse fields are readable back out.
        assert_eq!(
            error_response_fields(frames.get(14).unwrap()),
            Some(("58000".to_owned(), "autumn replay divergence".to_owned()))
        );
        assert_eq!(error_response_fields(frames.get(11).unwrap()), None);

        assert!(!splitter.is_unrecordable());
        assert_eq!(splitter.buffered(), 0);
    }

    #[test]
    fn detects_catalog_probes() {
        assert!(is_catalog_sql(
            "SELECT t.oid, t.typname FROM pg_catalog.pg_type t WHERE t.oid = $1"
        ));
        assert!(is_catalog_sql("select * from PG_TYPE"));
        assert!(is_catalog_sql(
            "SELECT column_name FROM information_schema.columns"
        ));
        assert!(!is_catalog_sql("SELECT id, name FROM users WHERE id = $1"));
        // Word boundaries: a user table that merely starts the same way is not
        // a catalog probe.
        assert!(!is_catalog_sql("SELECT * FROM pg_types_of_wine"));
        assert!(!is_catalog_sql("SELECT * FROM my_pg_class_audit"));
    }

    #[test]
    fn malformed_payloads_degrade_to_other_instead_of_panicking() {
        let truncated = Frame {
            tag: b'P',
            bytes: Bytes::from_static(&[b'P', 0, 0, 0, 6, b'x']),
        };
        assert_eq!(parse_frontend(&truncated), FrontendMessage::Other(b'P'));

        let empty_bind = Frame {
            tag: b'B',
            bytes: Bytes::from_static(&[b'B', 0, 0, 0, 4]),
        };
        assert_eq!(parse_frontend(&empty_bind), FrontendMessage::Other(b'B'));

        let unknown = Frame {
            tag: b'@',
            bytes: Bytes::from_static(&[b'@', 0, 0, 0, 4]),
        };
        assert_eq!(parse_frontend(&unknown), FrontendMessage::Other(b'@'));
    }

    #[test]
    fn short_length_field_marks_connection_unrecordable() {
        let mut splitter = FrameSplitter::new_backend();
        // A tagged frame declaring len < 4 is a protocol violation.
        let frames = splitter.push(&[b'D', 0, 0, 0, 3, 0, 0, 0]);
        assert!(frames.is_empty());
        assert!(splitter.is_unrecordable());
    }

    #[test]
    fn frontend_control_messages_are_recognized() {
        let mut splitter = FrameSplitter::new_frontend();
        let _ = splitter.push(&startup_packet());
        let mut stream = describe_msg(b'S', "s1");
        stream.extend_from_slice(&execute_msg("p1"));
        stream.extend_from_slice(&tagged(b'S', &[]));
        stream.extend_from_slice(&tagged(b'C', &[b'S', 0]));
        stream.extend_from_slice(&tagged(b'X', &[]));
        let frames = splitter.push(&stream);
        let parsed: Vec<FrontendMessage> = frames.iter().map(parse_frontend).collect();
        assert_eq!(
            parsed,
            vec![
                FrontendMessage::Describe {
                    kind: b'S',
                    name: "s1".to_owned()
                },
                FrontendMessage::Execute,
                FrontendMessage::Sync,
                FrontendMessage::Close {
                    kind: b'S',
                    name: String::new()
                },
                FrontendMessage::Terminate,
            ]
        );
    }
}
