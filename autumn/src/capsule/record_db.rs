//! Wire-level recording of the database traffic one request produced.
//!
//! Capture works by *teeing*, not by wrapping the query API. A pooled
//! connection is established over a [`RecordingStream`] — an
//! `AsyncRead + AsyncWrite` shim around the TCP socket — and every byte in both
//! directions is handed to a [`ConnectionRecorder`], which reassembles
//! `PostgreSQL` protocol frames with [`crate::capsule::wire`] and groups them
//! into [`Exchange`]s. Nothing about diesel, the pool, or the handler changes.
//!
//! # Attribution
//!
//! A pooled connection outlives the request that borrowed it, so the recorder
//! cannot tell whose traffic it is watching from the socket alone.
//! [`Db::checkout`](crate::db::Db::checkout) therefore sends
//! `SET autumn.capsule_request = '<capsule id>'` — merged into the same round
//! trip as `SET statement_timeout`, so capture costs no extra latency. The
//! recorder reads that marker off the wire and binds the connection to the
//! scope it names until the *next* marker replaces it. A checkout with no
//! capture scope sends the clearing form (`''`), so work that belongs to nobody
//! can never be attributed to whoever held the connection last (F2). The marker
//! exchange itself is never recorded: replaying it would re-bind a connection.
//!
//! # The connection memo
//!
//! A capsule must replay against a *cold* stub server, but it was recorded on a
//! connection that had already been used: its prepared statements were cached,
//! its session already configured, its type-info lookups already done. The
//! [`ConnectionMemo`] keeps that history — the birth-to-first-marker
//! `prologue`, the `Parse`/`Describe` metadata keyed by SQL, and the
//! `pg_catalog` lookups — for the life of the connection and copies it into
//! *every* capsule recorded on it. Without it, the second request served by a
//! pooled connection would record a `Bind` referring to a prepared statement no
//! replay could produce (F3).
//!
//! # When capture steps aside
//!
//! Capture needs a plaintext TCP connection it can frame. A TLS-required URL
//! (F7), a Unix-socket URL, or an application that installed its own
//! [`DatabasePoolProvider`](crate::db::DatabasePoolProvider) (F25) all disable
//! DB capture: an ordinary pool is built, a warning is logged, and every
//! capsule carries a note saying why it has no database tape.

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

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use diesel::{ConnectionError, ConnectionResult};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::pooled_connection::{AsyncDieselConnectionManager, ManagerConfig};
use futures::FutureExt as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::capsule::capture::{CaptureScope, scope_by_id};
use crate::capsule::schema::{BindValue, ConnectionTape, Exchange, ExchangeProtocol};
use crate::capsule::wire::{self, FrameSplitter, FrontendMessage, MarkerId};
use crate::config::{AutumnConfig, DatabaseConfig};
use crate::db::{DatabaseTopology, PoolError};

/// Most remembered entries of one memo bucket. A connection that prepares
/// thousands of distinct statements gets a partial memo rather than unbounded
/// growth; replay reports the resulting gap as a divergence.
const MAX_MEMO_ENTRIES: usize = 256;

/// Byte ceiling on one connection's memo, independent of the per-capsule
/// budget (the memo lives as long as the connection does).
const MAX_MEMO_BYTES: usize = 1024 * 1024;

/// Most exchanges awaiting a response on one connection. `AsyncPgConnection`
/// takes `&mut self` per query so application traffic is strictly sequential,
/// but diesel-async's own session setup pipelines two statements — a small
/// queue is normal, an unbounded one is not (F10).
const MAX_IN_FLIGHT: usize = 64;

/// Fallback port when a connection string names a host but no port.
const DEFAULT_PG_PORT: u16 = 5432;

/// Rough per-exchange bookkeeping overhead charged against the capsule budget
/// on top of the SQL and response bytes.
const EXCHANGE_OVERHEAD_BYTES: usize = 64;

// ── Capture eligibility ─────────────────────────────────────────────────────

/// The reason DB capture is off for this process, once something turned it off.
static UNAVAILABLE_NOTE: OnceLock<String> = OnceLock::new();

/// Why `url` cannot be recorded, or `None` when it can.
///
/// Recording needs a plaintext TCP stream: the tee frames `PostgreSQL` protocol
/// messages, and TLS ciphertext (or a Unix socket, which has no stream to
/// reconnect during replay) cannot be framed.
#[must_use]
pub fn capture_unavailable_reason(url: &str) -> Option<String> {
    if !matches!(
        crate::db::tls::TlsPosture::from_database_url(url),
        crate::db::tls::TlsPosture::Off
    ) {
        return Some(
            "the database URL asks for TLS (sslmode), and capture cannot frame an \
             encrypted connection"
                .to_owned(),
        );
    }
    let Ok(config) = url.parse::<tokio_postgres::Config>() else {
        return Some("the database URL is not a PostgreSQL connection string".to_owned());
    };
    if first_tcp_endpoint(&config).is_none() {
        return Some(
            "the database URL names no TCP host (a Unix-socket connection cannot be teed)"
                .to_owned(),
        );
    }
    None
}

