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
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use deadpool::managed::{Hook, HookError};
use diesel::{ConnectionError, ConnectionResult};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::pooled_connection::{
    AsyncDieselConnectionManager, ManagerConfig, RecyclingMethod,
};
use futures::FutureExt as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::capsule::capture::{CaptureScope, scope_by_id};
use crate::capsule::schema::{BindValue, ConnectionTape, Exchange, ExchangeProtocol};
use crate::capsule::wire::{
    self, FrameSplitter, FrontendMessage, MarkerId, is_session_housekeeping,
};
use crate::config::{AutumnConfig, DatabaseConfig};
use crate::db::{DatabaseTopology, PoolError};

/// Most remembered entries of one memo bucket. A connection that prepares
/// thousands of distinct statements gets a partial memo rather than unbounded
/// growth; replay reports the resulting gap as a divergence.
const MAX_MEMO_ENTRIES: usize = 256;

/// Byte ceiling on one connection's memo, independent of the per-capsule
/// budget (the memo lives as long as the connection does).
const MAX_MEMO_BYTES: usize = 1024 * 1024;

/// Fraction of a capsule's byte budget the connection memo may claim.
///
/// The memo is *history* — traffic that happened before the captured request —
/// so it must never crowd out the request's own exchanges. Without this share
/// a warm connection whose memo has grown to [`MAX_MEMO_BYTES`] (the same size
/// as the default `max_capsule_bytes`) would exhaust the whole budget on the
/// first copy, and every capsule that connection produced would be refused as
/// truncated before recording a single statement of the request.
const MEMO_BUDGET_SHARE: usize = 4;

/// Most live prepared-statement names one connection's recorder tracks.
///
/// `Close` removes an entry, so this is a ceiling on *live* statements rather
/// than on statements ever prepared. A connection that blows through it is
/// given up on: a `Bind` whose statement name is unknown would be recorded with
/// no SQL, which is worse than an honest refusal.
const MAX_STATEMENT_NAMES: usize = 1024;

/// The statement that unbinds a connection from whatever capsule it was last
/// attributed to.
///
/// Kept as a literal because the pool hooks below run on the checkout hot path;
/// [`clearing_marker_is_the_wire_marker`](tests::clearing_marker_is_the_wire_marker)
/// pins it to [`wire::marker_set_sql`]'s clearing form.
const CLEAR_MARKER_SQL: &str = "SET autumn.capsule_request = ''";

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
    if tcp_endpoints(&config).is_empty() {
        return Some(
            "the database URL names no TCP host (a Unix-socket connection cannot be teed)"
                .to_owned(),
        );
    }
    None
}

/// Warn — at the boot that decided it — that this app's DB capture is off.
///
/// The reason itself travels on the app's
/// [`DatabaseTopology`](crate::db::DatabaseTopology) (see
/// [`DatabaseTopology::capture_gap`](crate::db::DatabaseTopology::capture_gap)),
/// not in process state: two apps in one process can disagree about whether
/// their databases record, and one app's gap must never truncate the other's
/// capsules.
fn warn_db_capture_unavailable(reason: &str) {
    tracing::warn!(
        reason,
        "failure-capsule database capture is disabled; capsules will record the request, \
         clock and outcome but no database traffic"
    );
}

/// Note `reason` onto a capture scope, so the capsule explains itself — and
/// mark the capsule truncated, because it is missing effects the request had.
///
/// Called from the connection-checkout path, which is the one place that knows
/// a request is about to use a database at all; the reason comes from the
/// app's own pool topology, so it is per-app truth. The truncation mirrors
/// [`note_shard_capture_gap`]: a capsule whose request talked to a database
/// none of whose traffic was recorded holds strictly less than the request did,
/// so [`refusal_reason`](crate::capsule::refusal_reason) must refuse it rather
/// than let a replay report divergences that only reflect the missing tape.
pub fn note_db_capture_unavailable(scope: &CaptureScope, reason: &str) {
    scope.note(format!("db capture unavailable: {reason}"));
    scope.mark_truncated();
}

/// What a capsule says when its request used a shard connection.
pub const SHARD_CAPTURE_NOTE: &str = "shard database traffic is not captured in this slice: this request checked out a \
     `[[database.shards]]` connection, and its queries are absent from the tape";