/// Record — once per process — why DB capture is unavailable, and say so.
fn disable_db_capture(reason: &str) {
    tracing::warn!(
        reason,
        "failure-capsule database capture is disabled; capsules will record the request, \
         clock and outcome but no database traffic"
    );
    let _ = UNAVAILABLE_NOTE.set(format!("db capture unavailable: {reason}"));
}

/// The process-wide "no database tape, and here is why" note, when one applies.
#[must_use]
pub fn db_capture_unavailable_note() -> Option<&'static str> {
    UNAVAILABLE_NOTE.get().map(String::as_str)
}

/// Copy that note onto a capture scope, so the capsule explains itself.
///
/// Called from the connection-checkout path, which is the one place that knows
/// a request is about to use a database at all.
pub fn note_db_capture_unavailable(scope: &CaptureScope) {
    if let Some(note) = UNAVAILABLE_NOTE.get() {
        scope.note(note.clone());
    }
}

// ── Pool construction ───────────────────────────────────────────────────────

/// The boot-time pool factory shape `App::run` stores in its provider slot.
pub type CapturePoolProvider = Box<
    dyn FnOnce(
            DatabaseConfig,
        )
            -> Pin<Box<dyn Future<Output = Result<Option<DatabaseTopology>, PoolError>> + Send>>
        + Send,
>;

/// Build a pool whose connections tee their `PostgreSQL` wire traffic into the
/// capsule of whichever request currently holds them.
///
/// The pool is an ordinary `deadpool` pool of `AsyncPgConnection`s — callers
/// cannot tell it apart from [`crate::db::create_pool`]'s — built through a
/// `custom_setup` callback that opens the socket itself so it can wrap it.
///
/// # Errors
///
/// Returns [`PoolError::UnsupportedBackend`] when `url` is not recordable (see
/// [`capture_unavailable_reason`]), and [`PoolError::Build`] when the pool
/// itself cannot be constructed.
pub fn build_recording_pool(
    url: &str,
    max_size: usize,
    connect_timeout: Duration,
) -> Result<Pool<AsyncPgConnection>, PoolError> {
    if let Some(reason) = capture_unavailable_reason(url) {
        return Err(PoolError::UnsupportedBackend(format!(
            "a failure-capsule recording pool cannot be built for this database URL: {reason}"
        )));
    }
    let mut manager_config = ManagerConfig::<AsyncPgConnection>::default();
    manager_config.custom_setup = Box::new(|url: &str| {
        let url = url.to_owned();
        async move { establish_recording(&url).await }.boxed()
    });
    let manager =
        AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(url, manager_config);
    Ok(Pool::builder(manager)
        .max_size(max_size.max(1))
        .wait_timeout(Some(connect_timeout))
        .create_timeout(Some(connect_timeout))
        .runtime(deadpool::Runtime::Tokio1)
        .build()?)
}

/// Open one recorded connection: raw socket → tee → `tokio-postgres` →
/// `AsyncPgConnection`.
///
/// `AsyncPgConnection::try_from_client_and_connection` cannot be used here —
/// its `Connection<Socket, S>` bound rules out a custom stream type — so the
/// connection future is driven by a task we spawn ourselves and
/// [`AsyncPgConnection::try_from`] adopts the client. The cost is that
/// diesel-async's notification stream and connection-error broadcast are not
/// wired up on capture pools; `LISTEN`/`NOTIFY` is documented as unsupported
/// there, and Autumn's own listener uses a dedicated connection.
async fn establish_recording(url: &str) -> ConnectionResult<AsyncPgConnection> {
    let config = url
        .parse::<tokio_postgres::Config>()
        .map_err(|error| ConnectionError::InvalidConnectionUrl(error.to_string()))?;
    let (host, port) = first_tcp_endpoint(&config).ok_or_else(|| {
        ConnectionError::InvalidConnectionUrl(
            "the recording pool needs a TCP host in the database URL".to_owned(),
        )
    })?;

    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|error| {
            ConnectionError::BadConnection(format!("failed to connect to {host}:{port}: {error}"))
        })?;
    // Match what tokio-postgres's own connector does: PostgreSQL is a
    // request/response protocol, so Nagle only adds latency.
    let _ = stream.set_nodelay(true);

    let (client, connection) = config
        .connect_raw(RecordingStream::new(stream), tokio_postgres::NoTls)
        .await
        .map_err(|error| ConnectionError::BadConnection(error.to_string()))?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "recorded database connection ended");
        }
    });

    AsyncPgConnection::try_from(client).await
}