/// Record that the in-flight request reached for a shard connection.
///
/// Only the control topology's pools are built through the recording factory
/// (see [`maybe_capture_pool_provider`]); shard pools come from
/// [`create_shard_set`](crate::sharding::create_shard_set) and tee nothing. A
/// capsule for a request that used one therefore holds *less* than the request
/// did, so it is noted **and marked truncated**: a capsule that is missing
/// effects must not be presented as replayable, and
/// [`refusal_reason`](crate::capsule::refusal_reason) turns truncation into a
/// refusal with a reason instead of a run that looks faithful.
///
/// Called from the connection-checkout path, which is the one place that knows
/// a *particular request* touched a shard — a coarser "shards are configured"
/// test would truncate capsules for requests that never went near one. That
/// path runs on every shard checkout; a request outside any capture scope
/// pays one task-local lookup and moves on.
pub fn note_shard_capture_gap() {
    if let Some(scope) = crate::capsule::current_scope() {
        scope.note(SHARD_CAPTURE_NOTE);
        scope.mark_truncated();
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
/// # Attribution at the pool boundary
///
/// [`Db::checkout`](crate::db::Db::checkout) sends the attribution marker, but
/// it is not the only way a connection leaves this pool: the job runtime, the
/// mailer, job tracking and the app's own `state.pool().get()` all take
/// connections straight from it. Without help the recorder would keep the
/// *previous* borrower's binding and file their statements into a stranger's
/// capsule — or, on a connection they were the first to touch, write their SQL
/// into the connection prologue that [`ConnectionRecorder::copy_memo`] then
/// copies into every later capsule.
///
/// Both are closed here, at the boundary every borrower crosses: the
/// `post_create` and `pre_recycle` hooks run on the checking-out task and set
/// the binding from that task's own truth — the current capture scope's id
/// when one is present (a direct `state.pool().get()` on a request task: a
/// policy, a notification store, an app helper — their SQL belongs in the
/// request's tape), and the clearing form otherwise, so unscoped background
/// work can never be attributed to whoever held the connection last.
/// `deadpool` applies `pre_recycle` before the manager's recycle check and
/// `post_create` before the object is handed out, so no user statement can
/// precede the binding. The marker statement is housekeeping and is never
/// itself recorded. The cost is one extra round trip per checkout on a
/// recording pool, which is capture's price for correct attribution — ordinary
/// pools are untouched.
///
/// Recycling is [`RecyclingMethod::Fast`] rather than deadpool's default
/// `Verified`: the `SELECT $1` ping the verified method issues would be teed
/// like any other statement, and — being sent before the next borrower's marker
/// rebinds the connection — recorded into the *previous* request's exchanges. A
/// replay of that capsule never issues it, so the tape would mismatch on the
/// second checkout of a one-slot pool. The pre-recycle unbind above makes that
/// harmless, and dropping the ping removes the round trip as well.
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
    role: &'static str,
) -> Result<Pool<AsyncPgConnection>, PoolError> {
    if let Some(reason) = capture_unavailable_reason(url) {
        return Err(PoolError::UnsupportedBackend(format!(
            "a failure-capsule recording pool cannot be built for this database URL: {reason}"
        )));
    }
    let mut manager_config = ManagerConfig::<AsyncPgConnection>::default();
    manager_config.recycling_method = RecyclingMethod::Fast;
    manager_config.custom_setup = Box::new(move |url: &str| {
        let url = url.to_owned();
        async move { establish_recording(&url, role).await }.boxed()
    });
    let manager =
        AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(url, manager_config);
    Ok(Pool::builder(manager)
        .max_size(max_size.max(1))
        .wait_timeout(Some(connect_timeout))
        .create_timeout(Some(connect_timeout))
        .post_create(attribution_hook())
        .pre_recycle(attribution_hook())
        .runtime(deadpool::Runtime::Tokio1)
        .build()?)
}

/// A pool hook that binds the connection to the checking-out task's capture
/// scope — or unbinds it when there is none (see [`build_recording_pool`]).
///
/// Deadpool runs both hooks on the task that called `pool.get()`, so the
/// capture scope task-local is visible here. That closes the *direct
/// checkout* gap: a handler (or a policy, or a notification store) that takes
/// a connection through the public `state.pool().get()` API never passes
/// through `Db::checkout`'s marker, and without this binding its SQL would
/// silently vanish from the capsule — leaving a "complete" tape that
/// false-diverges the moment an unchanged replay issues the missing query. A
/// scope-free checkout — the job poller, the mailer, a detached task — still
/// clears the binding, so unattributed work can never land in whoever held
/// the connection last (F2). `Db::checkout`'s own merged marker remains and
/// simply re-asserts the same binding.
///
/// A failure is reported to `deadpool`, which drops the connection and takes
/// the next one: a connection whose binding cannot be set must not be handed
/// out, because everything it goes on to do would be filed under the wrong
/// capsule.
fn attribution_hook() -> Hook<AsyncDieselConnectionManager<AsyncPgConnection>> {
    Hook::async_fn(|conn: &mut AsyncPgConnection, _metrics| {
        Box::pin(async move {
            use diesel_async::SimpleAsyncConnection as _;

            let marker = crate::capsule::current_scope()
                .map(|scope| scope.id().to_owned())
                .filter(|id| crate::capsule::is_valid_scope_id(id))
                .and_then(|id| wire::marker_set_sql(&id))
                .unwrap_or_else(|| CLEAR_MARKER_SQL.to_owned());
            conn.batch_execute(&marker).await.map_err(|error| {
                tracing::debug!(
                    %error,
                    "a recorded connection's capsule binding could not be set; it will be \
                     discarded rather than handed out misattributed"
                );
                HookError::message(format!(
                    "the failure-capsule recording pool could not set a connection's capsule \
                     binding: {error}"
                ))
            })
        })
    })
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
async fn establish_recording(url: &str, role: &'static str) -> ConnectionResult<AsyncPgConnection> {
    let config = url
        .parse::<tokio_postgres::Config>()
        .map_err(|error| ConnectionError::InvalidConnectionUrl(error.to_string()))?;
    let endpoints = tcp_endpoints(&config);
    if endpoints.is_empty() {
        return Err(ConnectionError::InvalidConnectionUrl(
            "the recording pool needs a TCP host in the database URL".to_owned(),
        ));
    }

    // A multi-host URL is a failover list, and tokio-postgres's own connector
    // walks it in order — so this one must too, or enabling capture would
    // turn a healthy HA deployment away whenever its first host is down. Each
    // endpoint gets the *complete* attempt (TCP, startup and authentication,
    // the writability probe): a host that accepts TCP but cannot finish the
    // handshake — a half-up standby, a mid-failover primary — rejects that
    // endpoint, not the pool.
    let mut failures: Vec<String> = Vec::new();
    for (host, port) in &endpoints {
        match establish_recording_at(&config, host, *port, role).await {
            Ok(connection) => return Ok(connection),
            Err(reason) => failures.push(format!("{host}:{port}: {reason}")),
        }
    }
    Err(ConnectionError::BadConnection(format!(
        "failed to establish a recorded connection to every configured host ({})",
        failures.join("; ")
    )))
}

/// One complete recorded-connection attempt against one endpoint.
///
/// Everything that can disqualify a host happens here, so the caller's
/// failover loop only accepts an endpoint the ordinary connector would have
/// accepted: the TCP connect, the `PostgreSQL` startup and authentication
/// over the tee, the `target_session_attrs=read-write` probe when the URL
/// demands a writable session, and diesel-async's adoption of the client.
/// The probe is the same `SHOW transaction_read_only` tokio-postgres's own
/// multi-host connector issues; it runs before the first checkout marker, so
/// it lands in the connection memo's prologue — re-askable metadata the
/// replay stub answers by key, never a tape entry a replay must consume.
async fn establish_recording_at(
    config: &tokio_postgres::Config,
    host: &str,
    port: u16,
    role: &'static str,
) -> Result<AsyncPgConnection, String> {
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|error| error.to_string())?;
    // Match what tokio-postgres's own connector does: PostgreSQL is a
    // request/response protocol, so Nagle only adds latency.
    let _ = stream.set_nodelay(true);

    let (client, connection) = config
        .connect_raw(
            RecordingStream::with_role(stream, role),
            tokio_postgres::NoTls,
        )
        .await
        .map_err(|error| error.to_string())?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "recorded database connection ended");
        }
    });

    if matches!(
        config.get_target_session_attrs(),
        tokio_postgres::config::TargetSessionAttrs::ReadWrite
    ) {
        let rows = client
            .simple_query("SHOW transaction_read_only")
            .await
            .map_err(|error| error.to_string())?;
        let read_only = rows.iter().any(|message| {
            matches!(
                message,
                tokio_postgres::SimpleQueryMessage::Row(row) if row.get(0) == Some("on")
            )
        });
        if read_only {
            return Err(
                "the session is read-only, and the URL asks for target_session_attrs=read-write"
                    .to_owned(),
            );
        }
    }

    AsyncPgConnection::try_from(client)
        .await
        .map_err(|error| error.to_string())
}