/// The first TCP host/port pair in a parsed connection string.
fn first_tcp_endpoint(config: &tokio_postgres::Config) -> Option<(String, u16)> {
    let ports = config.get_ports();
    config
        .get_hosts()
        .iter()
        .enumerate()
        .find_map(|(index, host)| match host {
            tokio_postgres::config::Host::Tcp(name) => Some((
                name.clone(),
                ports
                    .get(index)
                    .or_else(|| ports.first())
                    .copied()
                    .unwrap_or(DEFAULT_PG_PORT),
            )),
            // A Unix-socket host: nothing to tee, and replay has no socket to
            // stand in for it.
            tokio_postgres::config::Host::Unix(_) => None,
        })
}

/// Decide whether `App::run` should build its database pools through the
/// recording factory.
///
/// Returns the provider slot to use. An application that installed its own
/// [`DatabasePoolProvider`](crate::db::DatabasePoolProvider) is left completely
/// alone — Autumn will not second-guess a custom pool — at the cost of DB
/// capture, which is logged and noted on every capsule (F25).
#[must_use]
pub fn maybe_capture_pool_provider(
    existing: Option<CapturePoolProvider>,
    config: &AutumnConfig,
) -> Option<CapturePoolProvider> {
    if !config.failure_capture.enabled {
        return existing;
    }
    if existing.is_some() {
        disable_db_capture(
            "the application installed a custom DatabasePoolProvider, which Autumn does \
             not wrap",
        );
        return existing;
    }
    Some(Box::new(|database: DatabaseConfig| {
        Box::pin(async move { recording_topology(&database) })
    }))
}

/// Build the control topology with recording pools, or fall back to ordinary
/// ones when any configured role cannot be recorded.
fn recording_topology(config: &DatabaseConfig) -> Result<Option<DatabaseTopology>, PoolError> {
    let Some(primary_url) = config.effective_primary_url() else {
        return Ok(None);
    };
    let blocked = capture_unavailable_reason(primary_url).or_else(|| {
        config
            .replica_url
            .as_deref()
            .and_then(capture_unavailable_reason)
    });
    if let Some(reason) = blocked {
        disable_db_capture(&reason);
        return crate::db::create_topology(config);
    }

    let timeout = Duration::from_secs(config.connect_timeout_secs);
    let primary = build_recording_pool(primary_url, config.effective_primary_pool_size(), timeout)?;
    let replica = config
        .replica_url
        .as_deref()
        .map(|url| build_recording_pool(url, config.effective_replica_pool_size(), timeout))
        .transpose()?;
    Ok(Some(DatabaseTopology::from_pools(primary, replica)))
}

// ── The tee ─────────────────────────────────────────────────────────────────

/// Connection ids are process-wide so a capsule's tapes are distinguishable
/// even when several connections served one request.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// An `AsyncRead + AsyncWrite` shim that copies everything crossing it into a
/// [`ConnectionRecorder`].
///
/// The shim is deliberately transparent: it never buffers, never reorders and
/// never fails on its own. If the recorder cannot make sense of the stream it
/// stops recording; the connection carries on regardless.
#[derive(Debug)]
pub struct RecordingStream<S> {
    inner: S,
    recorder: ConnectionRecorder,
}

impl<S> RecordingStream<S> {
    /// Wrap `inner`, assigning the connection a fresh recorder.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            recorder: ConnectionRecorder::new(NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)),
        }
    }

    /// This connection's recorder-assigned identifier.
    #[must_use]
    pub const fn connection_id(&self) -> u64 {
        self.recorder.id
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for RecordingStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) {
            let fresh = buf.filled().get(before..).unwrap_or_default();
            if !fresh.is_empty() {
                this.recorder.on_backend(fresh);
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for RecordingStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &polled {
            let written = *written;
            if let Some(chunk) = buf.get(..written) {
                this.recorder.on_frontend(chunk);
            }
        }
        polled
    }

    /// Tee a vectored write too.
    ///
    /// [`Self::is_write_vectored`] reports `false`, so a well-behaved writer
    /// never takes this path — but `AsyncWriteExt::write_vectored` can be
    /// called directly, and a tee that silently missed those bytes would
    /// produce a capsule that looks complete and replays wrong (F5).
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(written)) = &polled {
            let mut remaining = *written;
            for slice in bufs {
                if remaining == 0 {
                    break;
                }
                let take = remaining.min(slice.len());
                if let Some(chunk) = slice.get(..take) {
                    this.recorder.on_frontend(chunk);
                }
                remaining = remaining.saturating_sub(take);
            }
        }
        polled
    }

    /// Always `false`: the tee sees a single contiguous buffer per write, which
    /// keeps the framing scan simple and the recorded byte order unambiguous.
    fn is_write_vectored(&self) -> bool {
        false
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ── The recorder ────────────────────────────────────────────────────────────

/// Which part of a tape an exchange belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bucket {
    /// Connection birth, up to the first attribution marker.
    Prologue,
    /// `Parse`/`Describe` metadata, keyed by SQL.
    Statements,
    /// `pg_catalog` / `information_schema` probes.
    Catalog,
    /// The request's own work.
    Request,
}

/// An exchange being assembled from the frames of one round trip.
#[derive(Debug)]
struct Pending {
    protocol: ExchangeProtocol,
    sql: String,
    binds: Vec<BindValue>,
    has_parse: bool,
    has_bind: bool,
    /// Connection bookkeeping rather than work the request did: the
    /// attribution marker, or a `Close`. Tracked so its `ReadyForQuery` is
    /// consumed by its own slot in the queue, then discarded.
    housekeeping: bool,
    response: Vec<u8>,
    row_count: usize,
    error: Option<String>,
}

impl Pending {
    const fn extended() -> Self {
        Self {
            protocol: ExchangeProtocol::Extended,
            sql: String::new(),
            binds: Vec::new(),
            has_parse: false,
            has_bind: false,
            housekeeping: false,
            response: Vec::new(),
            row_count: 0,
            error: None,
        }
    }

    fn simple(sql: String) -> Self {
        Self {
            protocol: ExchangeProtocol::Simple,
            sql,
            ..Self::extended()
        }
    }

    fn into_exchange(self) -> Exchange {
        Exchange {
            protocol: self.protocol,
            sql: self.sql,
            binds: self.binds,
            response: self.response,
            row_count: self.row_count,
            error: self.error,
        }
    }
}

/// A connection's history, replayed into every capsule recorded on it (F3).
///
/// The buckets are deduplicated by SQL (and bind values, for catalog probes)
/// and bounded both in entries and in bytes, because a long-lived connection
/// would otherwise accumulate one entry per distinct statement forever.
#[derive(Debug, Default)]
pub struct ConnectionMemo {
    prologue: Vec<Exchange>,
    statements: Vec<Exchange>,
    catalog: Vec<Exchange>,
    bytes: usize,
}

impl ConnectionMemo {
    /// Remember `exchange`, replacing any earlier entry for the same statement.
    fn remember(&mut self, bucket: Bucket, exchange: &Exchange) {
        let cost = exchange_bytes(exchange);
        let entries = match bucket {
            Bucket::Prologue => &mut self.prologue,
            Bucket::Statements => &mut self.statements,
            Bucket::Catalog => &mut self.catalog,
            // A request's own exchanges belong to one capsule, never to the
            // connection's history.
            Bucket::Request => return,
        };
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.sql == exchange.sql && entry.binds == exchange.binds)
        {
            self.bytes = self
                .bytes
                .saturating_sub(exchange_bytes(existing))
                .saturating_add(cost);
            *existing = exchange.clone();
            return;
        }
        if entries.len() >= MAX_MEMO_ENTRIES || self.bytes.saturating_add(cost) > MAX_MEMO_BYTES {
            return;
        }
        entries.push(exchange.clone());
        self.bytes = self.bytes.saturating_add(cost);
    }
}

/// Reassembles one connection's byte stream into attributed exchanges.
///
/// One recorder lives inside one [`RecordingStream`], so all of its state is
/// per-connection and needs no locking; the only shared thing it touches is the
/// [`CaptureScope`] it is currently bound to.
#[derive(Debug)]
pub struct ConnectionRecorder {
    id: u64,
    frontend: FrameSplitter,
    backend: FrameSplitter,
    /// Prepared-statement name → SQL, so a later `Bind` naming only the
    /// statement still records what it runs.
    statement_sql: HashMap<String, String>,
    /// The extended-protocol exchange whose `Sync` has not arrived yet.
    building: Option<Pending>,
    /// Exchanges awaiting (or receiving) their response, oldest first.
    in_flight: VecDeque<Pending>,
    memo: ConnectionMemo,
    /// The capsule scope this connection is currently attributed to.
    ///
    /// Weak on purpose: a pooled connection outlives the request that bound it,
    /// and holding the scope strongly would keep a finished request's buffered
    /// capsule alive until the connection is next checked out.
    bound: Option<Weak<CaptureScope>>,
    /// `true` until the first attribution marker; everything before it is the
    /// connection's prologue.
    before_first_marker: bool,
    /// Set when the capsule budget ran out; recording resumes at the next
    /// window.
    stopped: bool,
    /// Set when the stream stopped making sense; this connection is done.
    disabled: bool,
}