/// Every TCP host/port pair in a parsed connection string, in configured
/// order.
///
/// Ports pair with hosts the way tokio-postgres pairs them: index-matched when
/// one port was given per host, the single (or first) port for every host
/// otherwise.
fn tcp_endpoints(config: &tokio_postgres::Config) -> Vec<(String, u16)> {
    let ports = config.get_ports();
    config
        .get_hosts()
        .iter()
        .enumerate()
        .filter_map(|(index, host)| match host {
            tokio_postgres::config::Host::Tcp(name) => Some((
                name.clone(),
                ports
                    .get(index)
                    .or_else(|| ports.first())
                    .copied()
                    .unwrap_or(DEFAULT_PG_PORT),
            )),
            // A Unix-socket host: nothing to tee, and replay has no socket to
            // stand in for it. The variant itself is `cfg(unix)` in
            // tokio-postgres, so the arm has to be too — on Windows `Host` has
            // only `Tcp` and this match is already exhaustive without it.
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(_) => None,
        })
        .collect()
}

/// Decide whether `App::run` should build its database pools through the
/// recording factory.
///
/// Returns the provider slot to use. An application that installed its own
/// [`DatabasePoolProvider`](crate::db::DatabasePoolProvider) is left completely
/// alone — Autumn will not second-guess a custom pool — at the cost of DB
/// capture, which is logged and noted on every capsule (F25).
///
/// The slot this fills is the **control topology's** only. `[[database.shards]]`
/// pools are built separately by
/// [`create_shard_set`](crate::sharding::create_shard_set) and are not recorded
/// in this slice; a boot with both capture and shards configured says so, and
/// any request that actually checks a shard connection out has its capsule
/// noted and truncated by [`note_shard_capture_gap`].
#[must_use]
pub fn maybe_capture_pool_provider(
    existing: Option<CapturePoolProvider>,
    config: &AutumnConfig,
) -> Option<CapturePoolProvider> {
    if !config.failure_capture.enabled {
        return existing;
    }
    if !config.database.shards.is_empty() {
        tracing::warn!(
            shard_count = config.database.shards.len(),
            "failure-capsule capture does not record `[[database.shards]]` traffic; a request \
             that checks out a shard connection will have its capsule marked truncated, and \
             `autumn replay` will refuse it"
        );
    }
    if let Some(inner) = existing {
        // The custom provider is left alone, but the topology it returns is
        // stamped with the gap so *this app's* capsules — and only this
        // app's — say why they carry no database tape.
        const REASON: &str =
            "the application installed a custom DatabasePoolProvider, which Autumn does not wrap";
        warn_db_capture_unavailable(REASON);
        return Some(Box::new(move |database: DatabaseConfig| {
            Box::pin(async move {
                inner(database)
                    .await
                    .map(|topology| topology.map(|t| t.with_capture_gap(Some(REASON.to_owned()))))
            })
        }));
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
        warn_db_capture_unavailable(&reason);
        return crate::db::create_topology(config)
            .map(|topology| topology.map(|t| t.with_capture_gap(Some(reason))));
    }

    let timeout = Duration::from_secs(config.connect_timeout_secs);
    let primary = build_recording_pool(
        primary_url,
        config.effective_primary_pool_size(),
        timeout,
        crate::capsule::schema::TAPE_ROLE_PRIMARY,
    )?;
    let replica = config
        .replica_url
        .as_deref()
        .map(|url| {
            build_recording_pool(
                url,
                config.effective_replica_pool_size(),
                timeout,
                crate::capsule::schema::TAPE_ROLE_REPLICA,
            )
        })
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
    /// Wrap `inner`, assigning the connection a fresh recorder on the
    /// primary role. Production connectors name their role explicitly via
    /// [`Self::with_role`]; this shorthand serves the wire-level tests.
    #[cfg(test)]
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self::with_role(inner, crate::capsule::schema::TAPE_ROLE_PRIMARY)
    }

    /// Wrap `inner`, tagging every tape this connection records with `role`
    /// (`"primary"` or `"replica"`), so replay can rebuild one stub pool per
    /// role and hand each tape back on the pool it was recorded on.
    #[must_use]
    pub fn with_role(inner: S, role: &'static str) -> Self {
        Self {
            inner,
            recorder: ConnectionRecorder::new(
                NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
                role,
            ),
        }
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

/// What a capsule says when the connection it borrowed had already stopped
/// recording.
const POISONED_CONNECTION_NOTE: &str = "db capture stopped earlier on the connection this request borrowed, so its queries are \
     absent from the tape";

/// What a capsule says when the connection memo did not fit its budget share.
const MEMO_SHARE_NOTE: &str = "db capture: the connection's remembered history did not fit this capsule's budget share, so \
     some of its prepared-statement metadata was left out; replay may report an unknown statement";

/// The result of copying a connection memo into a capsule window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoCopy {
    /// Everything the capsule needed was copied.
    Copied,
    /// Some buckets were left out to stay inside the memo's budget share.
    Partial,
    /// The capsule budget itself is exhausted; the capsule is truncated.
    OverBudget,
}

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
#[allow(
    clippy::struct_excessive_bools,
    reason = "the flags (first-marker window, disabled, poisoned) are independent \
              lifecycle facts about one connection, not an encodable state machine"
)]
pub struct ConnectionRecorder {
    id: u64,
    /// The pool role this connection belongs to, stamped onto every tape it
    /// records so replay claims it from the matching stub pool.
    role: &'static str,
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
    /// Set when the stream stopped making sense: nothing on this connection is
    /// recorded again. Markers are still watched (see
    /// [`Self::watch_marker_while_poisoned`]) so the capsules of *later*
    /// requests that borrow this connection are told their tape is missing,
    /// instead of being written out as complete.
    poisoned: bool,
    /// Set when the *client* half stopped framing. Marker watching needs frames,
    /// so this is the one failure the recorder cannot warn later requests
    /// about; it is also the one a well-behaved driver cannot cause.
    frontend_lost: bool,
}