impl ConnectionRecorder {
    fn new(id: u64) -> Self {
        Self {
            id,
            frontend: FrameSplitter::new_frontend(),
            backend: FrameSplitter::new_backend(),
            statement_sql: HashMap::new(),
            building: None,
            in_flight: VecDeque::new(),
            memo: ConnectionMemo::default(),
            bound: None,
            before_first_marker: true,
            stopped: false,
            disabled: false,
        }
    }

    /// Observe bytes the client wrote.
    /// The scope this connection is attributed to, if it is still alive.
    fn scope(&self) -> Option<Arc<CaptureScope>> {
        self.bound.as_ref().and_then(Weak::upgrade)
    }

    fn on_frontend(&mut self, bytes: &[u8]) {
        if self.disabled {
            return;
        }
        let frames = self.frontend.push(bytes);
        if self.frontend.is_unrecordable() {
            self.give_up("the client stream could not be framed");
            return;
        }
        for frame in frames {
            self.on_frontend_frame(&frame);
        }
    }

    /// Observe bytes the server wrote.
    fn on_backend(&mut self, bytes: &[u8]) {
        if self.disabled {
            return;
        }
        let frames = self.backend.push(bytes);
        if self.backend.is_unrecordable() {
            self.give_up("the server stream could not be framed");
            return;
        }
        for frame in frames {
            self.on_backend_frame(&frame);
        }
    }

    fn on_frontend_frame(&mut self, frame: &wire::Frame) {
        match wire::parse_frontend(frame) {
            FrontendMessage::Parse { name, sql, .. } => {
                self.statement_sql.insert(name, sql.clone());
                let pending = self.building.get_or_insert_with(Pending::extended);
                if pending.sql.is_empty() {
                    pending.sql = sql;
                }
                pending.has_parse = true;
            }
            FrontendMessage::Bind {
                statement, params, ..
            } => {
                let named = self.statement_sql.get(&statement).cloned();
                let pending = self.building.get_or_insert_with(Pending::extended);
                if pending.sql.is_empty()
                    && let Some(sql) = named
                {
                    pending.sql = sql;
                }
                pending.has_bind = true;
                pending.binds = params
                    .into_iter()
                    .map(|param| param.map_or(BindValue::Null, BindValue::Value))
                    .collect();
            }
            FrontendMessage::Describe { .. } | FrontendMessage::Execute => {
                let _ = self.building.get_or_insert_with(Pending::extended);
            }
            // Dropping a cached prepared statement. It is the client's own
            // bookkeeping, never part of what the request did, but its
            // `CloseComplete` + `ReadyForQuery` still cross the wire — so it
            // gets a slot in the queue that is discarded when it completes,
            // rather than being ignored here and letting its `ReadyForQuery`
            // cut short whatever exchange is queued behind it.
            FrontendMessage::Close => {
                self.building
                    .get_or_insert_with(Pending::extended)
                    .housekeeping = true;
            }
            // `Sync` closes an extended-protocol round trip; the backend
            // answers it with exactly one `ReadyForQuery`.
            FrontendMessage::Sync => self.close_unit(),
            FrontendMessage::Query(sql) => {
                // A simple `Query` is a complete round trip on its own.
                self.close_unit();
                let marker = wire::marker_request_id(&sql);
                let mut pending = Pending::simple(sql);
                if let Some(marker) = marker {
                    pending.housekeeping = true;
                    self.apply_marker(marker);
                }
                self.push_unit(pending);
            }
            _ => {}
        }
    }

    fn on_backend_frame(&mut self, frame: &wire::Frame) {
        let from_queue = !self.in_flight.is_empty();
        let Some(target) = (if from_queue {
            self.in_flight.front_mut()
        } else {
            self.building.as_mut()
        }) else {
            // Handshake frames, and asynchronous notices between round trips:
            // nothing is in flight, so there is nothing to attach them to.
            return;
        };

        target.response.extend_from_slice(&frame.bytes);
        if frame.tag == wire::TAG_DATA_ROW {
            target.row_count = target.row_count.saturating_add(1);
        }
        if frame.tag == wire::TAG_ERROR_RESPONSE
            && let Some((code, message)) = wire::error_response_fields(frame)
        {
            target.error = Some(format!("{code}: {message}"));
        }

        if wire::terminates_exchange(frame) {
            let finished = if from_queue {
                self.in_flight.pop_front()
            } else {
                self.building.take()
            };
            if let Some(finished) = finished {
                self.finish(finished);
            }
        }
    }

    /// Push whatever extended-protocol exchange is being assembled.
    fn close_unit(&mut self) {
        if let Some(pending) = self.building.take() {
            self.push_unit(pending);
        }
    }

    fn push_unit(&mut self, pending: Pending) {
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            self.give_up("too many exchanges in flight on one connection");
            return;
        }
        self.in_flight.push_back(pending);
    }

    /// File a completed exchange into the memo and, when the connection is
    /// attributed, into the capsule's tape.
    fn finish(&mut self, pending: Pending) {
        // Autumn's own attribution statement, or a prepared-statement close.
        // Recording either would put a connection-management side effect into
        // a capsule that replay then re-executes; neither is part of what the
        // request did.
        if pending.housekeeping {
            return;
        }
        let bucket = if pending.has_parse && !pending.has_bind {
            Bucket::Statements
        } else if wire::is_catalog_sql(&pending.sql) {
            Bucket::Catalog
        } else if self.before_first_marker {
            Bucket::Prologue
        } else {
            Bucket::Request
        };
        // `exchanges` is an ordered script the replay stub walks with a
        // cursor, so it may hold only statements the request itself will
        // re-issue. Session housekeeping — `SET TIME ZONE`, `SET
        // CLIENT_ENCODING`, `SET statement_timeout` — is answered
        // synthetically at replay, and a recorded copy would leave the cursor
        // one exchange ahead of the client for the rest of the tape. The
        // prologue keeps it: that bucket is a keyed lookup, never consumed.
        if bucket == Bucket::Request && is_session_housekeeping(&pending.sql) {
            return;
        }
        // The keyed buckets are looked up by SQL, so an exchange with no SQL
        // to key on is unusable there — and an empty-SQL prologue entry means
        // something else entirely to replay (a verbatim startup handshake).
        if bucket != Bucket::Request && pending.sql.is_empty() {
            return;
        }
        let exchange = pending.into_exchange();
        self.memo.remember(bucket, &exchange);
        self.append(bucket, exchange);
    }

    /// Append an exchange to the bound scope's tape, charging the budget.
    fn append(&mut self, bucket: Bucket, exchange: Exchange) {
        if self.stopped {
            return;
        }
        let Some(scope) = self.scope() else {
            return;
        };
        let budget = scope.settings().max_capsule_bytes;
        let id = self.id;
        let cost = exchange_bytes(&exchange);

        let recorded = scope
            .with_db(|db| {
                let tape = db.tape_mut(id);
                let entries = tape_bucket(tape, bucket);
                if entries
                    .iter()
                    .any(|entry| entry.sql == exchange.sql && entry.binds == exchange.binds)
                    && bucket != Bucket::Request
                {
                    // Already carried over from the memo; the connection's
                    // history holds one entry per statement, not one per use.
                    return true;
                }
                if !db.charge(cost, budget) {
                    return false;
                }
                tape_bucket(db.tape_mut(id), bucket).push(exchange);
                true
            })
            .unwrap_or(false);

        if !recorded {
            scope.mark_truncated();
            self.stopped = true;
        }
    }

    /// Bind, re-bind or unbind the connection from the marker the client sent.
    fn apply_marker(&mut self, marker: MarkerId) {
        self.before_first_marker = false;
        match marker {
            MarkerId::Set(id) => match scope_by_id(&id) {
                Some(scope) => {
                    self.bound = Some(Arc::downgrade(&scope));
                    self.stopped = false;
                    self.copy_memo();
                }
                // The named request has already finished (its scope was
                // deregistered): nothing to attribute this work to.
                None => self.bound = None,
            },
            MarkerId::Clear => self.bound = None,
            MarkerId::Invalid => {
                tracing::warn!(
                    connection = self.id,
                    "a capsule attribution marker carried an unusable request id; queries on \
                     this connection will not be recorded"
                );
                let previous = self.scope();
                self.bound = None;
                if let Some(scope) = previous {
                    scope.note(
                        "db capture: a connection marker carried an unusable capsule id, so \
                         later queries on that connection are unattributed",
                    );
                }
            }
        }
    }

    /// Copy the connection's history into the freshly opened capsule window.
    fn copy_memo(&mut self) {
        let Some(scope) = self.scope() else {
            return;
        };
        let budget = scope.settings().max_capsule_bytes;
        let id = self.id;
        let prologue = self.memo.prologue.clone();
        let statements = self.memo.statements.clone();
        let catalog = self.memo.catalog.clone();

        let copied = scope
            .with_db(|db| {
                let tape = db.tape_mut(id);
                let want_prologue = tape.prologue.is_empty() && !prologue.is_empty();
                let want_statements = tape.statements.is_empty() && !statements.is_empty();
                let want_catalog = tape.catalog.is_empty() && !catalog.is_empty();
                let mut cost = 0usize;
                if want_prologue {
                    cost = cost.saturating_add(total_bytes(&prologue));
                }
                if want_statements {
                    cost = cost.saturating_add(total_bytes(&statements));
                }
                if want_catalog {
                    cost = cost.saturating_add(total_bytes(&catalog));
                }
                if cost == 0 {
                    return true;
                }
                if !db.charge(cost, budget) {
                    return false;
                }
                let tape = db.tape_mut(id);
                if want_prologue {
                    tape.prologue = prologue;
                }
                if want_statements {
                    tape.statements = statements;
                }
                if want_catalog {
                    tape.catalog = catalog;
                }
                true
            })
            .unwrap_or(false);

        if !copied {
            scope.mark_truncated();
            self.stopped = true;
        }
    }

    /// Stop recording this connection for good, discarding what it produced.
    ///
    /// A partial tape is worse than none: replay would answer the request's
    /// queries with the wrong bytes. The capsule is marked truncated so replay
    /// refuses it outright.
    fn give_up(&mut self, reason: &str) {
        self.disabled = true;
        self.building = None;
        self.in_flight.clear();
        self.memo = ConnectionMemo::default();
        tracing::debug!(
            connection = self.id,
            reason,
            "failure-capsule database recording stopped for this connection"
        );
        if let Some(scope) = self.scope() {
            self.bound = None;
            let id = self.id;
            scope.with_db(|db| {
                *db.tape_mut(id) = ConnectionTape {
                    id,
                    ..ConnectionTape::default()
                };
            });
            scope.note(format!("db capture stopped: {reason}"));
            scope.mark_truncated();
        }
    }
}