impl ConnectionRecorder {
    fn new(id: u64, role: &'static str) -> Self {
        Self {
            id,
            role,
            frontend: FrameSplitter::new_frontend(),
            backend: FrameSplitter::new_backend(),
            statement_sql: HashMap::new(),
            building: None,
            in_flight: VecDeque::new(),
            memo: ConnectionMemo::default(),
            bound: None,
            before_first_marker: true,
            stopped: false,
            poisoned: false,
            frontend_lost: false,
        }
    }

    /// Observe bytes the client wrote.
    /// The scope this connection is attributed to, if it is still alive.
    /// A closed scope is deliberately treated as absent: the request is over,
    /// its capsule is being written, and the next thing this connection does
    /// (the pool's liveness ping, before the next request's marker arrives) is
    /// not part of what that request did.
    fn scope(&self) -> Option<Arc<CaptureScope>> {
        self.bound
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|scope| !scope.is_closed())
    }

    fn on_frontend(&mut self, bytes: &[u8]) {
        if self.frontend_lost {
            return;
        }
        let frames = self.frontend.push(bytes);
        if self.frontend.is_unrecordable() {
            self.frontend_lost = true;
            self.give_up("the client stream could not be framed");
            return;
        }
        for frame in frames {
            if self.poisoned {
                // Recording is over for this connection, but attribution
                // markers still matter: the next request to borrow it must be
                // told its capsule has no tape (F13's honesty rule), not handed
                // a capsule that looks complete because nothing was recorded.
                Self::watch_marker_while_poisoned(&frame);
            } else {
                self.on_frontend_frame(&frame);
            }
        }
    }

    /// Observe bytes the server wrote.
    fn on_backend(&mut self, bytes: &[u8]) {
        if self.poisoned {
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

    /// While poisoned, the only thing worth reading off the client half is the
    /// attribution marker: a request that binds this connection is a request
    /// whose capsule will be missing everything it does here.
    fn watch_marker_while_poisoned(frame: &wire::Frame) {
        let FrontendMessage::Query(sql) = wire::parse_frontend(frame) else {
            return;
        };
        let Some(MarkerId::Set(id)) = wire::marker_request_id(&sql) else {
            return;
        };
        let Some(scope) = scope_by_id(&id) else {
            return;
        };
        scope.note(POISONED_CONNECTION_NOTE);
        scope.mark_truncated();
    }

    fn on_frontend_frame(&mut self, frame: &wire::Frame) {
        match wire::parse_frontend(frame) {
            FrontendMessage::Parse { name, sql, .. } => {
                if self.statement_sql.len() >= MAX_STATEMENT_NAMES
                    && !self.statement_sql.contains_key(&name)
                {
                    // Recording a `Bind` whose statement we no longer know the
                    // SQL for would put an empty-SQL exchange on the tape, and
                    // replay would answer the wrong statement from it.
                    self.give_up(
                        "this connection holds more live prepared statements than \
                                  capture tracks",
                    );
                    return;
                }
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
            //
            // Closing a *statement* also retires its name: the SQL behind it
            // is no longer reachable by any later `Bind`, so keeping the entry
            // would grow the map for the life of the connection (a long-lived
            // pooled connection prepares and drops statements indefinitely).
            FrontendMessage::Close { kind, name } => {
                if kind == b'S' {
                    self.statement_sql.remove(&name);
                }
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
        let role = self.role;
        let cost = exchange_bytes(&exchange);

        let recorded = scope
            .with_db(|db| {
                // Re-checked under the buffer's own lock: `scope()` tested this
                // before the lock was taken, and the request can finish in
                // between. Closing and appending must not interleave, or a late
                // effect lands in a capsule that is already being written.
                if scope.is_closed() {
                    return true;
                }
                let tape = db.tape_mut(id);
                if tape.role != role {
                    role.clone_into(&mut tape.role);
                }
                let entries = tape_bucket(tape, bucket);
                // The ordered `exchanges` bucket never deduplicates — a request
                // may legitimately run the same statement twice — so the scan
                // is skipped for it entirely rather than run and discarded.
                if bucket != Bucket::Request
                    && entries
                        .iter()
                        .any(|entry| entry.sql == exchange.sql && entry.binds == exchange.binds)
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
    ///
    /// The memo may only claim `max_capsule_bytes / MEMO_BUDGET_SHARE` of the
    /// capsule's budget. It is history, not the request's own work, and a warm
    /// connection's memo can reach [`MAX_MEMO_BYTES`] — the size of the whole
    /// default budget — at which point copying it wholesale would exhaust the
    /// budget before the request recorded a single statement, and *every*
    /// capsule that connection produced would be refused as truncated. Buckets
    /// past the share are skipped in ascending order of replay value
    /// (statements, then catalog, then prologue) and the capsule says so.
    fn copy_memo(&mut self) {
        let Some(scope) = self.scope() else {
            return;
        };
        let budget = scope.settings().max_capsule_bytes;
        let allowance = budget / MEMO_BUDGET_SHARE;
        let id = self.id;
        let memo = &self.memo;

        let outcome = scope
            .with_db(|db| {
                if scope.is_closed() {
                    return MemoCopy::Copied;
                }
                let tape = db.tape_mut(id);
                // Only buckets this capsule has none of are worth copying, and
                // the clone is deferred until that is known: the memo can hold
                // up to a megabyte, and cloning it to discard it was the
                // dominant cost of every marker on a warm connection.
                let want_statements = tape.statements.is_empty() && !memo.statements.is_empty();
                let want_catalog = tape.catalog.is_empty() && !memo.catalog.is_empty();
                let want_prologue = tape.prologue.is_empty() && !memo.prologue.is_empty();

                let mut spent = 0usize;
                let mut skipped = false;
                let mut fits = |wanted: bool, entries: &[Exchange]| {
                    if !wanted {
                        return false;
                    }
                    let cost = total_bytes(entries);
                    if spent.saturating_add(cost) > allowance {
                        skipped = true;
                        return false;
                    }
                    spent = spent.saturating_add(cost);
                    true
                };
                let take_statements = fits(want_statements, &memo.statements);
                let take_catalog = fits(want_catalog, &memo.catalog);
                let take_prologue = fits(want_prologue, &memo.prologue);

                if spent > 0 {
                    if !db.charge(spent, budget) {
                        return MemoCopy::OverBudget;
                    }
                    let tape = db.tape_mut(id);
                    if take_statements {
                        tape.statements.clone_from(&memo.statements);
                    }
                    if take_catalog {
                        tape.catalog.clone_from(&memo.catalog);
                    }
                    if take_prologue {
                        tape.prologue.clone_from(&memo.prologue);
                    }
                }
                if skipped {
                    MemoCopy::Partial
                } else {
                    MemoCopy::Copied
                }
            })
            .unwrap_or(MemoCopy::OverBudget);

        match outcome {
            MemoCopy::Copied => {}
            MemoCopy::Partial => scope.note(MEMO_SHARE_NOTE),
            MemoCopy::OverBudget => {
                scope.mark_truncated();
                self.stopped = true;
            }
        }
    }

    /// Stop recording this connection for good, discarding what it produced.
    ///
    /// A partial tape is worse than none: replay would answer the request's
    /// queries with the wrong bytes. The capsule is marked truncated so replay
    /// refuses it outright — and so is the capsule of every *later* request
    /// that binds this connection, which is why poisoning is tracked separately
    /// from "stop parsing" (see [`Self::watch_marker_while_poisoned`]). Giving
    /// up before any marker has arrived used to leave later requests with
    /// capsules that carried no tape and claimed to be complete.
    fn give_up(&mut self, reason: &str) {
        self.poisoned = true;
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
            let role = self.role.to_owned();
            scope.with_db(|db| {
                *db.tape_mut(id) = ConnectionTape {
                    id,
                    role,
                    ..ConnectionTape::default()
                };
            });
            scope.note(format!("db capture stopped: {reason}"));
            scope.mark_truncated();
        }
    }
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

    #[test]
    fn tcp_endpoints_lists_every_configured_host_in_order() {
        let config = "host=first.example,second.example port=5433,5434 user=app dbname=app"
            .parse::<tokio_postgres::Config>()
            .expect("multi-host config parses");
        assert_eq!(
            tcp_endpoints(&config),
            vec![
                ("first.example".to_owned(), 5433),
                ("second.example".to_owned(), 5434),
            ],
            "a failover list must be preserved in configured order, with ports \
             paired the way tokio-postgres pairs them"
        );

        let single_port = "host=first.example,second.example port=6000 user=app"
            .parse::<tokio_postgres::Config>()
            .expect("single-port config parses");
        assert_eq!(
            tcp_endpoints(&single_port),
            vec![
                ("first.example".to_owned(), 6000),
                ("second.example".to_owned(), 6000),
            ],
            "one configured port applies to every host"
        );
    }

    /// The failover loop must give every endpoint the *complete* connection
    /// attempt: a host that accepts TCP but cannot finish the `PostgreSQL`
    /// handshake (here, a listener that closes every accepted socket — a
    /// half-up standby's shape) must reject that endpoint and move on, not
    /// select the socket permanently and fail the pool.
    #[tokio::test]
    async fn a_host_that_accepts_tcp_but_fails_the_handshake_is_not_selected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let half_up = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            // Accept and immediately drop: TCP succeeds, the startup message
            // meets an EOF.
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });

        let error = establish_recording(
            &format!("host=127.0.0.1,127.0.0.1 port={half_up},1 user=postgres dbname=postgres"),
            crate::capsule::schema::TAPE_ROLE_PRIMARY,
        )
        .await
        .err()
        .expect("neither endpoint can finish a handshake");
        let message = error.to_string();
        assert!(
            message.contains(&format!("127.0.0.1:{half_up}")) && message.contains("127.0.0.1:1"),
            "the error must show the loop moved past the TCP-accepting host \
             and tried the whole failover list, got {message}"
        );
    }

    #[tokio::test]
    async fn exhausting_every_host_names_each_failed_attempt() {
        let error = establish_recording(
            "host=127.0.0.1,127.0.0.1 port=1,2 user=postgres dbname=postgres",
            crate::capsule::schema::TAPE_ROLE_PRIMARY,
        )
        .await
        .err()
        .expect("nothing listens on ports 1 or 2");
        let message = error.to_string();
        assert!(
            message.contains("127.0.0.1:1") && message.contains("127.0.0.1:2"),
            "the error must name every attempted endpoint so an operator can \
             see the whole failover list was tried, got {message}"
        );
    }

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

    /// The hooks send the same statement the checkout path does, so the
    /// recorder recognises it as its own housekeeping and never records it.
    #[test]
    fn clearing_marker_is_the_wire_marker() {
        assert_eq!(
            wire::marker_set_sql("").as_deref(),
            Some(CLEAR_MARKER_SQL),
            "the pool hooks must send exactly the clearing marker the recorder parses"
        );
        assert!(
            is_session_housekeeping(CLEAR_MARKER_SQL),
            "the unbind statement is Autumn's own bookkeeping and must never reach a tape"
        );
        assert_eq!(
            wire::marker_request_id(CLEAR_MARKER_SQL),
            Some(MarkerId::Clear)
        );
    }

    /// A connection that stopped recording must say so to the *next* request
    /// that borrows it. Otherwise that request's capsule is written with no
    /// database tape and `truncated: false` — a capsule that claims the handler
    /// never touched the database.
    #[tokio::test]
    async fn a_poisoned_connection_tells_the_next_request_its_tape_is_missing() {
        let scope = scope_for_test("later-request");
        crate::capsule::capture::register(&scope);

        let (client, _server) = tokio::io::duplex(1024 * 1024);
        let mut stream = RecordingStream::new(client);
        stream.write_all(&startup()).await.expect("write startup");
        // Overrun the in-flight queue: the backend answers nothing, so every
        // query stays pending until the recorder gives up.
        for index in 0..=MAX_IN_FLIGHT {
            stream
                .write_all(&query(&format!("SELECT {index}")))
                .await
                .expect("write query");
        }
        assert!(
            stream.recorder.poisoned,
            "an unbounded in-flight queue must stop recording"
        );

        // The next request checks the connection out and binds it.
        stream
            .write_all(&query("SET autumn.capsule_request = 'later-request'"))
            .await
            .expect("write marker");

        assert!(
            scope
                .notes()
                .iter()
                .any(|note| note == POISONED_CONNECTION_NOTE),
            "the later request's capsule must explain the missing tape, got {:?}",
            scope.notes()
        );
        assert!(
            scope.is_truncated(),
            "a capsule with no tape because recording had already stopped must be refused \
             by replay, not presented as a request that used no database"
        );
    }

    /// `Close` retires a prepared statement name, so a long-lived pooled
    /// connection's name map tracks live statements rather than every statement
    /// it ever prepared.
    #[tokio::test]
    async fn closing_a_prepared_statement_forgets_its_sql() {
        let (client, _server) = tokio::io::duplex(4096);
        let mut stream = RecordingStream::new(client);
        stream.write_all(&startup()).await.expect("write startup");

        let mut parse = b"s1\0".to_vec();
        parse.extend_from_slice(b"SELECT $1::text\0");
        parse.extend_from_slice(&0i16.to_be_bytes());
        stream
            .write_all(&tagged(b'P', &parse))
            .await
            .expect("write parse");
        assert_eq!(
            stream.recorder.statement_sql.get("s1").map(String::as_str),
            Some("SELECT $1::text")
        );

        stream
            .write_all(&tagged(b'C', b"Ss1\0"))
            .await
            .expect("write close");
        assert!(
            stream.recorder.statement_sql.is_empty(),
            "closing the statement must retire its name, got {:?}",
            stream.recorder.statement_sql
        );
    }

    /// The memo is the connection's *history*: it may claim a share of the
    /// capsule budget, never all of it. A warm connection whose memo had grown
    /// to the size of the whole budget used to exhaust it on the first marker,
    /// so every capsule that connection produced was refused as truncated
    /// before the request recorded a single statement.
    #[tokio::test]
    async fn a_warm_memo_cannot_eat_the_whole_capsule_budget() {
        let settings = Arc::new(crate::capsule::CaptureSettings {
            max_capsule_bytes: 4_000,
            ..crate::capsule::CaptureSettings::default()
        });
        let scope = Arc::new(CaptureScope::new(
            "budget".to_owned(),
            settings,
            Arc::new(crate::log::filter::ParameterFilter::new(&[], &[])),
        ));

        let mut recorder = ConnectionRecorder::new(7, crate::capsule::schema::TAPE_ROLE_PRIMARY);
        recorder.bound = Some(Arc::downgrade(&scope));
        // A memo far bigger than the whole budget.
        for index in 0..20 {
            recorder.memo.remember(
                Bucket::Statements,
                &Exchange {
                    protocol: ExchangeProtocol::Extended,
                    sql: format!("SELECT {index} FROM big"),
                    binds: Vec::new(),
                    response: vec![0u8; 500],
                    row_count: 0,
                    error: None,
                },
            );
        }
        recorder.copy_memo();

        assert!(!recorder.stopped, "the memo must not stop recording");
        assert!(
            !scope.is_truncated(),
            "a big memo must not refuse the capsule before the request has run"
        );
        let charged = scope.with_db(|db| db.charged_bytes()).expect("db lock");
        assert!(
            charged <= 1_000,
            "the memo may claim at most a quarter of the 4000-byte budget, charged {charged}"
        );
        assert!(
            scope.notes().iter().any(|note| note == MEMO_SHARE_NOTE),
            "a capsule whose memo was trimmed must say so, got {:?}",
            scope.notes()
        );

        // And the request's own work still fits.
        recorder.append(
            Bucket::Request,
            Exchange {
                protocol: ExchangeProtocol::Extended,
                sql: "SELECT 1".to_owned(),
                binds: Vec::new(),
                response: vec![0u8; 100],
                row_count: 1,
                error: None,
            },
        );
        assert!(
            !scope.is_truncated(),
            "the request's own exchange must fit the budget the memo did not take"
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
        // `SET LOCAL` is never the framework's — `Db::checkout` issues a plain
        // session-level `SET statement_timeout` — so a transaction-scoped
        // setting is application code and belongs on the ordered tape, where
        // changing or removing it shows up as a divergence (#2202).
        assert!(
            !is_session_housekeeping("SET LOCAL statement_timeout = 5000"),
            "an application's transaction-scoped timeout must not be synthesized away"
        );
        assert!(!is_session_housekeeping("set local TIME ZONE 'UTC'"));
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
        // On Windows tokio-postgres has no `Host::Unix` variant — a socket
        // path parses as an (unreachable) TCP hostname — so socket detection
        // is a unix-only behaviour, exactly like the `Host::Unix` match arm
        // in `tcp_endpoints`.
        #[cfg(unix)]
        assert!(
            capture_unavailable_reason("postgres:///db?host=/var/run/postgresql").is_some(),
            "a Unix-socket URL has no stream to tee"
        );
    }

    fn scope_for_test(id: &str) -> Arc<CaptureScope> {
        Arc::new(CaptureScope::new(
            id.to_owned(),
            Arc::new(crate::capsule::CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::new(&[], &[])),
        ))
    }

    #[tokio::test]
    async fn a_shard_checkout_notes_and_truncates_the_in_flight_capsule() {
        let scope = scope_for_test("shard-gap");
        crate::capsule::capture::CAPSULE_SCOPE
            .scope(Arc::clone(&scope), async {
                note_shard_capture_gap();
            })
            .await;

        assert!(
            scope.notes().iter().any(|note| note == SHARD_CAPTURE_NOTE),
            "the capsule must say the shard traffic is missing, got {:?}",
            scope.notes()
        );
        assert!(
            scope.is_truncated(),
            "a capsule missing its shard effects must be refused by replay, not \
             presented as complete"
        );
    }

    #[tokio::test]
    async fn a_shard_checkout_outside_a_capture_scope_is_a_no_op() {
        // Nothing is being captured: the checkout path must not reach for a
        // scope that is not there.
        note_shard_capture_gap();

        // And a scope untouched by a shard checkout stays complete.
        let scope = scope_for_test("no-shard");
        assert!(scope.notes().is_empty());
        assert!(!scope.is_truncated());
    }
}