/// Whether every statement in `sql` is session housekeeping that the replay
/// stub answers synthetically rather than from the tape.
///
/// A batch is housekeeping only if *all* of its statements are: `SET
/// statement_timeout = 5000; SELECT 1` is the request's work and must be
/// recorded whole. Splitting on `;` is naive, but only in the safe direction —
/// a statement with a quoted semicolon simply fails to match and is recorded.
fn is_session_housekeeping(sql: &str) -> bool {
    let mut saw_statement = false;
    for statement in sql.split(';') {
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
fn is_housekeeping_statement(statement: &str) -> bool {
    let statement = statement.to_ascii_uppercase();
    let Some(setting) = statement.strip_prefix("SET ") else {
        return false;
    };
    let setting = setting.trim_start();
    [
        "TIME ZONE",
        "CLIENT_ENCODING",
        "STATEMENT_TIMEOUT",
        "AUTUMN.CAPSULE_REQUEST",
    ]
    .iter()
    .any(|name| setting.starts_with(name))
}

/// The tape vector a bucket writes into.
const fn tape_bucket(tape: &mut ConnectionTape, bucket: Bucket) -> &mut Vec<Exchange> {
    match bucket {
        Bucket::Prologue => &mut tape.prologue,
        Bucket::Statements => &mut tape.statements,
        Bucket::Catalog => &mut tape.catalog,
        Bucket::Request => &mut tape.exchanges,
    }
}

/// What one exchange costs against a size budget.
fn exchange_bytes(exchange: &Exchange) -> usize {
    let binds: usize = exchange
        .binds
        .iter()
        .map(|bind| match bind {
            BindValue::Value(bytes) => bytes.len(),
            BindValue::Null | BindValue::Masked => 0,
        })
        .sum();
    exchange
        .sql
        .len()
        .saturating_add(exchange.response.len())
        .saturating_add(binds)
        .saturating_add(EXCHANGE_OVERHEAD_BYTES)
}

fn total_bytes(exchanges: &[Exchange]) -> usize {
    exchanges
        .iter()
        .map(exchange_bytes)
        .fold(0usize, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    fn tagged(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = i32::try_from(payload.len() + 4).expect("small payload");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn query(sql: &str) -> Vec<u8> {
        let mut payload = sql.as_bytes().to_vec();
        payload.push(0);
        tagged(b'Q', &payload)
    }

    fn startup() -> Vec<u8> {
        let mut payload = 196_608u32.to_be_bytes().to_vec();
        payload.extend_from_slice(b"user\0postgres\0\0");
        let mut out = i32::try_from(payload.len() + 4)
            .expect("small payload")
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(&payload);
        out
    }

    /// F5: a vectored write must not slip past the tee. The shim reports
    /// `is_write_vectored() == false` so nothing takes that path by accident,
    /// *and* implements it, so a caller that asks for it explicitly is still
    /// recorded.
    #[tokio::test]
    async fn vectored_writes_are_refused_and_still_teed() {
        let (client, _server) = tokio::io::duplex(4096);
        let mut stream = RecordingStream::new(client);
        assert!(
            !AsyncWrite::is_write_vectored(&stream),
            "the tee must advertise itself as non-vectored so writers hand it one \
             contiguous buffer"
        );

        let startup = startup();
        let first = query("SELECT 1");
        let second = query("SELECT 2");
        let written = stream
            .write_vectored(&[
                IoSlice::new(&startup),
                IoSlice::new(&first),
                IoSlice::new(&second),
            ])
            .await
            .expect("duplex accepts the write");
        assert_eq!(written, startup.len() + first.len() + second.len());

        let seen: Vec<&str> = stream
            .recorder
            .in_flight
            .iter()
            .map(|pending| pending.sql.as_str())
            .collect();
        assert_eq!(
            seen,
            vec!["SELECT 1", "SELECT 2"],
            "every slice of a vectored write must reach the recorder"
        );
    }

    /// Plain writes are teed and split into round trips at `ReadyForQuery`.
    #[tokio::test]
    async fn simple_query_round_trip_becomes_one_exchange() {
        let (client, _server) = tokio::io::duplex(4096);
        let mut stream = RecordingStream::new(client);
        stream.write_all(&startup()).await.expect("write startup");
        stream
            .write_all(&query("SELECT 1"))
            .await
            .expect("write query");

        // Backend: CommandComplete then ReadyForQuery('I').
        let mut response = tagged(b'C', b"SELECT 1\0");
        response.extend_from_slice(&tagged(b'Z', b"I"));
        stream.recorder.on_backend(&response);

        assert!(
            stream.recorder.in_flight.is_empty(),
            "ReadyForQuery closes the exchange"
        );
        assert_eq!(
            stream.recorder.memo.prologue.len(),
            1,
            "traffic before the first marker is the connection's prologue"
        );
    }

    /// The marker binds and unbinds, and is never itself recorded.
    #[tokio::test]
    async fn marker_statements_are_not_recorded_as_exchanges() {
        let (client, _server) = tokio::io::duplex(4096);
        let mut stream = RecordingStream::new(client);
        stream.write_all(&startup()).await.expect("write startup");
        stream
            .write_all(&query(
                "SET statement_timeout = 0; SET autumn.capsule_request = 'nobody'",
            ))
            .await
            .expect("write marker");
        stream.recorder.on_backend(&tagged(b'Z', b"I"));

        assert!(
            stream.recorder.memo.prologue.is_empty(),
            "the housekeeping marker must not be remembered as connection history"
        );
        assert!(
            !stream.recorder.before_first_marker,
            "the marker ends the prologue even when no scope claims it"
        );
        assert!(
            stream.recorder.bound.is_none(),
            "an unknown capsule id must leave the connection unattributed"
        );
    }

    #[test]
    fn catalog_probes_and_prepared_statements_land_in_their_own_buckets() {
        let mut memo = ConnectionMemo::default();
        let statement = Exchange {
            protocol: ExchangeProtocol::Extended,
            sql: "SELECT $1::text".to_owned(),
            binds: Vec::new(),
            response: vec![b'1', 0, 0, 0, 4],
            row_count: 0,
            error: None,
        };
        memo.remember(Bucket::Statements, &statement);
        memo.remember(Bucket::Statements, &statement);
        assert_eq!(
            memo.statements.len(),
            1,
            "the same statement prepared twice is remembered once"
        );
        memo.remember(Bucket::Request, &statement);
        assert!(
            memo.prologue.is_empty() && memo.catalog.is_empty(),
            "a request's own exchange is never connection history"
        );
    }

    /// `exchanges` is a cursor-walked script at replay, and the stub answers
    /// session settings synthetically — so a recorded copy of one would leave
    /// the cursor a step ahead of the client for the rest of the tape.
    #[test]
    fn session_settings_are_housekeeping_but_real_work_is_not() {
        assert!(is_session_housekeeping("SET TIME ZONE 'UTC'"));
        assert!(is_session_housekeeping("set client_encoding TO 'UTF8'"));
        assert!(is_session_housekeeping(
            "SET statement_timeout = 5000; SET autumn.capsule_request = 'abc'"
        ));
        assert!(is_session_housekeeping("  SET TIME ZONE 'UTC';  "));

        assert!(
            !is_session_housekeeping("SET statement_timeout = 5000; SELECT 1"),
            "a batch that also does real work is the request's, not housekeeping"
        );
        assert!(!is_session_housekeeping("SELECT $1::text"));
        assert!(!is_session_housekeeping("SET LOCAL search_path TO app"));
        assert!(!is_session_housekeeping(""));
    }

    #[test]
    fn tls_and_socket_urls_are_not_recordable() {
        assert!(
            capture_unavailable_reason("postgres://u:p@host:5432/db").is_none(),
            "a plaintext TCP URL is recordable"
        );
        assert!(
            capture_unavailable_reason("postgres://u:p@host:5432/db?sslmode=require")
                .is_some_and(|reason| reason.contains("TLS")),
            "a TLS URL must be refused with a reason naming TLS"
        );
        assert!(
            capture_unavailable_reason("postgres:///db?host=/var/run/postgresql").is_some(),
            "a Unix-socket URL has no stream to tee"
        );
    }
}
