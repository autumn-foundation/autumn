//! Database connection pool and extractor.
//!
//! This module provides async connectivity via `diesel-async` with the
//! `deadpool` connection pool. The backend is whichever [`RuntimeConnection`]
//! resolves to — Postgres by default, `SQLite` under the crate's `sqlite`
//! feature. The pool is created at startup by
//! [`AppBuilder::run`](crate::app::AppBuilder::run) and stored in
//! [`crate::state::AppState`].
//!
//! When no `database.primary_url` or legacy `database.url` is configured,
//! [`create_pool`] returns `Ok(None)` and the application runs without a
//! database -- useful for static-site or API-gateway use cases.
//!
//! # The [`Db`] extractor
//!
//! Declare `db: Db` in your handler signature to get a pooled connection.
//! The connection is automatically returned to the pool when `Db` is dropped
//! at the end of the request.
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//!
//! #[get("/hello")]
//! async fn hello(db: Db) -> AutumnResult<String> {
//!     // Use `db` with Diesel queries...
//!     Ok("hello from db".to_string())
//! }
//! ```

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use axum::extract::FromRequestParts;
use diesel;
// Named by the default Postgres pool builder/TLS connector and by the
// Postgres migration connection (`MigrationConnection`), which stays compiled
// under the `sqlite` feature (diesel's `postgres` backend is still in the graph
// via `db`), so this import is used on both builds.
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
/// The deadpool connection pool Autumn's database seam produces.
///
/// Re-exported so a plugin implementing
/// [`DatabasePoolProvider`] — a stable plugin surface (issue #1601) — can name
/// the type its `create_pool` returns without taking its own `diesel-async`
/// dependency at a matching major.
pub use diesel_async::pooled_connection::deadpool::Pool;
use futures::FutureExt as _;
use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::Instrument as _;

use crate::config::DatabaseConfig;
use crate::error::AutumnError;

/// The database connection type the runtime is built against.
///
/// Defaults to Postgres (`AsyncPgConnection`). Enabling the crate's `sqlite`
/// feature flips it to a `SQLite` connection. Threading this alias (instead of a
/// hard-coded `AsyncPgConnection`) through the connection-typed surface — the
/// pool, the pooled connection, the transaction/query helpers, and the
/// `#[repository]`/`#[model]` generated code — is what lets the same codebase
/// target either backend.
///
/// FEATURE-UNIFICATION HAZARD: the `sqlite` feature must ONLY be enabled by an
/// end application (or an explicit `--features sqlite` build). If any workspace
/// crate or dev-dependency enables it, cargo feature unification flips this
/// alias for every consumer in the graph and breaks the Postgres default. See
/// the doc comment above `sqlite = []` in `autumn/Cargo.toml`.
#[cfg(not(feature = "sqlite"))]
pub type RuntimeConnection = diesel_async::AsyncPgConnection;
/// See [`RuntimeConnection`] (Postgres variant) for the full contract. Under
/// the `sqlite` feature the runtime is built against a `SQLite` connection.
#[cfg(feature = "sqlite")]
pub type RuntimeConnection =
    diesel_async::sync_connection_wrapper::SyncConnectionWrapper<diesel::SqliteConnection>;

/// The diesel query backend the runtime is built against — the companion of
/// [`RuntimeConnection`].
///
/// Defaults to Postgres (`diesel::pg::Pg`); the `sqlite` feature flips it to
/// `diesel::sqlite::Sqlite`. Generated `#[repository]`/`#[model]` CRUD names
/// this alias (as `::autumn_web::RuntimeBackend`) wherever a diesel
/// `QueryFragment<_>` / `SelectableHelper<_>` bound must resolve to the *active*
/// backend rather than a hard-coded `Pg`. Threading it (instead of `Pg`) through
/// the always-emitted upsert set-clause and boxed-query types is what lets the
/// same generated code type-check on either backend. Genuinely Postgres-only
/// query fragments (advisory-lock upserts, FTS `searchable`) keep an explicit
/// `Pg` bound — they are not portable and are cfg-gated off under `sqlite`.
#[cfg(not(feature = "sqlite"))]
pub type RuntimeBackend = diesel::pg::Pg;
/// See [`RuntimeBackend`] (Postgres variant). Under the `sqlite` feature the
/// runtime query backend is `diesel::sqlite::Sqlite`.
#[cfg(feature = "sqlite")]
pub type RuntimeBackend = diesel::sqlite::Sqlite;

// ── After-commit callback infrastructure ─────────────────────────────────────

/// A boxed async callback registered for post-transaction execution.
///
/// Stored in [`AFTER_COMMIT_REGISTRY`] during an active [`Db::tx`] block.
/// The registry is drained and each callback is awaited after the transaction
/// commits successfully. On rollback or panic the callbacks are dropped
/// without being called.
pub type CommitCallback = Box<
    dyn FnOnce() -> Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send + 'static>>
        + Send
        + 'static,
>;

tokio::task_local! {
    /// Task-local registry used by [`Db::tx`] to accumulate after-commit
    /// callbacks. Only set while the [`Db::tx`] future is being polled;
    /// absent outside a transaction block.
    pub static AFTER_COMMIT_REGISTRY: Arc<Mutex<Vec<CommitCallback>>>;
}

/// Per-request accumulator for database query timing, used by the
/// `Server-Timing` middleware to surface `db;dur=…;desc="N queries"`.
///
/// Populated by the [`RequestQueryTimer`] connection instrumentation, which
/// [`Db::checkout`] installs only on connections checked out while this
/// task-local is scoped (i.e. while the `ServerTimingLayer` has scoped the
/// request — see [`request_db_timing_active`]). The timer only *records* when
/// the task-local is scoped; a stale one left on a reused connection has an
/// `on_start` that is a cheap bool-probe no-op formatting and recording nothing. It
/// fires on every diesel-async query (including the raw `.load()`/`.execute()`
/// calls generated repositories run). The connection instrumentation is the
/// sole writer during a request — helpers like [`run_instrumented`]
/// deliberately do *not* record here, to avoid double-counting the same
/// statement. When the middleware is disabled the scope is absent, so opted-out
/// requests pay only that per-query bool probe (no `DebugQuery` formatting, no
/// allocation).
#[derive(Default, Debug)]
pub(crate) struct RequestDbTimings {
    /// Total elapsed time across all DB queries for this request, in
    /// microseconds. Microseconds keep the header format (`f64` ms rounded
    /// to three decimals) consistent with the access-log clock without an
    /// f64 atomic.
    pub(crate) total_us: AtomicU64,
    /// Number of instrumented DB queries this request performed.
    pub(crate) query_count: std::sync::atomic::AtomicUsize,
}

tokio::task_local! {
    /// Task-local accumulator populated by DB instrumentation and read by
    /// the `Server-Timing` middleware. See [`RequestDbTimings`].
    pub(crate) static REQUEST_DB_TIMINGS: Arc<RequestDbTimings>;
}

tokio::task_local! {
    /// Task-local sink for request-wide SQL query capture, independent of the
    /// `Server-Timing` timing accumulator ([`REQUEST_DB_TIMINGS`]).
    ///
    /// Scoped by the test harness (`RequestBuilder::send`) around the whole
    /// request so every instrumented statement is retained as a
    /// [`crate::inspector::QueryRecord`] for `TestResponse` query-count / N+1
    /// assertions. Kept as a separate task-local lane — not folded into
    /// `RequestDbTimings` — so query capture is unaffected by how the
    /// `Server-Timing` middleware scopes (and nests) its per-scope timing
    /// accumulators. Absent in production, where the capture lane is never
    /// scoped and nothing is retained.
    #[cfg(feature = "db")]
    pub(crate) static REQUEST_QUERY_CAPTURE: Arc<Mutex<Vec<crate::inspector::QueryRecord>>>;
}

/// Strip the trailing `-- binds: [...]` annotation that diesel's `DebugQuery`
/// `Display` appends to a query's SQL text.
///
/// diesel-async feeds each real query into the `StartQuery` instrumentation
/// event as a `DebugQuery` (see `diesel::debug_query` /
/// `AsyncPgConnection::with_prepared_statement`), whose `Display` renders
/// `"{sql} -- binds: {binds:?}"` — the format lives in
/// `diesel::query_builder::debug_query::display`
/// (`write!(f, "{query} -- binds: {debug_binds:?}")`). So the SQL text we
/// materialise from `query.to_string()` is e.g.
/// `SELECT * FROM books WHERE author_id = $1 -- binds: [1]`.
///
/// The per-row executions of an N+1 pattern share the same parameterised
/// statement (`… WHERE author_id = $1`) and differ only in their bind values
/// (`-- binds: [1]`, `-- binds: [2]`, …). If the capture path retained that
/// annotation, [`crate::inspector::normalize_sql`] (which only collapses
/// whitespace and lower-cases) would treat each per-row execution as a distinct
/// template, so [`crate::inspector::detect_n_plus_one`] would never see the
/// repetition and `assert_no_n_plus_one()` would miss the N+1 it exists to
/// catch. Truncating at the `-- binds` marker leaves the parameterised
/// statement — `$N` placeholders intact — so repeated per-row queries collapse
/// to one template.
///
/// Robust to input without the marker (transaction-control SQL diesel runs via
/// `batch_execute`, or synthetic test input): returns the input unchanged. Uses
/// the last occurrence, since diesel always appends the annotation after the
/// full statement.
#[cfg(feature = "db")]
fn strip_bind_annotation(sql: &str) -> &str {
    sql.rfind("-- binds:")
        .map_or(sql, |idx| sql[..idx].trim_end())
}

/// Record one instrumented DB query into the current request's
/// [`REQUEST_DB_TIMINGS`], if any. No-op when the task-local is unset —
/// which is the case whenever the `Server-Timing` middleware is disabled
/// or the query runs outside a request (e.g. background job).
pub(crate) fn record_request_db_query(elapsed: Duration, sql: Option<&str>) {
    // Lane 1: the `Server-Timing` timing accumulator (count + cumulative time).
    let _ = REQUEST_DB_TIMINGS.try_with(|t| {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        t.total_us.fetch_add(micros, Ordering::Relaxed);
        t.query_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });

    // Lane 2: request-wide SQL capture, independent of the timing lane. Only
    // active when a test scoped `REQUEST_QUERY_CAPTURE`; a no-op otherwise
    // (production, background jobs). The two `try_with` calls are independent —
    // either lane may be active without the other — and no lock is held across
    // an await.
    #[cfg(feature = "db")]
    if let Some(sql) = sql {
        let _ = REQUEST_QUERY_CAPTURE.try_with(|sink| {
            if let Ok(mut list) = sink.lock() {
                list.push(crate::inspector::QueryRecord {
                    // Store the parameterised statement WITHOUT diesel's per-row
                    // `-- binds: [...]` annotation (the `$N` placeholders stay), so
                    // repeated per-row queries normalise to the same template and
                    // `detect_n_plus_one` can see the repetition. See
                    // [`strip_bind_annotation`].
                    sql: strip_bind_annotation(sql).to_owned(),
                    params: Vec::new(),
                    elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    location: String::new(),
                });
            }
        });
    }
}

/// Whether the current task is inside a [`REQUEST_DB_TIMINGS`] scope — i.e.
/// whether the `ServerTimingLayer` (only active when `[observability]
/// server_timing` is enabled) has wrapped this request's handler future.
///
/// Cheap: probes the task-local without cloning the `Arc`, allocating, or
/// formatting anything. Probed by [`RequestQueryTimer::on_start`] on every
/// query: when no scope is active (the production default, `server_timing`
/// unset), the timer records nothing and skips the `DebugQuery` formatting, so
/// an always-installed timer costs opted-out requests only this bool probe per
/// query — no per-statement allocation.
#[cfg(feature = "db")]
pub(crate) fn request_db_timing_active() -> bool {
    REQUEST_DB_TIMINGS.try_with(|_| ()).is_ok()
}

/// Whether the current task is inside a [`REQUEST_QUERY_CAPTURE`] scope — i.e.
/// whether the test harness (`RequestBuilder::send`) has scoped a capture sink
/// around this request.
///
/// Mirrors [`request_db_timing_active`]: a cheap task-local probe that neither
/// clones the `Arc` nor allocates. [`Db::checkout`] installs the
/// [`RequestQueryTimer`] instrumentation when EITHER this lane or the timing
/// lane is active, so query capture works even when `server_timing` is off (the
/// common test case) and no `REQUEST_DB_TIMINGS` scope exists.
#[cfg(feature = "db")]
pub(crate) fn request_query_capture_active() -> bool {
    REQUEST_QUERY_CAPTURE.try_with(|_| ()).is_ok()
}

/// diesel-async [`Instrumentation`](diesel::connection::Instrumentation)
/// that feeds every executed statement into the per-request
/// [`REQUEST_DB_TIMINGS`] accumulator for the `Server-Timing` `db` metric.
///
/// Installed on a pooled connection at [`Db::checkout`] only when a
/// [`REQUEST_DB_TIMINGS`] scope is active (see [`request_db_timing_active`]),
/// i.e. only for requests the `ServerTimingLayer` is measuring — so opted-out
/// requests never install it and pay no per-query overhead. Because generated
/// repositories run raw `diesel-async` `.load()`/`.execute()` (they do not go
/// through [`run_instrumented`]), the connection-level instrumentation is the
/// only thing that observes those queries. It brackets each statement between
/// the `StartQuery`/`FinishQuery` events the connection emits and records the
/// elapsed wall time via [`record_request_db_query`], which is a no-op when no
/// request has scoped the task-local (background jobs, or the middleware being
/// disabled) — so it never panics off-request.
///
/// Only genuine application statements are timed. The dedicated
/// connection-establish, prepared-statement-cache, and
/// `BeginTransaction`/`CommitTransaction`/`RollbackTransaction` events are
/// ignored outright. Crucially, diesel-async runs the transaction-control SQL
/// itself (`BEGIN`, `COMMIT`, `ROLLBACK`, and the `SAVEPOINT`/`RELEASE`
/// variants for nested transactions) through `batch_execute`, which emits a
/// `StartQuery`/`FinishQuery` pair with that SQL — exactly like a real query.
/// Left unfiltered, a transaction wrapping a single `SELECT` would report
/// `desc="3 queries"` (BEGIN + SELECT + COMMIT) and fold begin/commit latency
/// into `db;dur`. Likewise [`Db::checkout`] issues a `SET statement_timeout`
/// housekeeping statement on every checkout, which would otherwise add a bogus
/// `+1 query` to every request before any app SQL runs. So at `StartQuery` we
/// inspect the statement text and skip timing when it is a housekeeping /
/// transaction-control command (see
/// [`RequestQueryTimer::is_uncounted_statement`]). Queries on a single
/// connection are strictly sequential (`&mut conn`), so a single
/// `Option<Instant>` slot is sufficient — there is no overlap between a start
/// and its finish, and skipping a start leaves the slot empty so the matching
/// finish is a no-op.
///
/// The timer is installed at [`Db::checkout`] (before the housekeeping `SET`)
/// **only when a `REQUEST_DB_TIMINGS` scope is active** — i.e. only for requests
/// the `ServerTimingLayer` is measuring. This gate matters because
/// `set_instrumentation` wholesale replaces the connection's instrumentation:
/// installing unconditionally would clobber any global default an application
/// registered via `diesel::connection::set_default_instrumentation` (query
/// logging, tracing, metrics), even when `server_timing` is disabled. Gating on
/// the scope leaves an opted-out app's instrumentation fully intact. When the
/// scope *is* active, installing a fresh timer per checkout also clears any
/// stale timer a pooled connection carried from a prior timed request. A stale
/// timer left on a connection later reused by an opted-out request is a cheap
/// no-op: `on_start` probes [`request_db_timing_active`] before doing any work,
/// so it never formats SQL or allocates off-scope.
///
/// Autumn does not currently *compose* with an app-registered
/// `set_default_instrumentation` — while `server_timing` is enabled its timer
/// replaces the app's on measured checkouts. This is a documented limitation
/// (`server_timing` is a dev/off-by-default feature); apps needing both should
/// keep it disabled in that environment.
#[cfg(feature = "db")]
pub(crate) struct RequestQueryTimer {
    /// The currently in-flight statement (start instant + SQL text), if any.
    pending: Option<PendingQuery>,
    /// Injected clock supplying the start/finish instants.
    ///
    /// Diesel's `Instrumentation::on_connection_event` signature is frozen and
    /// carries no time, so the only way to reach the app's clock from inside it
    /// is a field on the timer. Installed per checkout from
    /// `DbCheckoutParams::clock`, so a `#[sim_test]` sees virtual query
    /// latencies. It costs one `Arc` deref per statement — and only on
    /// connections that actually carry a timer, which `Db::checkout` installs
    /// solely while a query observer is scoped.
    clock: std::sync::Arc<dyn crate::time::ClockSource>,
}

#[cfg(feature = "db")]
impl std::fmt::Debug for RequestQueryTimer {
    // Hand-written because `dyn ClockSource` is not `Debug`; the clock is an
    // injected handle with no useful representation here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestQueryTimer")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "db")]
impl Default for RequestQueryTimer {
    /// A timer on the real system clock — the behaviour before the clock became
    /// injectable. Used by the unit tests that drive `on_start`/`on_finish`
    /// directly with synthetic instants and never read this field.
    fn default() -> Self {
        Self {
            pending: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        }
    }
}

/// A statement whose `StartQuery` has fired but whose `FinishQuery` has not.
///
/// Holds the start instant (for latency) and the materialised SQL text (so a
/// capturing accumulator can retain it). Queries on a single connection are
/// strictly sequential, so a single slot suffices.
#[cfg(feature = "db")]
#[derive(Debug)]
struct PendingQuery {
    started_at: crate::time::MonotonicInstant,
    sql: String,
}

#[cfg(feature = "db")]
impl RequestQueryTimer {
    /// A timer reading its instants from `clock` (the app's injected clock).
    fn with_clock(clock: std::sync::Arc<dyn crate::time::ClockSource>) -> Self {
        Self {
            pending: None,
            clock,
        }
    }

    /// Whether `sql` is a statement that must **not** be counted as an
    /// application query in the `Server-Timing` `db` metric. Two kinds reach
    /// the connection instrumentation but are not application work:
    ///
    /// * **Transaction control** that diesel-async runs via `batch_execute`
    ///   (which emits a `StartQuery`/`FinishQuery` pair just like a real
    ///   query): `BEGIN`, `COMMIT`, `ROLLBACK`, the `SAVEPOINT`/`RELEASE`
    ///   nested-transaction variants, and the two-word `START TRANSACTION` form.
    /// * **Session/config housekeeping** — any leading-token `SET`. This covers
    ///   the `SET statement_timeout` that [`Db::checkout`] issues on every
    ///   checkout before handing the connection to the request, as well as
    ///   `SET TRANSACTION`. None of these are application queries.
    ///
    /// Matches a case-insensitive leading token in
    /// `{BEGIN, COMMIT, ROLLBACK, SAVEPOINT, RELEASE, SET}`, plus the two-word
    /// `START TRANSACTION` form. See the type-level docs.
    fn is_uncounted_statement(sql: &str) -> bool {
        let mut tokens = sql.split_whitespace();
        let Some(first) = tokens.next() else {
            return false;
        };
        match first.to_ascii_uppercase().as_str() {
            "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "SET" => true,
            "START" => tokens
                .next()
                .is_some_and(|second| second.eq_ignore_ascii_case("TRANSACTION")),
            _ => false,
        }
    }

    /// Record the start of a statement.
    ///
    /// Probes both [`request_db_timing_active`] and
    /// [`request_query_capture_active`] first: when NEITHER the timing
    /// accumulator nor the capture sink is scoped (the opted-out / off-request
    /// path) the timer does nothing and — crucially — never invokes `sql`, so
    /// the caller's `DebugQuery` `to_string()` allocation is skipped entirely.
    /// This is what makes it safe to leave a `RequestQueryTimer` installed on a
    /// pooled connection that a later opted-out request reuses: the stale timer
    /// is a cheap bool-probe no-op, not a per-query allocator.
    ///
    /// When either lane *is* active, the SQL text is materialised and inspected:
    /// housekeeping / transaction-control statements (see
    /// [`Self::is_uncounted_statement`], e.g. the checkout `SET
    /// statement_timeout`) leave the slot empty so they are excluded from the
    /// accumulator; genuine application statements record their start instant.
    /// Clearing the slot keeps the matching `FinishQuery` a no-op.
    ///
    /// `sql` is a closure so the (allocating) formatting is deferred until we
    /// know the statement will be counted. Extracted from the event handler so
    /// the start/finish accounting is unit-testable without constructing a
    /// (non-exhaustive, unstable-to-build) `InstrumentationEvent`.
    fn on_start(&mut self, now: crate::time::MonotonicInstant, sql: impl FnOnce() -> String) {
        // Probe BOTH lanes: the timing accumulator (`server_timing`) and the
        // query-capture sink (test harness). Either being active means the
        // upcoming statement must be observed. When neither is scoped (the
        // opted-out / off-request path) the timer does nothing and never
        // invokes `sql`, so the caller's `DebugQuery` `to_string()` allocation
        // is skipped — keeping a stale installed timer a cheap bool-probe no-op.
        if !request_db_timing_active() && !request_query_capture_active() {
            self.pending = None;
            return;
        }
        // Materialise the SQL once: it is needed both for the housekeeping
        // filter and (when a test opted into capture) to retain the statement
        // text at `on_finish`.
        let sql = sql();
        self.pending = if Self::is_uncounted_statement(&sql) {
            None
        } else {
            Some(PendingQuery {
                started_at: now,
                sql,
            })
        };
    }

    /// Record the completion of the in-flight statement, accumulating its
    /// elapsed time into the per-request accumulator. A `FinishQuery` without
    /// a matching `StartQuery` is ignored.
    fn on_finish(&mut self, now: crate::time::MonotonicInstant) {
        if let Some(p) = self.pending.take() {
            record_request_db_query(now.saturating_duration_since(p.started_at), Some(&p.sql));
        }
    }
}

#[cfg(feature = "db")]
impl diesel::connection::Instrumentation for RequestQueryTimer {
    fn on_connection_event(&mut self, event: diesel::connection::InstrumentationEvent<'_>) {
        use diesel::connection::InstrumentationEvent;
        match event {
            InstrumentationEvent::StartQuery { query, .. } => {
                // `query` is an opaque `&dyn DebugQuery`; its `Display` impl
                // renders the SQL text, which we inspect to skip housekeeping /
                // transaction-control statements (see the type-level docs). The
                // `to_string()` is deferred behind a closure so an installed but
                // opted-out timer never pays the allocation — see `on_start`.
                let now = self.clock.monotonic();
                self.on_start(now, || query.to_string());
            }
            InstrumentationEvent::FinishQuery { .. } => {
                let now = self.clock.monotonic();
                self.on_finish(now);
            }
            // Ignore connection-establish, prepared-statement cache, and the
            // dedicated Begin/Commit/RollbackTransaction events — see the
            // type-level docs.
            _ => {}
        }
    }
}

/// Total count of after-commit callback errors since process start.
///
/// Incremented each time a callback registered via [`register_after_commit`]
/// or [`Db::tx`] returns an error **after** the transaction has already
/// committed. The underlying transaction is unaffected; this counter surfaces
/// failures for alerting and dashboards.
///
/// Exposed by the `/actuator/health` endpoint as the top-level
/// `autumn_after_commit_failures_total` field.
pub static AFTER_COMMIT_FAILURES_TOTAL: AtomicU64 = AtomicU64::new(0);

pub(crate) fn record_after_commit_failure() -> u64 {
    AFTER_COMMIT_FAILURES_TOTAL.fetch_add(1, Ordering::Relaxed) + 1
}

/// Total number of transaction retries triggered by a transient serialization
/// failure (`40001`) or deadlock (`40P01`) since process start.
///
/// Incremented by [`Db::tx_with`] each time a retryable transaction error is
/// re-run under a stronger isolation level. Surfaces contention on the existing
/// metrics surface (`autumn_tx_retries_total`).
pub static TX_RETRIES_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Total number of transactions that exhausted their retry budget without
/// succeeding, since process start. Exposed as `autumn_tx_retry_exhausted_total`.
pub static TX_RETRY_EXHAUSTED_TOTAL: AtomicU64 = AtomicU64::new(0);

// The transaction serialization-failure retry loop is Postgres-only (SQLite's
// `tx_with` runs a single plain transaction), so these retry-metric helpers are
// unused in a `--features sqlite` library build. They are still exercised by the
// crate's unit tests, so keep them compiled and only silence the dead-code
// warning under the feature.
#[cfg_attr(feature = "sqlite", allow(dead_code))]
pub(crate) fn record_tx_retry() -> u64 {
    TX_RETRIES_TOTAL.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg_attr(feature = "sqlite", allow(dead_code))]
pub(crate) fn record_tx_retry_exhausted() -> u64 {
    TX_RETRY_EXHAUSTED_TOTAL.fetch_add(1, Ordering::Relaxed) + 1
}

/// Guidance appended to the nested-`tx` rejection, naming the supported
/// alternative for a same-connection nested transaction.
const NESTED_TX_MESSAGE: &str = "Nested Db::tx calls are not supported; use \
    autumn_web::db::savepoint(conn, ..) inside the closure for a same-connection savepoint";

pub(crate) fn reject_ambient_after_commit_registry_for_tx() -> Result<(), AutumnError> {
    if AFTER_COMMIT_REGISTRY.try_with(|_| ()).is_ok() {
        return Err(AutumnError::bad_request_msg(NESTED_TX_MESSAGE));
    }
    Ok(())
}

pub(crate) fn spawn_committed_after_commit_callbacks(
    callbacks: Vec<CommitCallback>,
) -> Option<tokio::task::JoinHandle<()>> {
    if callbacks.is_empty() {
        return None;
    }

    Some(tokio::task::spawn(async move {
        for cb in callbacks {
            let result = match std::panic::catch_unwind(AssertUnwindSafe(cb)) {
                Ok(callback) => AssertUnwindSafe(callback).catch_unwind().await,
                Err(panic) => Err(panic),
            };

            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let failures_total = record_after_commit_failure();
                    tracing::error!(
                        autumn.after_commit.failures_total = failures_total,
                        "after_commit callback failed (tx already committed): {e}"
                    );
                }
                Err(panic) => {
                    let failures_total = record_after_commit_failure();
                    let panic = after_commit_panic_message(&*panic);
                    tracing::error!(
                        autumn.after_commit.failures_total = failures_total,
                        "after_commit callback panicked (tx already committed): {panic}"
                    );
                }
            }
        }
    }))
}

fn after_commit_panic_message(payload: &(dyn Any + Send)) -> String {
    match (
        payload.downcast_ref::<&'static str>(),
        payload.downcast_ref::<String>(),
    ) {
        (Some(message), _) => (*message).to_owned(),
        (_, Some(message)) => message.clone(),
        (None, None) => "non-string panic payload".to_owned(),
    }
}

/// Register a callback to run after the current database transaction commits.
///
/// If called inside a [`Db::tx`] block, the callback is deferred until the
/// transaction commits successfully. On rollback the callback is dropped
/// without being called.
///
/// The deferred callback is process-local work spawned after commit. It avoids
/// side effects for rolled-back transactions, but it is not a crash-safe
/// delivery mechanism. For side effects that must survive process exit, write a
/// durable outbox or queue row inside the same database transaction and use
/// this callback only as an optional wake-up hint.
///
/// If called **outside** any active transaction, the callback runs immediately
/// (eager execution) with a `debug`-level log note.
///
/// # Panics
///
/// Panics if the internal registry mutex is poisoned (only possible if a
/// previous thread holding the lock panicked, which should not occur in normal
/// operation).
///
/// # Example
///
/// ```rust,ignore
/// db.tx(move |conn| {
///     scoped_boxed(async move {
///         diesel::insert_into(users::table).values(&new_user).execute(conn).await?;
///         autumn_web::db::register_after_commit(|| async {
///             welcome_email_job.enqueue("user_id", user_id).await
///         }).await;
///         Ok(())
///     })
/// }).await?;
/// ```
pub async fn register_after_commit<F, Fut>(f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = crate::AutumnResult<()>> + Send + 'static,
{
    let mut f_opt = Some(f);
    AFTER_COMMIT_REGISTRY
        .try_with(|registry| {
            let f = f_opt.take().expect("closure only entered once");
            let boxed: CommitCallback = Box::new(move || Box::pin(f()));
            registry.lock().expect("registry lock").push(boxed);
        })
        .ok();

    // If still Some, the task-local wasn't set — we're outside a tx; run eagerly.
    if let Some(f) = f_opt {
        tracing::debug!("register_after_commit: no active transaction; running callback eagerly");
        if let Err(e) = f().await {
            let failures_total = record_after_commit_failure();
            tracing::error!(
                autumn.after_commit.failures_total = failures_total,
                "register_after_commit eager callback failed: {e}"
            );
        }
    }
}

/// Trait to abstract the state requirement for the `Db` extractor.
/// This breaks the circular dependency between the database extractor
/// and the central `AppState`.
/// Process-wide handle to the real system clock, cloned by
/// [`DbState::clock`]'s default body and by the state-less checkout paths.
///
/// A fresh `Arc::new(SystemClock)` per DB extraction would heap-allocate for a
/// zero-sized type on every request; cloning one shared handle is a relaxed
/// refcount bump.
static DEFAULT_SYSTEM_CLOCK: std::sync::LazyLock<std::sync::Arc<dyn crate::time::ClockSource>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(crate::time::SystemClock));

pub trait DbState {
    /// Returns the database connection pool, if configured.
    fn pool(&self) -> Option<&Pool<RuntimeConnection>>;

    /// Returns the metrics collector, if configured.
    fn metrics(&self) -> Option<&crate::middleware::MetricsCollector> {
        None
    }

    /// Returns the read/replica connection pool, if configured.
    fn replica_pool(&self) -> Option<&Pool<RuntimeConnection>> {
        None
    }

    /// Returns the pool used for read-only work.
    ///
    /// Defaults to the replica role when present, otherwise the primary role.
    fn read_pool(&self) -> Option<&Pool<RuntimeConnection>> {
        self.replica_pool().or_else(|| self.pool())
    }

    /// Returns the configured shard set, when `[[database.shards]]`
    /// entries exist. Defaults to `None` so unsharded states need no
    /// changes.
    fn shards(&self) -> Option<&crate::sharding::ShardSet> {
        None
    }

    /// Returns any registered database connection checkout interceptors.
    fn db_interceptors(
        &self,
    ) -> Vec<std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>> {
        Vec::new()
    }

    /// Why failure-capsule capture cannot record this app's database traffic,
    /// when its pool topology carries a gap (see
    /// [`DatabaseTopology::capture_gap`]). Defaults to `None`: the pools
    /// record, or capture is off entirely.
    #[cfg(feature = "reporting")]
    fn db_capture_gap(&self) -> Option<std::sync::Arc<str>> {
        None
    }
    /// Returns the global statement timeout, if configured.
    fn statement_timeout(&self) -> Option<std::time::Duration> {
        None
    }

    /// Returns the app's injected clock, used to time connection checkouts and
    /// individual statements.
    ///
    /// Defaults to the real [`crate::time::SystemClock`], which is exactly what
    /// this timing read was before the clock became injectable — so an existing
    /// `impl DbState` needs no change. [`crate::state::AppState`] overrides it
    /// with the clock the app was built with, which is what makes DB timings
    /// virtual (and reproducible) under a `#[sim_test]`.
    fn clock(&self) -> std::sync::Arc<dyn crate::time::ClockSource> {
        std::sync::Arc::clone(&DEFAULT_SYSTEM_CLOCK)
    }

    /// Returns the slow query threshold.
    fn slow_query_threshold(&self) -> std::time::Duration {
        std::time::Duration::from_millis(500)
    }
}

// ── SQL telemetry helpers ─────────────────────────────────────────────────────

/// Scrub a SQL string to remove literal parameter values.
///
/// Replaces values with `?` placeholders to prevent PII leakage in
/// slow-query logs while still surfacing the query shape for performance
/// analysis.
///
/// Rules:
/// - Single-quoted string literals `'...'` → `'?'`
/// - Unquoted integer/float literals → `?`
/// - Postgres `$N` positional parameters are left untouched
///
/// # Examples
///
/// ```
/// use autumn_web::db::scrub_sql;
///
/// assert_eq!(scrub_sql("SELECT * FROM users WHERE name = 'Alice'"),
///            "SELECT * FROM users WHERE name = '?'");
/// assert_eq!(scrub_sql("SELECT * FROM orders WHERE id = 42"),
///            "SELECT * FROM orders WHERE id = ?");
/// assert_eq!(scrub_sql("SELECT * FROM t WHERE x = $1"),
///            "SELECT * FROM t WHERE x = $1");
/// ```
/// Consumes the body of an E-string escape literal and its closing `'`.
///
/// Called after the opening `'` has already been consumed. Handles
/// `\'` backslash-escaped quotes so they do not prematurely close the string.
fn consume_estring_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    loop {
        match chars.next() {
            None => break,
            Some('\'') => {
                if chars.peek() == Some(&'\'') {
                    chars.next(); // consume the doubled quote
                } else {
                    break;
                }
            }
            Some('\\') => {
                chars.next(); // skip the character after the backslash
            }
            Some(_) => {}
        }
    }
}

/// Consumes the body of a dollar-quoted string and its closing `$tag$`.
///
/// Called after the opening `$tag$` delimiter has already been consumed.
/// Uses a simple sliding-window match — sufficient for valid SQL.
fn consume_dollar_quoted_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, tag: &str) {
    let closing: Vec<char> = format!("${tag}$").chars().collect();
    let clen = closing.len();
    let mut match_count = 0usize;
    for sc in chars.by_ref() {
        if sc == closing[match_count] {
            match_count += 1;
            if match_count == clen {
                break; // Found the closing delimiter.
            }
        } else {
            match_count = 0;
            // The current char may start a new partial match.
            if sc == closing[0] {
                match_count = 1;
            }
        }
    }
}

/// Returns true for every char that can legally precede a bare numeric
/// literal in SQL — whitespace, comparison, arithmetic, and structural chars.
#[inline]
const fn is_separator(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n'          // whitespace
        | '=' | '<' | '>'          // comparison
        | '!' | '+' | '-'          // arithmetic / negation (signed literals)
        | '*' | '/' | '%'          // arithmetic operators
        | '(' | ',' // structure
    )
}

#[must_use]
pub fn scrub_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    // Tracks whether the last character written was a separator, so a digit
    // at the current position starts a standalone literal rather than being
    // part of an identifier like `table1` or `col2`.
    let mut prev_is_sep = true; // treat start-of-input as a separator boundary

    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        // ── E-string literal  E'...' / e'...'  (backslash-escape aware) ──
        // Must be checked before the single-quote handler so we consume the
        // `E` prefix and don't leave it in the fingerprint.
        if (c == 'E' || c == 'e') && chars.peek() == Some(&'\'') {
            chars.next(); // consume the opening '
            out.push_str("'?'");
            prev_is_sep = false;
            consume_estring_body(&mut chars);
            continue;
        }

        // ── Single-quoted string literal ─────────────────────────────────
        if c == '\'' {
            out.push_str("'?'");
            prev_is_sep = false;
            loop {
                match chars.next() {
                    None => break,
                    Some('\'') => {
                        if chars.peek() == Some(&'\'') {
                            // Escaped quote ('') — consume both, stay inside string
                            chars.next();
                        } else {
                            // Closing quote
                            break;
                        }
                    }
                    Some(_) => {}
                }
            }
            continue;
        }

        // ── Dollar sign: positional parameter or dollar-quoted string ─────
        if c == '$' {
            let next_ch = chars.peek().copied();

            // Positional parameter $N — pass through verbatim.
            if next_ch.is_some_and(|nc| nc.is_ascii_digit()) {
                out.push('$');
                prev_is_sep = false;
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    if let Some(d) = chars.next() {
                        out.push(d);
                    }
                }
                continue;
            }

            // Dollar-quoted string: $$ (anonymous) or $tag$ (tagged).
            // Collect the optional tag, looking for the second `$`.
            let mut tag = String::new();
            let mut found_closing_dollar = false;

            if next_ch == Some('$') {
                // Anonymous $$: consume the second `$`.
                chars.next();
                found_closing_dollar = true;
            } else if next_ch.is_some_and(|nc| nc.is_alphabetic() || nc == '_') {
                // Accumulate tag chars until we hit `$` or a non-identifier char.
                while let Some(&tc) = chars.peek() {
                    if tc == '$' {
                        chars.next(); // consume the closing `$` of the opening tag
                        found_closing_dollar = true;
                        break;
                    } else if tc.is_alphanumeric() || tc == '_' {
                        tag.push(tc);
                        chars.next();
                    } else {
                        // Not a valid tag character — not a dollar-quoted string.
                        break;
                    }
                }
            }

            if found_closing_dollar {
                out.push_str("'?'");
                prev_is_sep = false;
                consume_dollar_quoted_body(&mut chars, &tag);
            } else {
                // Not a recognisable dollar form — emit $ and any partial tag.
                out.push('$');
                out.push_str(&tag);
                prev_is_sep = false;
            }
            continue;
        }

        // ── Unquoted numeric literal ──────────────────────────────────────
        // Only scrub when preceded by a separator to avoid stomping on
        // identifiers like `table1`, `col2`, or `alias99`.
        let is_leading_dot =
            c == '.' && prev_is_sep && chars.peek().is_some_and(char::is_ascii_digit);
        if (c.is_ascii_digit() && prev_is_sep) || is_leading_dot {
            out.push('?');
            if is_leading_dot {
                chars.next(); // consume the leading dot
            }
            // Consume integer/decimal digits, underscores, and dots.
            while chars
                .peek()
                .is_some_and(|d| d.is_ascii_digit() || *d == '.' || *d == '_')
            {
                chars.next();
            }
            // Consume optional scientific-notation exponent: e/E [+/-] <digits>.
            if chars.peek().is_some_and(|e| *e == 'e' || *e == 'E') {
                chars.next(); // consume 'e'/'E'
                if chars.peek().is_some_and(|s| *s == '+' || *s == '-') {
                    chars.next(); // consume optional sign
                }
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    chars.next();
                }
            }
            prev_is_sep = false;
            continue;
        }

        // ── Regular character ─────────────────────────────────────────────
        out.push(c);
        prev_is_sep = is_separator(c);
    }

    out
}

/// Instrument a database query: time it, log slow queries with a scrubbed SQL
/// fingerprint, record metrics, and map Postgres `57014` (statement timeout)
/// to [`AutumnError::query_timeout`].
///
/// Times the query on the **real** system clock. Prefer
/// [`run_instrumented_with_clock`] where an injected clock is reachable, so the
/// recorded latency follows virtual time under a
/// [`#[sim_test]`](crate::sim_test).
///
/// # Parameters
/// - `sql`: The raw SQL string for slow-query fingerprinting (scrubbed before logging).
/// - `route_key`: Label string used for metrics, e.g. `"GET /users"`.
/// - `slow_threshold`: Queries taking longer than this emit a `WARN` log.
/// - `metrics`: The [`crate::middleware::MetricsCollector`] to record into.
/// - `query`: The async closure that actually executes the query.
///
/// # Returns
/// The result of `query()`, with Postgres `57014` mapped to
/// [`AutumnError::query_timeout`].
///
/// # Errors
/// Returns [`AutumnError`] from the underlying query, or [`AutumnError::query_timeout`]
/// when Postgres cancels the statement due to `statement_timeout`.
pub async fn run_instrumented<F, Fut, T>(
    sql: &str,
    route_key: &str,
    slow_threshold: std::time::Duration,
    metrics: &crate::middleware::metrics::MetricsCollector,
    query: F,
) -> Result<T, AutumnError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, diesel::result::Error>>,
{
    run_instrumented_with_clock(
        &crate::time::SystemClock,
        sql,
        route_key,
        slow_threshold,
        metrics,
        query,
    )
    .await
}

/// [`run_instrumented`], timing the query on an injected clock.
///
/// The determinism-seam twin (#1797): pass `state.clock()` and the recorded
/// latency follows virtual time under a [`#[sim_test]`](crate::sim_test)
/// instead of the real machine clock.
///
/// ```rust,ignore
/// let rows = autumn_web::db::run_instrumented_with_clock(
///     state.clock(),
///     "SELECT …",
///     "GET /users",
///     slow_threshold,
///     state.metrics(),
///     || users::table.load(&mut conn),
/// )
/// .await?;
/// ```
///
/// # Parameters
///
/// As [`run_instrumented`], plus `clock`: the source both timing readings are
/// taken from. Borrowed rather than owned so
/// [`AppState::clock`](crate::state::AppState::clock) — which hands out a
/// `&dyn ClockSource` — is directly usable; no `Arc` handle is needed.
///
/// # Errors
///
/// As [`run_instrumented`].
pub async fn run_instrumented_with_clock<F, Fut, T>(
    clock: &dyn crate::time::ClockSource,
    sql: &str,
    route_key: &str,
    slow_threshold: std::time::Duration,
    metrics: &crate::middleware::metrics::MetricsCollector,
    query: F,
) -> Result<T, AutumnError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, diesel::result::Error>>,
{
    let start = clock.monotonic();
    let result = query().await;
    let elapsed = clock.monotonic().saturating_duration_since(start);
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

    // Record metrics regardless of success/failure
    let verb = sql.split_whitespace().next().unwrap_or("?");
    let metric_key = format!("{route_key} {verb}");
    metrics.record_db_query(&metric_key, elapsed_ms);
    // NOTE: deliberately *not* recorded into the per-request Server-Timing
    // accumulator. The connection-level `RequestQueryTimer` instrumentation
    // (installed at `Db::checkout`) already brackets every executed statement
    // — including any query run through this helper — so recording here too
    // would double-count it in `db;dur` and `desc="N queries"`.

    // Log slow queries with scrubbed SQL
    if elapsed >= slow_threshold {
        let fingerprint = scrub_sql(sql);
        tracing::warn!(
            route = %route_key,
            sql = %fingerprint,
            duration_ms = elapsed_ms,
            "slow database query"
        );
    }

    // Map result — translate Postgres 57014 to query_timeout
    result.map_err(|db_err| {
        if is_query_canceled(&db_err) {
            tracing::warn!(
                route = %route_key,
                duration_ms = elapsed_ms,
                "database query cancelled: statement_timeout exceeded"
            );
            AutumnError::query_timeout(format!(
                "Database query timed out after {elapsed_ms}ms (statement_timeout exceeded)"
            ))
        } else {
            AutumnError::from(db_err)
        }
    })
}

/// Walk `err`'s source chain looking for a `tokio_postgres` SQLSTATE matching
/// `predicate`, downcasting each link to [`tokio_postgres::Error`] and
/// [`tokio_postgres::error::DbError`] in turn. Shared by [`is_query_canceled`]
/// and [`is_retryable_txn_error`] — both need the same downcast-through-the-
/// chain strategy, only with a different SQLSTATE to look for.
fn source_chain_has_sqlstate(
    err: &(dyn std::error::Error + 'static),
    predicate: impl Fn(&tokio_postgres::error::SqlState) -> bool,
) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            .is_some_and(&predicate)
        {
            return true;
        }
        if e.downcast_ref::<tokio_postgres::error::DbError>()
            .is_some_and(|db_err| predicate(db_err.code()))
        {
            return true;
        }
        source = e.source();
    }
    false
}

/// Check whether a Diesel error wraps a Postgres `57014` `query_canceled` error.
///
/// Prefers downcasting through the source chain to find a
/// [`tokio_postgres::Error`] and checking its SQL state code directly,
/// which is more robust than string-matching error messages.
fn is_query_canceled(err: &diesel::result::Error) -> bool {
    // Robust string-matching first to catch wrapped/unwrapped representations
    let err_str = err.to_string().to_lowercase();
    if err_str.contains("57014")
        || err_str.contains("query_canceled")
        || err_str.contains("canceling statement due to statement timeout")
        || err_str.contains("statement timeout")
        || err_str.contains("query canceled")
    {
        return true;
    }

    source_chain_has_sqlstate(err, |state| {
        *state == tokio_postgres::error::SqlState::QUERY_CANCELED
    })
}

/// Error type for pool creation failures.
///
/// Returned by [`create_pool`] (and the other topology builders) when a pool
/// cannot be constructed. Historically this was a bare alias for deadpool's
/// `BuildError`; it now also carries the boot-time refusal emitted when the
/// configured backend is not the one this build serves — a `SQLite` target in a
/// default build, or a Postgres target under `--features sqlite` (issue #1614)
/// — so callers fail fast at pool construction with an actionable message
/// instead of at the first query. The [`Build`](PoolError::Build) variant delegates its `Display` to the
/// underlying `BuildError`, so the Postgres path's error text is unchanged.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PoolError {
    /// The underlying deadpool builder failed (e.g., timeouts configured
    /// without a runtime, or an invalid max-size configuration).
    #[error(transparent)]
    Build(#[from] diesel_async::pooled_connection::deadpool::BuildError),

    /// A recognized backend that this build does not serve was configured. The
    /// message names the feature that serves it. See
    /// [`DatabaseBackend`](crate::config::DatabaseBackend).
    #[error("{0}")]
    UnsupportedBackend(String),
}

/// Primary plus optional read-replica database pools.
#[derive(Clone)]
pub struct DatabaseTopology {
    primary: Pool<RuntimeConnection>,
    replica: Option<Pool<RuntimeConnection>>,
    /// Connection URL to target with startup migrations, when the provider
    /// resolved one at runtime that the static config doesn't carry (e.g. the
    /// managed-Postgres provider whose socket URL is only known after boot).
    /// Scoping it to the topology keeps it per-app instead of a process global.
    migration_url: Option<String>,
    /// Why failure-capsule capture cannot record this topology's traffic, when
    /// the pool factory had to step aside (a TLS-required URL, a custom
    /// provider). Scoped to the topology for the same reason as
    /// `migration_url`: two apps in one process can disagree, and one app's
    /// gap must never mark the other app's capsules truncated.
    #[cfg(feature = "reporting")]
    capture_gap: Option<String>,
}

impl DatabaseTopology {
    /// Build a topology from explicit primary and optional replica pools.
    ///
    /// This is useful for custom [`DatabasePoolProvider`] implementations that
    /// need to create or decorate both roles themselves.
    #[must_use]
    pub const fn from_pools(
        primary: Pool<RuntimeConnection>,
        replica: Option<Pool<RuntimeConnection>>,
    ) -> Self {
        Self {
            primary,
            replica,
            migration_url: None,
            #[cfg(feature = "reporting")]
            capture_gap: None,
        }
    }

    /// Build a topology from a primary pool only.
    #[must_use]
    pub const fn primary_only(primary: Pool<RuntimeConnection>) -> Self {
        Self {
            primary,
            replica: None,
            migration_url: None,
            #[cfg(feature = "reporting")]
            capture_gap: None,
        }
    }

    /// Attach a runtime-resolved migration URL (see [`Self::migration_url`]).
    ///
    /// Providers whose primary URL isn't present in the static config — such as
    /// the managed-Postgres provider — call this so startup migrations target
    /// the pool that was actually built, without publishing the URL to a
    /// process-global shared across every app instance.
    #[must_use]
    pub fn with_migration_url(mut self, url: Option<String>) -> Self {
        self.migration_url = url;
        self
    }

    /// The runtime-resolved migration URL, if the provider supplied one.
    #[must_use]
    pub fn migration_url(&self) -> Option<&str> {
        self.migration_url.as_deref()
    }

    /// Record why failure-capsule capture cannot record this topology's
    /// traffic (see [`Self::capture_gap`]).
    #[cfg(feature = "reporting")]
    #[must_use]
    pub fn with_capture_gap(mut self, reason: Option<String>) -> Self {
        self.capture_gap = reason;
        self
    }

    /// Why failure-capsule capture cannot record this topology's traffic, or
    /// `None` when it can (or capture is off).
    ///
    /// Carried on the topology — not in process state — so two apps in one
    /// process each note their own gap on their own capsules.
    #[cfg(feature = "reporting")]
    #[must_use]
    pub fn capture_gap(&self) -> Option<&str> {
        self.capture_gap.as_deref()
    }

    /// Primary/write role pool.
    #[must_use]
    pub const fn primary(&self) -> &Pool<RuntimeConnection> {
        &self.primary
    }

    /// Optional read/replica role pool.
    #[must_use]
    pub const fn replica(&self) -> Option<&Pool<RuntimeConnection>> {
        self.replica.as_ref()
    }

    /// Pool used for read-only work.
    #[must_use]
    pub fn read(&self) -> &Pool<RuntimeConnection> {
        self.replica.as_ref().unwrap_or(&self.primary)
    }
}

/// Whether continuous `SQLite` replication (#1628) is active in this process.
///
/// Process-global on purpose. The per-connection pragma batch below is installed
/// by a `custom_setup` callback that has no access to `AutumnConfig`, and
/// replication is inherently a single-process, single-database concern — #1614's
/// `SQLite` tier is single-host and single-writer, and continuous replication
/// *depends* on exactly one component being allowed to checkpoint.
static SQLITE_REPLICATION_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The per-connection pragma batch a pooled `SQLite` connection is set up with.
///
/// Pure over its two inputs so the decision is unit-testable without a pool, a
/// database, or the process-global replication latch.
///
/// * A **read-only** target gets the non-writing pragmas only: `journal_mode =
///   WAL` and `synchronous` both write, and would fail connection setup with
///   "attempt to write a readonly database", taking the whole pool 503.
/// * When **replication is active** (#1628), `wal_autocheckpoint = 0` is added so
///   the replicator is the only component that ever checkpoints. An
///   auto-checkpoint rewrites the main database file, which would tear a base
///   snapshot mid-copy, and restarts the WAL under a new salt, which would force
///   an expensive fresh generation every 1000 pages.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
const fn sqlite_connection_pragmas(read_only: bool, replicating: bool) -> &'static str {
    if read_only {
        "PRAGMA busy_timeout = 5000; \
         PRAGMA foreign_keys = ON;"
    } else if replicating {
        "PRAGMA busy_timeout = 5000; \
         PRAGMA journal_mode = WAL; \
         PRAGMA wal_autocheckpoint = 0; \
         PRAGMA synchronous = NORMAL; \
         PRAGMA foreign_keys = ON;"
    } else {
        "PRAGMA busy_timeout = 5000; \
         PRAGMA journal_mode = WAL; \
         PRAGMA synchronous = NORMAL; \
         PRAGMA foreign_keys = ON;"
    }
}

/// Declare that continuous replication is active in this process.
///
/// Once set, every `SQLite` connection this process's pools create is given
/// `PRAGMA wal_autocheckpoint = 0`, so `SQLite` never restarts the write-ahead
/// log on its own. That matters twice over: an auto-checkpoint rewrites the
/// **main database file**, which would tear a base snapshot the replicator is
/// copying, and it restarts the WAL, which would force an expensive fresh
/// generation every 1000 pages. The replicator checkpoints deliberately
/// instead, and only once everything in the WAL is already offsite.
///
/// **Latch-only, and read per connection.** It can be turned on but never off,
/// because the pragma is installed by a `custom_setup` callback that runs each
/// time deadpool *creates* a connection — lazily, and again after a recycle —
/// for the whole life of the process. A second app booted in the same process
/// (the workspace's own consolidated test binary is exactly that) must not be
/// able to clear the flag out from under a replicating app's pool and let its
/// connections auto-checkpoint behind the replicator's back.
pub(crate) fn set_sqlite_replication_active() {
    SQLITE_REPLICATION_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether [`set_sqlite_replication_active`] has been called in this process.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) fn sqlite_replication_active() -> bool {
    SQLITE_REPLICATION_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

fn build_pool(
    url: &str,
    pool_size: usize,
    connect_timeout_secs: u64,
) -> Result<Pool<RuntimeConnection>, PoolError> {
    // Under the `sqlite` feature `RuntimeConnection` is a SQLite connection, so
    // the pool must be built over `SyncConnectionWrapper<SqliteConnection>`
    // rather than the Postgres manager below. Route to the dedicated SQLite
    // builder (PR2, issue #1614). This block is the whole function body under
    // the feature (the Postgres arms below are cfg'd out), so it is the tail
    // expression — no `return` needed.
    #[cfg(feature = "sqlite")]
    {
        build_sqlite_pool(url, pool_size, connect_timeout_secs)
    }

    // Default (Postgres) build. A SQLite target is recognized here but its
    // runtime pool only exists in a `--features sqlite` build — the pool below
    // is a Postgres pool (`Pool<AsyncPgConnection>`). Refuse at pool-build
    // (boot) time with an actionable message so a SQLite misconfiguration fails
    // fast rather than reaching a confusing first-query failure. A Postgres
    // target skips this branch entirely.
    #[cfg(not(feature = "sqlite"))]
    if crate::config::DatabaseBackend::detect(url) == Some(crate::config::DatabaseBackend::Sqlite) {
        return Err(PoolError::UnsupportedBackend(format!(
            "SQLite is a recognized database backend but its runtime pool is only available in \
             a build of autumn-web compiled with `--features sqlite`; this is a default \
             (Postgres) build (target: {target:?})",
            target = crate::db_url::redact_target(url)
        )));
    }

    #[cfg(not(feature = "sqlite"))]
    {
        let timeout = Duration::from_secs(connect_timeout_secs);
        // When the URL's `sslmode` asks for TLS, plug a rustls-backed connector
        // into the pool via a custom setup callback — diesel-async's default
        // establish path hardcodes `NoTls`, which cannot satisfy
        // `sslmode=require` at all. `sslmode` absent/`disable`/`prefer` keeps the
        // default (NoTls) path, so existing configurations behave exactly as
        // before. See [`tls`] for the full posture table.
        let manager = match tls::TlsPosture::from_database_url(url) {
            tls::TlsPosture::Off => AsyncDieselConnectionManager::<AsyncPgConnection>::new(url),
            posture => {
                let mut config =
                    diesel_async::pooled_connection::ManagerConfig::<AsyncPgConnection>::default();
                config.custom_setup = tls::setup_callback(posture);
                AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(url, config)
            }
        };
        Ok(Pool::builder(manager)
            .max_size(pool_size.max(1))
            .wait_timeout(Some(timeout))
            .create_timeout(Some(timeout))
            .runtime(deadpool::Runtime::Tokio1)
            .build()?)
    }
}

/// Normalize a configured `SQLite` target into the filename token diesel's
/// `SqliteConnection` understands.
///
/// diesel's `SQLite` backend passes the string straight to `sqlite3_open`, which
/// understands a filesystem path, a `file:` URI, or the special `:memory:`
/// token — but **not** a `sqlite:` URL scheme. Strip the recognized `SQLite` URL
/// spellings down to that: `sqlite::memory:`, `sqlite://:memory:`, and an empty
/// `sqlite://` all become an in-memory database; `sqlite:///path` /
/// `sqlite://path` / `sqlite:path` reduce to their path; a `file:` URI passes
/// through unchanged.
///
/// A bare path passes through this function unchanged too, but no configured
/// target reaches it that way: [`build_sqlite_pool`] refuses anything
/// [`DatabaseBackend::detect`](crate::config::DatabaseBackend::detect) does not
/// classify as `SQLite`, and a bare path is deliberately unclassified. The two
/// scheme-less in-memory spellings (`:memory:` and the empty string) are the
/// exception the pool admits explicitly.
#[cfg(feature = "sqlite")]
fn normalize_sqlite_target(url: &str) -> String {
    if url.starts_with("file:") {
        return url.to_owned();
    }
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    if rest.is_empty() || rest == ":memory:" {
        return String::from(":memory:");
    }
    rest.to_owned()
}

/// Whether a normalized `SQLite` target names a **private** in-memory database
/// (each such connection is its own private database, so the pool must be
/// single-slot to stay consistent — see [`build_sqlite_pool`]).
///
/// Covers every in-memory spelling `SQLite` accepts through this pool: the bare
/// `:memory:` token, the `file:` URI form `file::memory:` (with or without a
/// query string — addresses Codex P1: the multi-slot default would otherwise
/// hand out connections that each see a different, empty in-memory database),
/// and any `file:` URI that asks for `mode=memory`.
///
/// A **shared-cache** in-memory database (`cache=shared`) is the deliberate
/// exception: it IS shareable across the pool's connections within one process,
/// so it must NOT be forced single-slot and returns `false` here.
#[cfg(feature = "sqlite")]
fn sqlite_target_is_memory(target: &str) -> bool {
    if target.contains("cache=shared") {
        return false;
    }
    target == ":memory:"
        || target == "file::memory:"
        || target.starts_with("file::memory:?")
        || target.contains("mode=memory")
}

/// Whether a `SQLite` database URL (any accepted spelling) resolves to **any**
/// in-memory target — the private spellings (`sqlite::memory:` / `:memory:` /
/// `file::memory:`) AND the shared-cache in-memory form
/// (`file::memory:?cache=shared`, `file:app?mode=memory&cache=shared`).
///
/// This is deliberately broader than [`sqlite_target_is_memory`] (the pool
/// *sizing* predicate): it does NOT exempt `cache=shared`. It is the predicate
/// the startup-migration reject uses, because **no** in-memory target — private
/// or shared-cache — can retain a registered migration for the runtime pool. The
/// migration runs on a transient synchronous connection; `SQLite` destroys a
/// shared in-memory database the moment its *last* connection closes, and the
/// runtime deadpool is created lazily (it may not have checked out a connection
/// yet), so the pool's first checkout opens a fresh, empty in-memory database and
/// every DB-backed request then 500s with "no such table". Only a **file-backed**
/// database survives the migration connection closing, so that is the sole
/// supported remedy.
///
/// Pool *sizing* deliberately keeps using [`sqlite_target_is_memory`] instead:
/// a shared-cache in-memory database IS shareable across the pool's connections
/// within one live process, so it must NOT be forced single-slot. The two
/// predicates answer different questions ("share across the pool?" vs. "survive
/// the migration connection closing?") and must not be conflated.
#[cfg(feature = "sqlite")]
pub(crate) fn sqlite_target_is_any_in_memory(url: &str) -> bool {
    let target = normalize_sqlite_target(url);
    target == ":memory:"
        || target == "file::memory:"
        || target.starts_with("file::memory:?")
        || target.contains("mode=memory")
}

/// Whether a normalized `SQLite` target names a **read-only** database via its
/// URI query string (`mode=ro`, or `immutable=1`/`immutable=true`).
///
/// A read-only target rejects any write, so the per-connection setup batch must
/// skip the write-affecting pragmas (`journal_mode = WAL`) that would otherwise
/// fail with "attempt to write a readonly database" and take the whole pool 503
/// (see [`build_sqlite_pool`]). The query string is parsed key-by-key
/// (case-insensitive keys and values) rather than substring-matched, so an
/// unrelated value that merely contains `ro` never trips it.
///
/// A plain file path, an in-memory target (`mode=memory`, which is not
/// read-only), and a `cache=shared` target all return `false`.
#[cfg(feature = "sqlite")]
fn sqlite_target_is_read_only(target: &str) -> bool {
    let Some((_, query)) = target.split_once('?') else {
        return false;
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("mode") && value.eq_ignore_ascii_case("ro") {
            return true;
        }
        if key.eq_ignore_ascii_case("immutable")
            && (value == "1" || value.eq_ignore_ascii_case("true"))
        {
            return true;
        }
    }
    false
}

/// Build a deadpool pool over `SyncConnectionWrapper<SqliteConnection>` for a
/// `SQLite` target (issue #1614, PR2).
///
/// `SyncConnectionWrapper` runs synchronous diesel `SqliteConnection` calls on
/// Tokio's blocking pool, so this integrates with the async runtime like the
/// Postgres path. Pool sizing is deliberately conservative: `SQLite` is
/// single-writer, so a large pool only multiplies `SQLITE_BUSY` contention, and
/// an in-memory database is **private per connection** — a multi-slot pool over
/// `:memory:` would hand out connections that each see a different, empty
/// database (so migrations applied on one would be invisible on another).
/// In-memory targets are therefore forced to a single slot; file targets
/// respect the configured size (still small by convention).
#[cfg(feature = "sqlite")]
fn build_sqlite_pool(
    url: &str,
    pool_size: usize,
    connect_timeout_secs: u64,
) -> Result<Pool<RuntimeConnection>, PoolError> {
    // Under the `sqlite` feature the runtime targets SQLite, so the URL must
    // actually NAME a SQLite target. Everything past this point treats the
    // string as a filename: `normalize_sqlite_target` strips the `sqlite:` /
    // `sqlite://` schemes and passes anything else through verbatim, so an
    // unguarded target is opened as a file whose name happens to be that
    // string. Refuse anything `DatabaseBackend::detect` does not classify as
    // SQLite, with a message that names the offending target and the fix.
    //
    // Two shapes reach this guard and both used to fail open:
    //
    //   * A Postgres target — a build/config mismatch, and the message says so
    //     specifically rather than talking about schemes.
    //   * Anything `detect` returns `None` for: another backend's URL
    //     (`mysql://…`), a typo of the scheme (`sqllite:///app.db`), or a bare
    //     filesystem path (`/var/lib/app.db`), which `DatabaseBackend`
    //     deliberately does not recognize. `DatabaseConfig::validate` rejects
    //     these too, so the ordinary `AutumnConfig::load` boot path never
    //     reaches this arm — but `create_pool`, `create_topology` and
    //     `create_shard_topology` are public and a programmatically-built
    //     `DatabaseConfig` or a custom `DatabasePoolProvider` gets here without
    //     that screen. Accepting them would split the runtime from every other
    //     consumer of the same URL — `autumn doctor`, the generator's DDL
    //     mapping and `autumn migrate` all classify through `detect` and would
    //     report "no recognized backend" while this pool quietly served a
    //     database out of a junk file.
    //
    // The bare in-memory spellings are the one accepted target `detect` does
    // NOT classify (it recognizes schemes, and these carry none), so they are
    // admitted explicitly rather than by scheme: `normalize_sqlite_target` has
    // a dedicated branch mapping `""` and `:memory:` to `:memory:`
    // (`rest.is_empty() || rest == ":memory:"`), `run_pending_sqlite` names
    // `:memory:` among the spellings it accepts, and `TestApp::with_transactional_db`
    // routes whatever URL it is handed straight through `create_pool`. They also
    // cannot become a stray file, which is the failure this guard exists to
    // prevent. Refusing them would be a regression, not a tightening.
    match crate::config::DatabaseBackend::detect(url) {
        _ if crate::db_url::is_bare_in_memory_sqlite(url) => {}
        Some(crate::config::DatabaseBackend::Sqlite) => {}
        Some(crate::config::DatabaseBackend::Postgres) => {
            return Err(PoolError::UnsupportedBackend(format!(
                "this build of autumn-web targets SQLite (compiled with `--features sqlite`) but \
                 the configured database URL is a Postgres target; configure a `sqlite:` URL \
                 instead (target: {target:?})",
                target = crate::db_url::redact_target(url)
            )));
        }
        None => {
            return Err(PoolError::UnsupportedBackend(format!(
                "this build of autumn-web targets SQLite (compiled with `--features sqlite`) but \
                 the configured database URL names no recognized database backend; configure a \
                 SQLite target spelled `sqlite:<path>`, `sqlite://<path>`, or `file:<path>` — a \
                 bare filesystem path is not accepted (target: {target:?})",
                target = crate::db_url::redact_target(url)
            )));
        }
    }

    let timeout = Duration::from_secs(connect_timeout_secs);
    let target = normalize_sqlite_target(url);
    let max_size = if sqlite_target_is_memory(&target) {
        1
    } else {
        pool_size.max(1)
    };
    // SQLite starts every connection with `foreign_keys` OFF, so a bare manager would
    // hand out pooled connections that silently ignore `REFERENCES` constraints, making
    // orphan rows and referential-integrity violations possible app-wide. It also uses a
    // default busy handler that returns `SQLITE_BUSY` immediately when another pooled
    // connection holds the single writer lock, so ordinary overlapping writes on a
    // multi-slot file pool fail as 5xx instead of waiting briefly. Install both pragmas —
    // plus a deliberate WAL journal mode with `synchronous = NORMAL` for better write
    // concurrency — during every pooled connection's setup, mirroring how the sync store
    // configures its own connection (`crate::sync::store`: `busy_timeout = 5000`,
    // `journal_mode = WAL`, `synchronous = NORMAL`, `foreign_keys = ON`). `busy_timeout`
    // is set first, so everything after it queues on the timeout instead of failing on a
    // held lock; `journal_mode = WAL` is a harmless no-op for a pure `:memory:` database.
    // A `custom_setup` callback runs once per newly-created connection, the same
    // per-connection hook the Postgres path uses to install TLS.
    //
    // A read-only URI target (`mode=ro`, `immutable`, e.g.
    // `sqlite://file:/srv/reference.db?mode=ro`) is the exception: `journal_mode = WAL`
    // writes to the database — it rewrites the file header and creates the `-wal`/`-shm`
    // sidecars — so it fails with "attempt to write a readonly database", `custom_setup`
    // propagates that as a connection-setup error, and the pool could not service even
    // read-only queries (`Db` routes 503). Such targets get only the non-writing
    // per-connection pragmas (`busy_timeout`, `foreign_keys`), so a read-only pool builds
    // and serves reads. In-memory targets are not read-only and keep the full batch.
    let mut config = diesel_async::pooled_connection::ManagerConfig::<RuntimeConnection>::default();
    config.custom_setup = Box::new(|url: &str| {
        use diesel_async::{AsyncConnection as _, SimpleAsyncConnection as _};
        let url = url.to_owned();
        async move {
            let mut conn = RuntimeConnection::establish(&url).await?;
            let pragmas = sqlite_connection_pragmas(
                sqlite_target_is_read_only(&url),
                sqlite_replication_active(),
            );
            conn.batch_execute(pragmas)
                .await
                .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
            // #1910 FTS5 capability probe. Searchable repositories emit FTS5 virtual
            // tables and `bm25()` ranking, and the `AddSearch` migration's `CREATE
            // VIRTUAL TABLE ... USING fts5(...)` is the hard stop that fails loudly at
            // boot if the linked SQLite lacks FTS5. Probe it here — create and drop a
            // throwaway FTS5 table in the always-writable `temp` database, harmless on
            // read-only main targets — so the failure is a clear diagnostic naming FTS5
            // and the fix, rather than a bare "no such module: fts5" from a migration.
            // There is no silent fallback to LIKE: full-text search requires FTS5.
            if let Err(e) = conn
                .batch_execute(
                    "CREATE VIRTUAL TABLE temp.__autumn_fts5_probe USING fts5(x); \
                     DROP TABLE temp.__autumn_fts5_probe;",
                )
                .await
            {
                return Err(diesel::ConnectionError::CouldntSetupConfiguration(
                    diesel::result::Error::QueryBuilderError(
                        format!(
                            "SQLite FTS5 is not available in the linked SQLite library, but \
                             autumn-web full-text search (searchable repositories / the \
                             `--search` scaffold, issue #1910) requires it. Build with the \
                             bundled, FTS5-enabled SQLite by enabling autumn-web's `sqlite` \
                             feature (it turns on `libsqlite3-sys/bundled`, whose amalgamation \
                             defines SQLITE_ENABLE_FTS5). Underlying probe error: {e}"
                        )
                        .into(),
                    ),
                ));
            }
            Ok(conn)
        }
        .boxed()
    });
    let manager =
        AsyncDieselConnectionManager::<RuntimeConnection>::new_with_config(target, config);
    Ok(Pool::builder(manager)
        .max_size(max_size)
        .wait_timeout(Some(timeout))
        .create_timeout(Some(timeout))
        .runtime(deadpool::Runtime::Tokio1)
        .build()?)
}

/// Create a connection pool from the database configuration.
///
/// Returns `Ok(None)` if no primary database URL is configured
/// (`database.primary_url` and the legacy `database.url` are absent or `null`
/// in `autumn.toml`).
///
/// # Errors
///
/// Returns [`PoolError`] if the pool cannot be built (e.g., invalid
/// max-size configuration).
pub fn create_pool(config: &DatabaseConfig) -> Result<Option<Pool<RuntimeConnection>>, PoolError> {
    let Some(url) = config.effective_primary_url() else {
        return Ok(None);
    };

    #[cfg(feature = "sqlite")]
    reject_sqlite_statement_timeout(config.statement_timeout)?;

    let pool = build_pool(
        url,
        config.effective_primary_pool_size(),
        config.connect_timeout_secs,
    )?;

    Ok(Some(pool))
}

/// Reject a configured `database.statement_timeout` under the `SQLite` backend.
///
/// `SQLite` cannot enforce a per-statement wall-clock timeout through the async
/// connection wrapper: diesel's `SqliteConnection` exposes no
/// interrupt/progress-handler hook (nor the raw `sqlite3` handle) through
/// `SyncConnectionWrapper`, so a runaway query cannot be aborted mid-flight.
/// Rather than silently ignore a correctness guarantee we cannot honor, this
/// fails the boot fast with an actionable message (fail-closed, issue #1996).
///
/// A `None` or zero timeout (the default) asks for no guarantee and boots
/// cleanly, so ordinary `SQLite` apps are unaffected — only an operator who
/// explicitly configured a timeout we cannot meet is stopped. This is the single
/// choke point for every pool-construction entry point: the built-in
/// `create_pool`, `create_topology`, and `create_shard_topology` factories all
/// call it before the pool is built, and `setup_database` calls it once more at
/// the pool-provider dispatch boundary so a custom
/// [`DatabasePoolProvider`](crate::db::DatabasePoolProvider) — whose
/// `create_topology`/`create_shard_topology` need not route through those
/// factories — cannot bypass the guard. No configured timeout can reach a live
/// `SQLite` pool. `busy_timeout` still bounds lock waits; a real per-statement
/// timeout is tracked by #1996/#1910.
///
/// # Errors
///
/// Returns [`PoolError::UnsupportedBackend`] when `statement_timeout` is `Some`
/// and non-zero.
#[cfg(feature = "sqlite")]
pub(crate) fn reject_sqlite_statement_timeout(
    statement_timeout: Option<Duration>,
) -> Result<(), PoolError> {
    let Some(timeout) = statement_timeout.filter(|t| !t.is_zero()) else {
        return Ok(());
    };
    Err(PoolError::UnsupportedBackend(format!(
        "SQLite backend cannot enforce database.statement_timeout ({}ms): diesel's \
         SqliteConnection exposes no interrupt/progress-handler hook through the async \
         connection wrapper, so a runaway query cannot be aborted. Unset \
         database.statement_timeout for the SQLite backend (busy_timeout already bounds \
         lock waits), or run on Postgres. Tracking issue: #1996/#1910.",
        timeout.as_millis()
    )))
}

/// Reject a `SQLite` `replica_url` that cannot act as a real read replica for the
/// given `primary_url`.
///
/// Under the `sqlite` feature a configured replica cannot replicate: `SQLite` has
/// no primary/replica replication in this pool architecture. Two single-slot
/// `:memory:` pools are two *private* empty databases, and a distinct replica
/// *file* has nothing replicating into it, so read-routed queries would hit an
/// empty replica after writes and schema setup went to the primary. Reject an
/// in-memory or distinct-file replica with an actionable boot error (addresses
/// Codex P2). A replica that normalizes to the SAME file as the primary is the
/// same database — harmless — so it is allowed. `normalize_sqlite_target` is
/// reused so "same file" matches how the pool actually opens the file, and
/// `sqlite_target_is_memory` so pool sizing and this guard agree on what
/// "in-memory" means. Shared by the control-database and per-shard topologies so
/// the two rejection rules cannot drift.
#[cfg(feature = "sqlite")]
fn reject_unusable_sqlite_replica(primary_url: &str, replica_url: &str) -> Result<(), PoolError> {
    let replica_target = normalize_sqlite_target(replica_url);
    let primary_target = normalize_sqlite_target(primary_url);
    if sqlite_target_is_memory(&replica_target) || replica_target != primary_target {
        return Err(PoolError::UnsupportedBackend(format!(
            "SQLite does not support a separate read replica: replica_url {replica:?} \
             is in-memory or differs from primary_url {primary:?}. Configure only a \
             primary, or point the replica at the same database file as the primary.",
            replica = crate::db_url::redact_target(replica_url),
            primary = crate::db_url::redact_target(primary_url)
        )));
    }
    Ok(())
}

/// Create primary and optional replica pools from the database configuration.
///
/// Returns `Ok(None)` when neither `database.primary_url` nor the legacy
/// `database.url` compatibility field is configured.
///
/// # Errors
///
/// Returns [`PoolError`] if either configured role cannot be built.
pub fn create_topology(config: &DatabaseConfig) -> Result<Option<DatabaseTopology>, PoolError> {
    let Some(primary_url) = config.effective_primary_url() else {
        return Ok(None);
    };

    #[cfg(feature = "sqlite")]
    reject_sqlite_statement_timeout(config.statement_timeout)?;

    let primary = build_pool(
        primary_url,
        config.effective_primary_pool_size(),
        config.connect_timeout_secs,
    )?;

    #[cfg(feature = "sqlite")]
    if let Some(replica_url) = config.replica_url.as_deref() {
        reject_unusable_sqlite_replica(primary_url, replica_url)?;
    }

    let replica = config
        .replica_url
        .as_deref()
        .map(|url| {
            build_pool(
                url,
                config.effective_replica_pool_size(),
                config.connect_timeout_secs,
            )
        })
        .transpose()?;

    Ok(Some(DatabaseTopology::from_pools(primary, replica)))
}

/// Create one shard's primary and optional replica pools, applying the
/// shard's pool-size and timeout fallbacks to the `[database]` defaults.
///
/// # Errors
///
/// Returns [`PoolError`] if either configured role cannot be built.
pub fn create_shard_topology(
    shard: &crate::config::ShardConfig,
    defaults: &DatabaseConfig,
) -> Result<DatabaseTopology, PoolError> {
    // A SQLite `[[database.shards]]` deployment reaches this via the ungated
    // `sharding::create_shard_set` → `create_shard_topology` path (the
    // transactional test-harness variant `create_shard_set_transactional` is
    // Postgres-only), so the same fail-closed guard applies per shard. The shard
    // inherits the `[database]` `statement_timeout`.
    #[cfg(feature = "sqlite")]
    reject_sqlite_statement_timeout(defaults.statement_timeout)?;

    let primary = build_pool(
        &shard.primary_url,
        shard.effective_primary_pool_size(defaults),
        defaults.connect_timeout_secs,
    )?;

    // A per-shard `replica_url` is subject to the same SQLite-replica rule as the
    // control database: without it a SQLite `[[database.shards]]` entry with a
    // distinct or in-memory replica would boot and route reads to an unrelated,
    // empty database. Reuse the exact same helper so the two rejections cannot
    // drift (addresses Codex P2).
    #[cfg(feature = "sqlite")]
    if let Some(replica_url) = shard.replica_url.as_deref() {
        reject_unusable_sqlite_replica(&shard.primary_url, replica_url)?;
    }

    let replica = shard
        .replica_url
        .as_deref()
        .map(|url| {
            build_pool(
                url,
                shard.effective_replica_pool_size(defaults),
                defaults.connect_timeout_secs,
            )
        })
        .transpose()?;

    Ok(DatabaseTopology::from_pools(primary, replica))
}

// ── Transaction options, retry, and isolation (issue #1202) ──────────────────

/// Postgres transaction isolation level, requested per call via [`TxOptions`].
///
/// `ReadCommitted` is Postgres' default and Autumn's default; the stronger
/// levels are opt-in on [`Db::tx_with`]. See the transactions guide for what
/// each level buys and costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationLevel {
    /// Postgres default. Each statement sees rows committed before it began.
    #[default]
    ReadCommitted,
    /// A snapshot fixed at the first query; no non-repeatable or phantom reads.
    RepeatableRead,
    /// Full serializability (SSI). Transactions behave as if run one at a time;
    /// conflicts surface as `40001` and are retried by [`Db::tx_with`].
    Serializable,
}

impl IsolationLevel {
    /// The SQL fragment used in tracing (`db.isolation`).
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadCommitted => "read_committed",
            Self::RepeatableRead => "repeatable_read",
            Self::Serializable => "serializable",
        }
    }
}

/// Default number of attempts (including the first) for the retrying isolation
/// levels. Chosen to match the jobs system's default retry budget.
const DEFAULT_TX_MAX_ATTEMPTS: u32 = 5;

/// Options for [`Db::tx_with`]: isolation level, access mode, and the automatic
/// retry policy for transient serialization failures.
///
/// [`TxOptions::default`] is byte-for-byte equivalent to today's [`Db::tx`]:
/// READ COMMITTED, read-write, a single attempt, and no retry.
///
/// ```rust
/// use autumn_web::db::{TxOptions, IsolationLevel};
///
/// let opts = TxOptions::serializable().read_only().max_attempts(8);
/// assert_eq!(opts.isolation, IsolationLevel::Serializable);
/// assert!(opts.read_only);
/// assert_eq!(opts.max_attempts, 8);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct TxOptions {
    /// Requested isolation level.
    pub isolation: IsolationLevel,
    /// Run the transaction `READ ONLY`.
    pub read_only: bool,
    /// Add `DEFERRABLE` (only meaningful for `SERIALIZABLE READ ONLY`).
    pub deferrable: bool,
    /// Total attempts including the first. `1` disables retry.
    pub max_attempts: u32,
    /// Backoff before the first retry; doubles each subsequent retry.
    pub initial_backoff: Duration,
    /// Upper bound on any single backoff delay.
    pub max_backoff: Duration,
}

impl Default for TxOptions {
    fn default() -> Self {
        Self {
            isolation: IsolationLevel::ReadCommitted,
            read_only: false,
            deferrable: false,
            max_attempts: 1,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(500),
        }
    }
}

impl TxOptions {
    /// READ COMMITTED, no retry — identical to [`Db::tx`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// READ COMMITTED (the default), no retry.
    #[must_use]
    pub fn read_committed() -> Self {
        Self {
            isolation: IsolationLevel::ReadCommitted,
            ..Self::default()
        }
    }

    /// REPEATABLE READ with automatic retry (`serialization_failure` can occur
    /// at this level too).
    #[must_use]
    pub fn repeatable_read() -> Self {
        Self {
            isolation: IsolationLevel::RepeatableRead,
            max_attempts: DEFAULT_TX_MAX_ATTEMPTS,
            ..Self::default()
        }
    }

    /// SERIALIZABLE with automatic retry. This is the correctness-critical
    /// default: conflicts surface as `40001` and are retried transparently.
    #[must_use]
    pub fn serializable() -> Self {
        Self {
            isolation: IsolationLevel::Serializable,
            max_attempts: DEFAULT_TX_MAX_ATTEMPTS,
            ..Self::default()
        }
    }

    /// Set the isolation level, keeping the other options.
    #[must_use]
    pub const fn isolation(mut self, level: IsolationLevel) -> Self {
        self.isolation = level;
        self
    }

    /// Run the transaction `READ ONLY`.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Add `DEFERRABLE` (for `SERIALIZABLE READ ONLY`).
    #[must_use]
    pub const fn deferrable(mut self) -> Self {
        self.deferrable = true;
        self
    }

    /// Set the maximum number of attempts (clamped to at least 1). A value of
    /// `1` disables retry.
    #[must_use]
    pub const fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = if attempts < 1 { 1 } else { attempts };
        self
    }

    /// `max_attempts`, clamped to at least 1.
    ///
    /// The `max_attempts` field is public (struct-literal update syntax is a
    /// supported way to build a `TxOptions`), so a value of `0` can reach the
    /// struct without going through [`TxOptions::max_attempts`]'s clamp. The
    /// retry loop calls this instead of reading the field directly, so `0`
    /// always behaves like `1` (run the closure once, no retry) rather than
    /// skipping the closure entirely.
    // Only the Postgres retry loop consults this; unused in a `--features
    // sqlite` library build (still unit-tested, so keep it compiled).
    #[cfg_attr(feature = "sqlite", allow(dead_code))]
    const fn effective_max_attempts(&self) -> u32 {
        if self.max_attempts < 1 {
            1
        } else {
            self.max_attempts
        }
    }

    /// Set the backoff before the first retry.
    #[must_use]
    pub const fn initial_backoff(mut self, delay: Duration) -> Self {
        self.initial_backoff = delay;
        self
    }

    /// Set the ceiling on any single backoff delay.
    #[must_use]
    pub const fn max_backoff(mut self, delay: Duration) -> Self {
        self.max_backoff = delay;
        self
    }
}

/// Whether the retry loop should re-run the closure or return the error.
// The whole serialization-failure retry machinery is Postgres-only (see
// `Db::tx_with`); these items are unused in a `--features sqlite` library build
// but remain unit-tested, so keep them compiled and silence dead-code under the
// feature.
#[cfg_attr(feature = "sqlite", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDecision {
    Retry,
    Stop,
}

/// Decide whether a just-failed attempt should be retried.
///
/// `attempt` is the 1-indexed number of the attempt that just failed. A retry
/// happens only when the error is retryable **and** attempts remain.
#[cfg_attr(feature = "sqlite", allow(dead_code))]
const fn retry_decision(attempt: u32, max_attempts: u32, retryable: bool) -> RetryDecision {
    if retryable && attempt < max_attempts {
        RetryDecision::Retry
    } else {
        RetryDecision::Stop
    }
}

/// Deterministic capped exponential backoff base: `initial * 2^(attempt-1)`,
/// saturating on overflow and capped at `max`.
///
/// Mirrors the jobs system's backoff shape (`pg_retry_delay_ms`) plus the cap
/// idiom from the migrate startup loop. Kept jitter-free so it is unit-testable.
#[cfg_attr(feature = "sqlite", allow(dead_code))]
fn retry_backoff_base(initial: Duration, max: Duration, attempt: u32) -> Duration {
    // Cap the shift so `2^shift` never overflows and huge attempts saturate.
    let shift = attempt.saturating_sub(1).min(31);
    let multiplier = 1u32 << shift;
    initial.checked_mul(multiplier).unwrap_or(max).min(max)
}

/// [`retry_backoff_base`] with +/-20% jitter applied, never exceeding `max`.
///
/// Jitter decorrelates a thundering herd of transactions all retrying after the
/// same conflict. Delegates to [`crate::cache::jittered_ttl`] (rather than
/// re-implementing the RNG/fallback logic) so there's one jitter
/// implementation in the crate, including its `getrandom`-failure fallback to
/// `SystemTime` entropy instead of silently degrading to no jitter.
#[cfg_attr(feature = "sqlite", allow(dead_code))]
fn retry_backoff_delay(initial: Duration, max: Duration, attempt: u32) -> Duration {
    let base = retry_backoff_base(initial, max, attempt);
    crate::cache::jittered_ttl(base, 0.2).min(max)
}

/// Whether `err` wraps a Postgres serialization failure (`40001`) or deadlock
/// (`40P01`) — the two transient errors that are safe to retry by re-running
/// the whole transaction.
///
/// Classification is structural and authoritative, never based on scanning an
/// arbitrary error's displayed message: a `diesel::result::Error` anywhere in
/// `err`'s source chain (not just the top-level wrapped error — a custom `E`
/// that wraps a diesel error via `#[source]`/`#[from]` is still found) is
/// classified by its `DatabaseErrorKind`. `diesel-async`'s Postgres backend
/// maps `40001` to `SerializationFailure` directly from the real SQLSTATE (see
/// `diesel_async::pg::error_helper::from_tokio_postgres_error`) — fully
/// reliable, no message inspection involved. Any *other* kind (e.g.
/// `UniqueViolation`) is trusted as non-retryable outright — never
/// string-matched, so a constraint name or key value that happens to contain
/// `"40001"` cannot misclassify it.
///
/// `40P01` (deadlock) has no dedicated `DatabaseErrorKind` and always surfaces
/// as `Unknown`. Worse: the wrapper type diesel-async uses to carry Postgres's
/// error fields for an `Unknown`-kind error (`PostgresDbErrorWrapper`, private
/// to diesel-async) implements only `DatabaseErrorInformation`, not
/// `std::error::Error` — so `source_chain_has_sqlstate`'s downcast walk can
/// **never** reach the real SQLSTATE for it; there is no structural path to a
/// deadlock's code at all through this crate boundary. The only signal
/// available is the message, so this checks it — but with an **exact**
/// match, not a substring: Postgres's deadlock detector always raises the
/// primary message as precisely `"deadlock detected"` with nothing else (the
/// context lives in DETAIL/HINT, not here), so an app's own `RAISE EXCEPTION`
/// would have to reproduce that exact string as its *entire* message to
/// collide — unlike a substring check, which a longer business message
/// merely mentioning the phrase would already trip.
///
/// When no `diesel::result::Error` is found anywhere in the chain, this checks
/// for a raw `tokio_postgres` SQLSTATE directly (a custom `E` that bypasses
/// diesel). If neither is found, the error is **not** retried — there is
/// deliberately no generic message-substring fallback beyond the one exact
/// match above: retrying based on bare text risks misclassifying an unrelated
/// domain/validation error as a transient conflict, silently re-running a
/// non-idempotent closure and delaying the response.
#[cfg_attr(feature = "sqlite", allow(dead_code))]
pub fn is_retryable_txn_error(err: &AutumnError) -> bool {
    use tokio_postgres::error::SqlState;

    fn is_retryable_sqlstate(state: &SqlState) -> bool {
        *state == SqlState::T_R_SERIALIZATION_FAILURE || *state == SqlState::T_R_DEADLOCK_DETECTED
    }

    if let Some(diesel_err) = err.downcast_chain_ref::<diesel::result::Error>() {
        return match diesel_err {
            // Fast path: diesel-async maps 40001 to SerializationFailure from
            // the real SQLSTATE. Fully structural, no message involved.
            diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::SerializationFailure,
                _,
            ) => true,
            // 40P01 (deadlock) has no dedicated kind and surfaces as Unknown.
            // The chain walk is kept for forward-compatibility (a future
            // diesel/diesel-async release, or another backend, could start
            // exposing the real SQLSTATE through the source chain) but is
            // currently unreachable for diesel-async's own Postgres backend
            // — see the doc comment above. The message check that follows is
            // therefore the only thing that actually fires deadlock retries
            // today, hence the exact (not substring) match.
            diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::Unknown,
                info,
            ) => {
                source_chain_has_sqlstate(diesel_err, is_retryable_sqlstate)
                    || info
                        .message()
                        .trim()
                        .eq_ignore_ascii_case("deadlock detected")
            }
            // Any other kind (UniqueViolation, NotNullViolation, ...) is
            // authoritatively non-retryable — no string matching against its
            // message/constraint text.
            _ => source_chain_has_sqlstate(diesel_err, is_retryable_sqlstate),
        };
    }

    // No `diesel::result::Error` anywhere in the chain — check for a raw
    // `tokio_postgres` SQLSTATE directly. No text-based fallback beyond this;
    // see the doc comment above for why.
    err.downcast_chain_ref::<tokio_postgres::Error>()
        .and_then(tokio_postgres::Error::code)
        .is_some_and(is_retryable_sqlstate)
        || err
            .downcast_chain_ref::<tokio_postgres::error::DbError>()
            .is_some_and(|db| is_retryable_sqlstate(db.code()))
}

/// Run `f` inside a transaction on `conn`, adapting the `ScopedBoxFuture`
/// callback shape used throughout Autumn's generated code to the
/// `AsyncFnOnce` callback [`diesel_async::AsyncConnection::transaction`]
/// expects since diesel-async 0.9.
///
/// This is a runtime support function for code generated by Autumn proc
/// macros. It is semver-exempt; do not call it directly.
///
/// # Errors
///
/// Returns the error from `f`, or a `diesel::result::Error` from starting,
/// committing, or rolling back the transaction.
#[doc(hidden)]
pub async fn scoped_transaction<'a, T, E, C, F>(conn: &'a mut C, f: F) -> Result<T, E>
where
    C: diesel_async::AsyncConnection + Send,
    T: Send + 'a,
    E: From<diesel::result::Error> + Send + 'a,
    F: for<'r> FnOnce(&'r mut C) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
        + Send
        + 'a,
{
    // Mirrors the default body of `TransactionManager::transaction` in
    // diesel-async 0.9, but drives the boxed callback future directly instead
    // of going through the `AsyncFnOnce` bounds (which reject the boxed
    // `ScopedBoxFuture` callback shape).
    use diesel_async::TransactionManager as _;

    C::TransactionManager::begin_transaction(conn).await?;
    match f(&mut *conn).await {
        Ok(value) => {
            C::TransactionManager::commit_transaction(conn).await?;
            Ok(value)
        }
        Err(user_error) => match C::TransactionManager::rollback_transaction(conn).await {
            // A broken transaction manager means the rollback error is a
            // consequence of the original error; surface the original.
            Ok(()) | Err(diesel::result::Error::BrokenTransactionManager) => Err(user_error),
            Err(rollback_error) => Err(rollback_error.into()),
        },
    }
}

/// Run a write read-modify-write closure inside a transaction that takes the
/// `SQLite` write lock up front (`BEGIN IMMEDIATE`).
///
/// On Postgres this delegates to [`scoped_transaction`] unchanged. On `SQLite` it
/// begins the transaction with `BEGIN IMMEDIATE` before running `f`, so a
/// concurrent writer queues on the connection's `busy_timeout` instead of
/// failing its deferred read→write snapshot upgrade with `SQLITE_BUSY_SNAPSHOT`
/// (which bypasses the busy handler). It is the transaction primitive for
/// generated write-RMW paths (`with_lock`, `update`, `delete_by_id`,
/// `find_or_create_by`); read-only transactions, [`Db::tx`], and [`savepoint`]
/// deliberately stay on the deferred [`scoped_transaction`] so read-only user
/// transactions keep their read concurrency.
///
/// This is a runtime support function for code generated by Autumn proc macros.
/// It is semver-exempt; do not call it directly.
///
/// # `SQLite` nesting parity
///
/// The `BEGIN IMMEDIATE` is issued **through** diesel's `AnsiTransactionManager`
/// (`begin_transaction_sql`) rather than as a raw statement, so the manager's
/// depth counter is synchronized (0 → 1). A nested [`savepoint`] or a
/// `TransactionManager`-driven `.transaction()` inside `f` therefore emits a
/// `SAVEPOINT` (matching Postgres) instead of a raw `BEGIN` that would fail with
/// "cannot start a transaction within a transaction". Commit and rollback also
/// route through the manager, so its depth/status bookkeeping stays correct. If
/// a `COMMIT` fails, the manager leaves the connection marked in-transaction,
/// so deadpool sees a broken transaction manager and discards the connection
/// rather than recycling it with an open write transaction.
///
/// # Errors
///
/// Returns the error from `f`, or a `diesel::result::Error` from starting or
/// committing the transaction. On `SQLite` a panic inside `f` rolls the
/// transaction back (through the transaction manager) before resuming the
/// unwind, so the pooled connection is never recycled with an open write
/// transaction.
/// Run `f` on `conn`, wrapped in a [`scoped_immediate_transaction`] only when
/// `wrap` is true.
///
/// This exists for the counter-cache write paths (#1325). Several no-hooks
/// mutation paths are deliberately transaction-free: they issue exactly one
/// statement, so a `BEGIN`/`COMMIT` round trip would be pure overhead. A counter
/// cache makes that one statement two, which must commit or roll back together
/// — so those paths need a transaction if, and only if, the model actually
/// declares a counter cache.
///
/// `#[model]` and `#[repository]` are separate proc-macro invocations, so the
/// repository macro cannot see the model's `#[belongs_to(..., counter_cache)]`
/// attributes; it learns about them from the `const HAS_COUNTER_CACHES` the
/// model emits. That const is what callers pass as `wrap`.
///
/// Previously the macro branched on that const and emitted the **whole mutation
/// body twice** — once inside a transaction closure, once bare — at seven sites
/// per repository. That is free at runtime (the const folds) but not at compile
/// time: both copies are parsed, name-resolved, type-checked and borrow-checked
/// on every build, and it accounted for ~13.5% of every generated repository.
/// Routing both cases through one closure here keeps the runtime behaviour
/// exactly as it was — no transaction is opened when `wrap` is false — while the
/// body is emitted once.
///
/// This is a runtime support function for code generated by Autumn proc macros.
/// It is semver-exempt; do not call it directly.
///
/// # Errors
///
/// Returns the error from `f`, or a `diesel::result::Error` from starting or
/// committing the transaction when one is opened.
#[doc(hidden)]
pub async fn maybe_immediate_transaction<'a, T, E, F>(
    conn: &'a mut RuntimeConnection,
    wrap: bool,
    f: F,
) -> Result<T, E>
where
    T: Send + 'a,
    E: From<diesel::result::Error> + Send + 'a,
    F: for<'r> FnOnce(
            &'r mut RuntimeConnection,
        ) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
        + Send
        + 'a,
{
    if wrap {
        scoped_immediate_transaction(conn, f).await
    } else {
        f(conn).await
    }
}

#[doc(hidden)]
pub async fn scoped_immediate_transaction<'a, T, E, F>(
    conn: &'a mut RuntimeConnection,
    f: F,
) -> Result<T, E>
where
    T: Send + 'a,
    E: From<diesel::result::Error> + Send + 'a,
    F: for<'r> FnOnce(
            &'r mut RuntimeConnection,
        ) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
        + Send
        + 'a,
{
    crate::backend_select! {
        pg => { scoped_transaction(conn, f).await },
        sqlite => {{
            // Begin the immediate transaction THROUGH the transaction manager so
            // its depth counter is kept in sync (depth 0 → 1). Reaching the inner
            // `SqliteConnection` for `begin_transaction_sql` requires the concrete
            // `SyncConnectionWrapper<SqliteConnection>` (`spawn_blocking`), which
            // is why this helper takes `&mut RuntimeConnection` rather than a
            // generic connection.
            use diesel::connection::{AnsiTransactionManager, TransactionManager};

            // Take the write lock up front. A concurrent writer queues here on
            // `busy_timeout` instead of failing a deferred snapshot upgrade. With
            // the depth counter at 1, a nested transaction/savepoint inside `f`
            // becomes a SAVEPOINT (Postgres parity) instead of a raw nested BEGIN.
            conn.spawn_blocking(|inner| {
                AnsiTransactionManager::begin_transaction_sql(inner, "BEGIN IMMEDIATE")
            })
            .await
            .map_err(E::from)?;

            // `catch_unwind` is mandatory: a panic that unwound without a rollback
            // would leave deadpool free to recycle this connection with an open,
            // uncommitted write transaction.
            let outcome = AssertUnwindSafe(f(&mut *conn)).catch_unwind().await;
            match outcome {
                Ok(Ok(value)) => {
                    // Commit through the manager. At depth 1 it runs `COMMIT`. If
                    // that fails (e.g. a deferred-FK violation) the manager leaves
                    // the connection marked in-transaction, so its
                    // `is_broken_transaction_manager` reports broken and deadpool
                    // discards the connection on return rather than recycling it
                    // with an open write transaction — the pool never hands back a
                    // dirty connection.
                    conn.spawn_blocking(|inner| {
                        <AnsiTransactionManager as TransactionManager<
                            diesel::SqliteConnection,
                        >>::commit_transaction(inner)
                    })
                    .await
                    .map_err(E::from)?;
                    Ok(value)
                }
                Ok(Err(user_error)) => {
                    if let Err(e) = conn
                        .spawn_blocking(|inner| {
                            <AnsiTransactionManager as TransactionManager<
                                diesel::SqliteConnection,
                            >>::rollback_transaction(inner)
                        })
                        .await
                    {
                        tracing::warn!(
                            "failed to roll back immediate transaction after error: {e}"
                        );
                    }
                    Err(user_error)
                }
                Err(panic) => {
                    if let Err(e) = conn
                        .spawn_blocking(|inner| {
                            <AnsiTransactionManager as TransactionManager<
                                diesel::SqliteConnection,
                            >>::rollback_transaction(inner)
                        })
                        .await
                    {
                        tracing::error!(
                            "failed to roll back immediate transaction during panic: {e}"
                        );
                    }
                    std::panic::resume_unwind(panic);
                }
            }
        }},
    }
}

/// Run `f` inside a Postgres `SAVEPOINT` on a connection already inside a
/// transaction.
///
/// Pass the `conn` handed to a [`Db::tx`] / [`Db::tx_with`] closure. The
/// savepoint is released when `f` returns `Ok`, or rolled back (`ROLLBACK TO
/// SAVEPOINT`) when it returns `Err` — leaving the surrounding transaction
/// intact.
///
/// This is the supported way to get a nested, partially-rollbackable unit of
/// work: `Db::tx` itself cannot be re-entered on the same connection (its
/// closure receives `&mut PooledConnection`, not `&mut Db`), so a same-connection
/// savepoint can only live here, inside the closure.
///
/// # Caveat
///
/// After-commit callbacks registered inside the savepoint via
/// [`register_after_commit`] fire when the **outer** transaction commits,
/// regardless of whether this savepoint rolled back — the callback registry is
/// transaction-scoped, not savepoint-scoped.
///
/// # Errors
///
/// Returns the error from `f`, or a `diesel::result::Error` if the savepoint
/// itself cannot be established or released.
pub async fn savepoint<'a, C, T, E, F>(conn: &'a mut C, f: F) -> Result<T, E>
where
    // Generic over the connection so it works with the `&mut PooledConnection`
    // handed to a `tx` closure and the `&mut AsyncPgConnection` handed to a
    // `tx_with` closure alike.
    C: diesel_async::AsyncConnection + Send,
    T: Send + 'a,
    E: From<diesel::result::Error> + Send + 'a,
    F: for<'r> FnOnce(&'r mut C) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
        + Send
        + 'a,
{
    // `scoped_transaction` drives `C::TransactionManager` exactly like
    // `conn.transaction` does. diesel-async issues SAVEPOINT (not BEGIN)
    // because `conn` is already in a transaction, and RELEASE / ROLLBACK TO on
    // Ok / Err respectively.
    scoped_transaction(conn, f).await
}

// ── Db extractor ─────────────────────────────────────────────

/// Connection type managed by the deadpool pool.
pub type PooledConnection = diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>;

struct TxDepthGuard<'a> {
    depth: &'a mut usize,
    poisoned: &'a mut bool,
    disarmed: bool,
}

impl Drop for TxDepthGuard<'_> {
    fn drop(&mut self) {
        *self.depth -= 1;
        if !self.disarmed {
            *self.poisoned = true;
        }
    }
}

/// Async database connection extractor.
///
/// Declare `db: Db` in a handler signature to get a pooled database
/// connection. The connection is returned to the pool when `Db` is dropped
/// at the end of the request.
///
/// `Db` implements [`Deref`](std::ops::Deref) and
/// [`DerefMut`](std::ops::DerefMut) to [`RuntimeConnection`], so you can use it
/// directly with Diesel query methods. That alias is
/// `diesel_async::AsyncPgConnection` by default and a `SQLite` connection under
/// the crate's `sqlite` feature — one extractor, two backends.
///
/// If no database is configured (i.e., `database.primary_url` and legacy
/// `database.url` are absent),
/// requests that use `Db` will receive a `503 Service Unavailable`
/// response.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/ping-db")]
/// async fn ping_db(db: Db) -> AutumnResult<&'static str> {
///     // `db` dereferences to the active RuntimeConnection
///     Ok("database is reachable")
/// }
/// ```
/// Extension/extractor struct for route-level statement timeout override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementTimeout(pub std::time::Duration);

pub struct Db {
    conn: PooledConnection,
    /// Span covering the full checkout-to-release window. Dropped when
    /// `Db` is dropped at the end of the request, so span duration
    /// reflects real connection hold time rather than just `pool.get()`
    /// latency. Exposed via [`Db::span`] so handlers can attach
    /// per-query spans as children with
    /// [`tracing::Instrument::instrument`].
    span: tracing::Span,
    tx_depth: usize,
    tx_poisoned: bool,
    route_key: Option<String>,
    metrics: Option<crate::middleware::MetricsCollector>,
    slow_query_threshold: std::time::Duration,
    /// Checkout instant on the injected clock's monotonic timeline; `Drop`
    /// subtracts it from a fresh reading to get the connection-hold duration.
    start_time: crate::time::MonotonicInstant,
    /// The app's injected clock, kept so `Drop` can take the closing reading
    /// from the same timeline `start_time` came from.
    clock: std::sync::Arc<dyn crate::time::ClockSource>,
    is_test_tx: bool,
}

impl Db {
    /// Connection-scoped span. Instrument a query future with this to
    /// emit a child span tagged under the connection checkout window.
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use tracing::Instrument as _;
    ///
    /// # async fn example(mut db: Db) -> AutumnResult<()> {
    /// let span = db.span().clone();
    /// // run a Diesel query here, e.g. users::table.load(&mut *db)
    /// async {
    ///     // ... diesel_async query ...
    ///     Ok::<_, AutumnError>(())
    /// }
    /// .instrument(span)
    /// .await
    /// # }
    /// ```
    #[must_use]
    pub const fn span(&self) -> &tracing::Span {
        &self.span
    }

    /// Run an async closure inside a database transaction at the default
    /// isolation level (READ COMMITTED).
    ///
    /// Commits when the closure returns `Ok(_)`, rolls back when it returns
    /// `Err(_)`. For a stronger isolation level and/or automatic
    /// serialization-failure retry, use [`Db::tx_with`].
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] when:
    ///
    /// - the underlying transaction returns an error,
    /// - the closure returns an error that converts into `AutumnError`,
    /// - this `Db` is already inside a transaction,
    /// - this `Db` has been poisoned by a previously cancelled/dropped
    ///   transaction future.
    ///
    /// # Panics
    ///
    /// Panics if the internal after-commit registry mutex is poisoned (only
    /// possible if a previous thread holding the lock panicked).
    pub async fn tx<'a, T, E, F>(&'a mut self, f: F) -> Result<T, crate::error::AutumnError>
    where
        T: Send + 'a,
        E: From<diesel::result::Error> + Send + Sync + 'a,
        crate::error::AutumnError: From<E>,
        F: for<'r> FnOnce(
                &'r mut PooledConnection,
            ) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
            + Send
            + 'a,
    {
        if self.tx_poisoned {
            return Err(crate::error::AutumnError::service_unavailable_msg(
                "Database connection is in an invalid transaction state",
            ));
        }
        if self.tx_depth > 0 {
            return Err(crate::error::AutumnError::bad_request_msg(
                NESTED_TX_MESSAGE,
            ));
        }
        reject_ambient_after_commit_registry_for_tx()?;
        self.tx_depth += 1;
        let mut guard = TxDepthGuard {
            depth: &mut self.tx_depth,
            poisoned: &mut self.tx_poisoned,
            disarmed: false,
        };

        // Each tx gets its own callback registry shared with the task-local so
        // that code running inside the closure (jobs, mailer, hooks) can push
        // callbacks without having access to `Db` directly. The `Arc` lets us
        // read the registry after the `scope` future completes.
        let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

        // NOTE: `tx` keeps its own body rather than delegating to `tx_with`
        // because the two hand out different connection types. `transaction()`
        // runs on the pooled `Object` (closure sees `&mut PooledConnection`),
        // whereas `tx_with` must use `build_transaction()` — only available on
        // `AsyncPgConnection` — so its closure sees `&mut AsyncPgConnection`.
        // `scoped_transaction` adapts the public `ScopedBoxFuture` callback
        // shape to the transaction API diesel-async 0.9 expects.
        let result = AFTER_COMMIT_REGISTRY
            .scope(
                registry.clone(),
                scoped_transaction::<T, E, _, _>(&mut self.conn, f),
            )
            .await
            .map_err(Into::into);

        guard.disarmed = true;

        // On commit: spawn the registered callbacks outside the transaction
        // connection, but await them sequentially inside that task so callback
        // dependencies observe registration order.
        // Errors are counted and logged; they do NOT affect the committed tx.
        // In transactional tests (outer transaction is rolled back), we suppress
        // spawning these callbacks to prevent observing uncommitted side effects.
        if result.is_ok() {
            let callbacks: Vec<CommitCallback> = {
                let mut reg = registry.lock().expect("registry lock");
                std::mem::take(&mut *reg)
            };

            if !callbacks.is_empty() && !self.is_test_tx {
                let _ = spawn_committed_after_commit_callbacks(callbacks);
            }
        }

        result
    }

    /// Run an async closure inside a database transaction with explicit
    /// [`TxOptions`] — isolation level, read-only/deferrable mode, and automatic
    /// retry of transient serialization failures (`40001`) and deadlocks
    /// (`40P01`).
    ///
    /// Commits when the closure returns `Ok(_)`, rolls back when it returns
    /// `Err(_)`. On a retryable failure with attempts remaining, the whole
    /// closure is re-run after a capped exponential backoff with jitter.
    ///
    /// # The closure must be re-runnable
    ///
    /// **Because the closure can run more than once, it must be free of
    /// side effects that are not themselves transactional (or must be
    /// idempotent).** Database work is rolled back between attempts, and
    /// after-commit callbacks from failed attempts are discarded — but any
    /// non-database side effect in the closure body (logging aside — external
    /// API calls, channel sends, in-memory mutation) will re-execute on each
    /// retry. Keep such effects out of the closure, or gate them on the final
    /// success.
    ///
    /// # Observability
    ///
    /// The transaction runs under a `db.transaction` span carrying
    /// `db.isolation` and the final `db.tx.attempts` count. Each retry also
    /// increments [`TX_RETRIES_TOTAL`]; an exhausted retry budget increments
    /// [`TX_RETRY_EXHAUSTED_TOTAL`].
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] under the same conditions as [`Db::tx`]. A
    /// non-retryable error is returned immediately; when the retry budget is
    /// exhausted, the **final** underlying error is returned (never swallowed).
    ///
    /// Under the `sqlite` feature there is no transaction builder that can
    /// enforce `READ ONLY`, so a request carrying [`TxOptions::read_only`]
    /// returns an unsupported-options error **before the closure runs** rather
    /// than silently executing a writable transaction (which would let writes
    /// commit under a read-only contract). A normal read-write transaction is
    /// unaffected. The Postgres path enforces read-only via the transaction
    /// builder and never rejects.
    ///
    /// # Panics
    ///
    /// Panics if the internal after-commit registry mutex is poisoned (only
    /// possible if a previous thread holding the lock panicked).
    #[allow(clippy::too_many_lines)]
    // The Postgres path re-borrows `f` per retry attempt (`&mut f`); the SQLite
    // path consumes it once, so `mut` is unused there.
    #[cfg_attr(feature = "sqlite", allow(unused_mut))]
    pub async fn tx_with<'a, T, E, F>(
        &'a mut self,
        opts: TxOptions,
        mut f: F,
    ) -> Result<T, crate::error::AutumnError>
    where
        T: Send + 'a,
        E: From<diesel::result::Error> + Send + Sync + 'a,
        crate::error::AutumnError: From<E>,
        // The closure receives `&mut RuntimeConnection` (not `&mut PooledConnection`
        // as `tx` does): on Postgres, isolation levels require `build_transaction()`,
        // which is inherent on `AsyncPgConnection` (the default `RuntimeConnection`).
        // Diesel query methods work on either backend. Under the `sqlite` feature
        // `RuntimeConnection` is a SQLite connection and the isolation/retry path
        // below degrades to a single plain transaction.
        F: for<'r> FnMut(
                &'r mut RuntimeConnection,
            ) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
            + Send
            + 'a,
    {
        if self.tx_poisoned {
            return Err(crate::error::AutumnError::service_unavailable_msg(
                "Database connection is in an invalid transaction state",
            ));
        }
        if self.tx_depth > 0 {
            return Err(crate::error::AutumnError::bad_request_msg(
                NESTED_TX_MESSAGE,
            ));
        }
        reject_ambient_after_commit_registry_for_tx()?;

        // Under the SQLite runtime there is no transaction builder that can enforce
        // `READ ONLY` semantics — the `sqlite` arm below runs a single plain, writable
        // transaction. Silently honoring a caller's `TxOptions::read_only()` by running a
        // writable transaction anyway would let writes succeed and commit under a
        // contract that promised none, a safety regression for any code relying on a
        // read-only transaction to prevent mutation. So reject the request up front,
        // before the closure can run, rather than pretend to honor it. Real `query_only`
        // enforcement is avoided deliberately: deadpool's `custom_setup` runs on create
        // only, so a leaked `PRAGMA query_only = ON` would poison a pooled connection for
        // its lifetime. The Postgres path enforces read-only via the transaction
        // builder's `read_only()` and is unaffected.
        #[cfg(feature = "sqlite")]
        if opts.read_only {
            return Err(crate::error::AutumnError::bad_request_msg(
                "SQLite runtime does not support read-only transactions \
                 (TxOptions::read_only); this build cannot enforce read-only \
                 semantics on SQLite. Remove the read_only option or run on \
                 Postgres.",
            ));
        }

        self.tx_depth += 1;
        let mut guard = TxDepthGuard {
            depth: &mut self.tx_depth,
            poisoned: &mut self.tx_poisoned,
            disarmed: false,
        };

        let span = tracing::info_span!(
            "db.transaction",
            db.system = "postgresql",
            db.isolation = opts.isolation.as_str(),
            db.tx.attempts = tracing::field::Empty,
        );

        if self.is_test_tx {
            // Under a transactional `TestApp` the connection is already inside the test
            // harness's outer transaction (`begin_test_transaction`), so issuing a
            // literal `BEGIN`/`SET TRANSACTION ISOLATION LEVEL` via `build_transaction()`
            // here would be invalid: Postgres rejects `SET TRANSACTION ISOLATION LEVEL`
            // inside a subtransaction, and the retry loop's per-attempt `&mut f` re-borrow
            // does not type-check against the plain `transaction()` method's lifetime
            // shape, whose bound is not scoped to a single call. So nest via `SAVEPOINT`
            // instead, exactly like `Db::tx`, running the closure once: the requested
            // isolation, read-only, deferrable, and retry options are inherited from — or
            // meaningless nested inside — the outer test transaction, so there is nothing
            // to retry against a single test-harness connection.
            let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));
            let conn: &mut RuntimeConnection = &mut self.conn;
            let result = AFTER_COMMIT_REGISTRY
                .scope(registry, scoped_transaction::<T, E, _, _>(conn, f))
                .instrument(span.clone())
                .await
                .map_err(Into::into);

            span.record("db.tx.attempts", 1u32);
            guard.disarmed = true;
            // `is_test_tx` always suppresses after-commit spawning (matching
            // `Db::tx`), so the registry is simply dropped without draining.
            return result;
        }

        // SQLite has no `SET TRANSACTION ISOLATION LEVEL` / read-only /
        // deferrable transaction builder and no serialization-failure retry
        // semantics, so run the closure once inside a single plain transaction.
        // The requested isolation level, read-only, deferrable, and retry
        // options are Postgres-only and are ignored here (documented).
        // After-commit callbacks still fire on commit, exactly as on the
        // Postgres path.
        #[cfg(feature = "sqlite")]
        {
            let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));
            let conn: &mut RuntimeConnection = &mut self.conn;
            let result: Result<T, E> = AFTER_COMMIT_REGISTRY
                .scope(registry.clone(), scoped_transaction::<T, E, _, _>(conn, f))
                .instrument(span.clone())
                .await;
            span.record("db.tx.attempts", 1u32);
            guard.disarmed = true;
            // This block is the whole remaining function body under the feature
            // (the Postgres retry loop below is cfg'd out), so the `match` is the
            // tail expression — no `return` needed.
            match result {
                Ok(value) => {
                    let callbacks: Vec<CommitCallback> = {
                        let mut reg = registry.lock().expect("registry lock");
                        std::mem::take(&mut *reg)
                    };
                    if !callbacks.is_empty() {
                        let _ = spawn_committed_after_commit_callbacks(callbacks);
                    }
                    Ok(value)
                }
                Err(user_error) => Err(crate::error::AutumnError::from(user_error)),
            }
        }

        #[cfg(not(feature = "sqlite"))]
        {
            let max_attempts = opts.effective_max_attempts();
            let mut attempt: u32 = 0;

            let outcome: Result<T, crate::error::AutumnError> = loop {
                attempt += 1;

                // A fresh registry per attempt: a rolled-back attempt's after-commit
                // callbacks are discarded simply by dropping this `Arc` undrained.
                let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

                let attempt_result: Result<T, E> = {
                    // *** LOAD-BEARING ORDER: borrow `f` BEFORE `build_transaction()`.
                    // diesel-async's `TransactionBuilder::run` bounds the closure by
                    // the connection-borrow lifetime; `&mut f`'s region must start
                    // earlier than that borrow or it fails to compile. Do not reorder.
                    let f_ref = &mut f;

                    let mut builder = self.conn.build_transaction();
                    builder = match opts.isolation {
                        IsolationLevel::ReadCommitted => builder.read_committed(),
                        IsolationLevel::RepeatableRead => builder.repeatable_read(),
                        IsolationLevel::Serializable => builder.serializable(),
                    };
                    if opts.read_only {
                        builder = builder.read_only();
                    }
                    if opts.deferrable {
                        builder = builder.deferrable();
                    }

                    AFTER_COMMIT_REGISTRY
                        .scope(
                            registry.clone(),
                            builder.run::<T, E, _>(async move |conn| f_ref(conn).await),
                        )
                        .instrument(span.clone())
                        .await
                };

                match attempt_result {
                    Ok(value) => {
                        // Commit path: drain THIS attempt's callbacks and spawn
                        // them. (The `is_test_tx` case already returned above, so
                        // spawning here is always live.)
                        let callbacks: Vec<CommitCallback> = {
                            let mut reg = registry.lock().expect("registry lock");
                            std::mem::take(&mut *reg)
                        };
                        if !callbacks.is_empty() {
                            let _ = spawn_committed_after_commit_callbacks(callbacks);
                        }
                        break Ok(value);
                    }
                    Err(e) => {
                        // Convert to AutumnError first, then classify on it.
                        let ae = crate::error::AutumnError::from(e);
                        let retryable = is_retryable_txn_error(&ae);
                        match retry_decision(attempt, max_attempts, retryable) {
                            RetryDecision::Retry => {
                                let retries_total = record_tx_retry();
                                let delay = retry_backoff_delay(
                                    opts.initial_backoff,
                                    opts.max_backoff,
                                    attempt,
                                );
                                tracing::debug!(
                                    parent: &span,
                                    attempt,
                                    max_attempts,
                                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                    autumn.tx.retries_total = retries_total,
                                    "retrying transaction after serialization/deadlock failure"
                                );
                                // `registry` drops here → rolled-back attempt's
                                // after-commit callbacks are discarded; the loop
                                // then re-runs the closure.
                                tokio::time::sleep(delay).await;
                            }
                            RetryDecision::Stop => {
                                if retryable {
                                    let exhausted_total = record_tx_retry_exhausted();
                                    tracing::warn!(
                                        parent: &span,
                                        attempt,
                                        max_attempts,
                                        autumn.tx.retry_exhausted_total = exhausted_total,
                                        "transaction retry budget exhausted; returning final error"
                                    );
                                }
                                break Err(ae);
                            }
                        }
                    }
                }
            };

            span.record("db.tx.attempts", attempt);
            guard.disarmed = true;
            outcome
        }
    }
}

impl std::ops::Deref for Db {
    type Target = RuntimeConnection;
    fn deref(&self) -> &Self::Target {
        assert!(
            !self.tx_poisoned,
            "Db connection is poisoned due to a cancelled/dropped transaction"
        );
        &self.conn
    }
}

impl std::ops::DerefMut for Db {
    fn deref_mut(&mut self) -> &mut Self::Target {
        assert!(
            !self.tx_poisoned,
            "Db connection is poisoned due to a cancelled/dropped transaction"
        );
        &mut self.conn
    }
}

/// Everything required to check out and instrument a pooled connection.
///
/// Shared by the plain [`Db`] extractor and shard-routed checkouts so that
/// every connection — regardless of which pool it came from — gets the same
/// span, interceptor, statement-timeout, and slow-query treatment.
pub(crate) struct DbCheckoutParams<'a> {
    /// Pool to check the connection out of.
    pub pool: &'a Pool<RuntimeConnection>,
    /// Role label surfaced to [`DbConnectionInterceptor`]s, e.g. `"primary"`
    /// or `"shard:<name>:primary"`.
    pub pool_name: &'a str,
    /// Shard name recorded on the `db.connection` span, when routed.
    pub shard: Option<&'a str>,
    /// Resolved statement timeout (route override already merged with the
    /// global config). `None` disables the timeout (`SET statement_timeout = 0`).
    pub statement_timeout: Option<std::time::Duration>,
    /// `"METHOD /matched/path"` key used for per-route DB metrics.
    pub route_key: Option<String>,
    pub metrics: Option<crate::middleware::MetricsCollector>,
    pub slow_query_threshold: std::time::Duration,
    pub interceptors: Vec<std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
    /// Why this app's pools cannot record capture traffic, from the state's
    /// topology (see [`DatabaseTopology::capture_gap`]); `None` when they can.
    #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
    pub capture_gap: Option<std::sync::Arc<str>>,
    /// The app's injected clock, supplying the checkout-to-release timing and
    /// the per-statement query timer. Threaded through so connection latency is
    /// virtual (and reproducible) under a `#[sim_test]` instead of being read
    /// from a raw `std::time::Instant`.
    pub clock: std::sync::Arc<dyn crate::time::ClockSource>,
}

/// The capsule attribution marker to merge into the checkout round trip, or
/// `None` when this checkout has nothing to attribute.
///
/// Scope presence is per-request, per-app truth: a scope only exists under a
/// capture-enabled router's `CaptureLayer`, so two apps with different capture
/// settings in one process cannot disturb each other. A scope-free checkout
/// sends nothing — stale bindings are cleared by the recording pool's own
/// create/recycle hooks, which run before any borrower's first statement
/// (#1598, F2). An id that would not survive interpolation yields `None`
/// rather than a quoted-string escape (F24).
///
/// `capture_gap` is the app's own pool truth: when the topology could not be
/// built with recording pools (TLS, a custom provider), the scope is noted
/// and truncated instead — there is no recorder on the wire for a marker to
/// reach.
#[cfg(not(feature = "sqlite"))]
fn capsule_checkout_marker(capture_gap: Option<&str>) -> Option<String> {
    #[cfg(feature = "reporting")]
    {
        let scope = crate::capsule::current_scope()?;
        if let Some(reason) = capture_gap {
            // Capture had to step aside for this app's database: say so in
            // the capsule rather than leaving a reader to wonder where the DB
            // tape went.
            crate::capsule::record_db::note_db_capture_unavailable(&scope, reason);
            return None;
        }
        Some(scope.id().to_owned())
            .filter(|id| crate::capsule::is_valid_scope_id(id))
            .and_then(|id| crate::capsule::wire::marker_set_sql(&id))
    }
    #[cfg(not(feature = "reporting"))]
    {
        let _ = capture_gap;
        None
    }
}

impl Db {
    /// Check a connection out of `params.pool` with full instrumentation.
    ///
    /// This is the single code path behind the [`Db`] extractor and all
    /// shard-routed checkouts: span creation, checkout interceptors,
    /// `SET statement_timeout`, and the metrics captured for the
    /// slow-query warning on `Drop`.
    pub(crate) async fn checkout(params: DbCheckoutParams<'_>) -> Result<Self, AutumnError> {
        // `SET statement_timeout` (and its i32-cap arithmetic below) is a
        // Postgres session GUC. Under the `sqlite` feature the runtime backend
        // is entirely SQLite (the `RuntimeConnection` alias flips wholesale —
        // see `build_sqlite_pool`), which rejects the statement and would turn
        // every `Db`-using route into a 503. The const, the `RunQueryDsl`
        // import, the timeout arithmetic, and the `SET` itself are therefore all
        // gated off on the SQLite build; the Postgres path is byte-identical.
        #[cfg(not(feature = "sqlite"))]
        const PG_TIMEOUT_MAX_MS: u64 = i32::MAX as u64;
        #[cfg(not(feature = "sqlite"))]
        use diesel_async::RunQueryDsl as _;

        // Span covers the full time the connection is held — from
        // checkout through the end of the request — rather than just
        // `pool.get()`. Dropping `Db` closes the span, so span duration
        // reflects real connection hold time and `db.system=postgresql`
        // propagates to any query futures handlers instrument with
        // `db.span()`.
        let span = tracing::info_span!(
            "db.connection",
            otel.kind = "client",
            db.system = "postgresql",
            db.shard = tracing::field::Empty,
        );
        if let Some(shard) = params.shard {
            span.record("db.shard", shard);
        }

        // Failure-capsule slice one records only the *control* topology's pools
        // (#1598): `[[database.shards]]` pools are built by `create_shard_set`,
        // never through the capture factory, so a request that reaches for a
        // shard generates database traffic no capsule can hold. Flag it here —
        // before the checkout, because whether the connection is obtained does
        // not change the fact that this request's tape is incomplete — so the
        // capsule says so and replay refuses it, rather than presenting a tape
        // that silently omits the shard's effects.
        #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
        if params.shard.is_some() {
            crate::capsule::record_db::note_shard_capture_gap();
        }

        // The same honesty rule for a backend that has no wire capture at all:
        // a `SQLite` build tees nothing (F18), so a capsule for a request that
        // used the database is missing that request's effects and must be
        // refused by replay rather than presented as complete.
        #[cfg(all(feature = "reporting", feature = "sqlite"))]
        crate::capsule::note_backend_capture_gap();

        let pool = params.pool;
        let mut checkout_future: std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<PooledConnection, AutumnError>> + Send + '_,
            >,
        > = Box::pin(async move {
            pool.get().await.map_err(|e| {
                tracing::error!("Failed to acquire database connection: {e}");
                AutumnError::service_unavailable_msg(e.to_string())
            })
        });
        for interceptor in &params.interceptors {
            let ctx = crate::interceptor::DbCheckoutContext {
                pool_name: params.pool_name.to_string(),
            };
            checkout_future = interceptor.intercept_checkout(ctx, checkout_future);
        }

        let mut conn = checkout_future.instrument(span.clone()).await?;

        // `statement_timeout` is a Postgres session GUC; it is intentionally
        // unused on the SQLite backend (see the gating note below), so consume
        // it here to keep the shared `DbCheckoutParams` field from reading as
        // dead code under `--features sqlite`.
        #[cfg(feature = "sqlite")]
        let _ = params.statement_timeout;

        // Postgres statement_timeout is a signed 32-bit integer (milliseconds).
        // Cap at i32::MAX to avoid a confusing 503 for very large configured values.
        #[cfg(not(feature = "sqlite"))]
        let timeout_ms = params.statement_timeout.map_or(0u64, |d| {
            u64::try_from(d.as_millis())
                .unwrap_or(PG_TIMEOUT_MAX_MS)
                .min(PG_TIMEOUT_MAX_MS)
        });

        // Install a fresh per-request query timer, but only when a query observer is
        // active: either a `REQUEST_DB_TIMINGS` scope (the `ServerTimingLayer`, enabled
        // by `[observability] server_timing`) or a `REQUEST_QUERY_CAPTURE` scope (the
        // test harness capturing the SQL list, which runs with `server_timing` off). The
        // timer feeds both lanes via `record_request_db_query`, and is installed before
        // the `SET statement_timeout` housekeeping statement below.
        //
        // `set_instrumentation` wholesale replaces the connection's instrumentation, so
        // it must not run unconditionally: an application that registered a global
        // default via `diesel::connection::set_default_instrumentation` — query logging,
        // tracing, metrics — would have it silently clobbered on the first checkout and
        // never restored, even with `server_timing` disabled. Gating on
        // `request_db_timing_active() || request_query_capture_active()` preserves the
        // app's instrumentation whenever neither lane is scoped, the production default,
        // and overwrites it only while a query observer is active.
        //
        // Installing a fresh timer on every observed checkout also clears any stale
        // `RequestQueryTimer` a pooled connection carried from a prior request —
        // diesel-async's deadpool manager never resets instrumentation on recycle — so a
        // stale timer can never record the upcoming housekeeping `SET`, or later app
        // queries, into another request's accumulator. Installing before the `SET`, which
        // `is_uncounted_statement` classifies as housekeeping, keeps that statement out
        // of the `Server-Timing` `db` count regardless. A stale timer left on a
        // connection later reused by an opted-out request is a cheap no-op: `on_start`
        // probes both lanes before formatting or recording anything.
        #[cfg(feature = "db")]
        {
            use diesel_async::AsyncConnection as _;
            if request_db_timing_active() || request_query_capture_active() {
                conn.set_instrumentation(RequestQueryTimer::with_clock(std::sync::Arc::clone(
                    &params.clock,
                )));
            }
        }

        // Postgres-only per-checkout initialization; see the gating note above.
        // SQLite builds skip it entirely (it would 503 every `Db`-using route).
        //
        // When failure-capsule capture is armed (#1598), the capsule
        // attribution marker rides along in this SAME round trip as one simple
        // batch, so recording costs no extra latency on the checkout path. With
        // capture off — the default — the statement issued here is byte-for-byte
        // what it has always been.
        #[cfg(not(feature = "sqlite"))]
        {
            #[cfg(feature = "reporting")]
            let capture_gap = params.capture_gap.as_deref();
            #[cfg(not(feature = "reporting"))]
            let capture_gap = None;
            let outcome = match capsule_checkout_marker(capture_gap) {
                Some(marker) => {
                    use diesel_async::SimpleAsyncConnection as _;
                    conn.batch_execute(&format!("SET statement_timeout = {timeout_ms}; {marker}"))
                        .await
                }
                None => diesel::sql_query(format!("SET statement_timeout = {timeout_ms}"))
                    .execute(&mut conn)
                    .await
                    .map(|_| ()),
            };
            outcome.map_err(|e| {
                tracing::error!("Failed to set database statement_timeout to {timeout_ms}ms: {e}");
                AutumnError::service_unavailable_msg(format!("Database initialization error: {e}"))
            })?;
        }

        let start_time = params.clock.monotonic();
        let is_test_tx = params
            .interceptors
            .iter()
            .any(|i| i.is_transactional_test());

        Ok(Self {
            conn,
            span,
            tx_depth: 0,
            tx_poisoned: false,
            route_key: params.route_key,
            metrics: params.metrics,
            slow_query_threshold: params.slow_query_threshold,
            start_time,
            clock: params.clock,
            is_test_tx,
        })
    }

    /// Check a plain, uninstrumented connection out of `pool` for use in tests.
    ///
    /// Unlike the request extractor, this applies no interceptors, statement
    /// timeout, or route metrics — it exists so integration tests can drive
    /// [`Db::tx`] / [`Db::tx_with`] directly against a [`crate::test::TestDb`]
    /// pool without spinning up a full `TestApp`.
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] if a connection cannot be acquired from `pool`.
    #[cfg(feature = "test-support")]
    pub async fn connect_for_test(pool: &Pool<RuntimeConnection>) -> Result<Self, AutumnError> {
        Self::checkout(DbCheckoutParams {
            pool,
            pool_name: "test",
            shard: None,
            statement_timeout: None,
            route_key: None,
            metrics: None,
            slow_query_threshold: std::time::Duration::from_millis(500),
            interceptors: Vec::new(),
            #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
            capture_gap: None,
            // No `AppState` here by construction — this helper exists so a test
            // can drive `Db::tx` against a bare pool. The real clock matches the
            // behaviour this path had before the clock became injectable.
            clock: std::sync::Arc::clone(&DEFAULT_SYSTEM_CLOCK),
        })
        .await
    }
}

/// Request-derived context shared by every `Db`-producing extractor.
///
/// Captures the route-override statement timeout, the matched-path metrics
/// key, and the state-held instrumentation handles so shard-routed
/// checkouts behave identically to the plain [`Db`] extractor.
#[derive(Clone)]
pub(crate) struct RequestDbContext {
    pub statement_timeout: Option<std::time::Duration>,
    pub route_key: Option<String>,
    pub metrics: Option<crate::middleware::MetricsCollector>,
    pub slow_query_threshold: std::time::Duration,
    pub interceptors: Vec<std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
    /// The app's injected clock, carried so a shard-routed checkout times on
    /// the same seam the plain `Db` extractor does.
    pub clock: std::sync::Arc<dyn crate::time::ClockSource>,
}

impl RequestDbContext {
    pub(crate) fn from_parts<S: DbState>(parts: &axum::http::request::Parts, state: &S) -> Self {
        let timeout_override = parts.extensions.get::<StatementTimeout>().copied();
        let matched_path = parts
            .extensions
            .get::<axum::extract::MatchedPath>()
            .map_or_else(|| parts.uri.path(), axum::extract::MatchedPath::as_str);
        Self {
            statement_timeout: timeout_override
                .map(|t| t.0)
                .or_else(|| state.statement_timeout()),
            route_key: Some(format!("{} {}", parts.method, matched_path)),
            metrics: state.metrics().cloned(),
            slow_query_threshold: state.slow_query_threshold(),
            interceptors: state.db_interceptors(),
            clock: state.clock(),
        }
    }
}

/// A database checkout that has been *prepared* but not *taken*.
///
/// [`Db`] is a `FromRequestParts` extractor, so axum runs it before the final
/// `FromRequest` extractor reads the request body. A handler taking both
/// therefore holds a pooled connection for as long as the client takes to send
/// its body — and the client controls that. A handful of slow-body requests can
/// pin `pool_size` connections and starve unrelated database work, with no
/// request timeout configured by default to stop them.
///
/// This captures what the checkout needs from the request parts — statement
/// timeout, route key, metrics, interceptors, clock — and takes the connection
/// only when [`DeferredDb::checkout`] is awaited, which the handler does after
/// the body is in hand.
///
/// `pub(crate)` deliberately: the general answer is a lazy `Db` for every
/// handler (#2264), and shipping a second public extractor would make that
/// harder to land rather than easier.
pub(crate) struct DeferredDb {
    pool: Pool<RuntimeConnection>,
    ctx: RequestDbContext,
    #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
    capture_gap: Option<std::sync::Arc<str>>,
}

impl DeferredDb {
    /// Take the connection. Called once the request body has been read.
    pub(crate) async fn checkout(self) -> Result<Db, AutumnError> {
        let result = Db::checkout(DbCheckoutParams {
            pool: &self.pool,
            pool_name: "primary",
            shard: None,
            statement_timeout: self.ctx.statement_timeout,
            route_key: self.ctx.route_key,
            metrics: self.ctx.metrics,
            slow_query_threshold: self.ctx.slow_query_threshold,
            interceptors: self.ctx.interceptors,
            #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
            capture_gap: self.capture_gap,
            clock: self.ctx.clock,
        })
        .await;
        // The same read-your-writes bookkeeping the eager extractor does, at
        // the moment the connection is actually taken.
        if result.is_ok() {
            crate::read_your_writes::mark_write();
        }
        result
    }
}

impl<S> FromRequestParts<S> for DeferredDb
where
    S: DbState + Send + Sync,
{
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        // The pool is resolved here, so a misconfigured app still fails at
        // extraction exactly as it does with `Db`. Only the checkout moves.
        let pool = state
            .pool()
            .ok_or_else(|| AutumnError::service_unavailable_msg("Database not configured"))?
            .clone();
        Ok(Self {
            pool,
            ctx: RequestDbContext::from_parts(parts, state),
            #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
            capture_gap: state.db_capture_gap(),
        })
    }
}

impl<S> FromRequestParts<S> for Db
where
    S: DbState + Send + Sync,
{
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let pool = state
            .pool()
            .ok_or_else(|| AutumnError::service_unavailable_msg("Database not configured"))?;
        let ctx = RequestDbContext::from_parts(parts, state);

        let result = Self::checkout(DbCheckoutParams {
            pool,
            pool_name: "primary",
            shard: None,
            statement_timeout: ctx.statement_timeout,
            route_key: ctx.route_key,
            metrics: ctx.metrics,
            slow_query_threshold: ctx.slow_query_threshold,
            interceptors: ctx.interceptors,
            #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
            capture_gap: state.db_capture_gap(),
            clock: ctx.clock,
        })
        .await;
        // Notify the RYWW task-local that a primary connection was checked out.
        // No-op when read_your_writes = "off" (task-local absent).
        if result.is_ok() {
            crate::read_your_writes::mark_write();
        }
        result
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let (Some(route_key), Some(metrics)) = (&self.route_key, &self.metrics) {
            let elapsed = self
                .clock
                .monotonic()
                .saturating_duration_since(self.start_time);
            let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

            // Record DB query metric
            let metric_key = format!("{route_key} SELECT");
            metrics.record_db_query(&metric_key, elapsed_ms);
            // Deliberately not recorded into the Server-Timing per-request
            // accumulator. `elapsed` here is the whole connection
            // checkout-to-release window (see `start_time` and the `span` doc
            // above), not a single query's wall time. Every request that extracts
            // `Db` would otherwise add its entire connection-hold time as one
            // bogus "query", inflating both `db;dur` and the `desc="N queries"`
            // count, and double-counting against real queries recorded by
            // `run_instrumented`. The accumulator must reflect genuine
            // instrumented queries only.

            // Log slow query if it exceeds the threshold
            if elapsed >= self.slow_query_threshold {
                tracing::warn!(
                    route = %route_key,
                    sql = "SELECT ?",
                    duration_ms = elapsed_ms,
                    "slow database query"
                );
            }
        }
    }
}

// ----------------------------------------------------------------------------
// DatabasePoolProvider — tier-1 boot-time replaceable pool factory
// ----------------------------------------------------------------------------

/// Pluggable boot-time database pool factory.
///
/// Replace the default `deadpool + diesel-async` factory with a custom
/// strategy (custom metrics wrapper, circuit breaker, separate pools per
/// shard, etc.) by implementing this trait and installing it on the
/// [`AppBuilder`](crate::app::AppBuilder) via
/// [`with_pool_provider`](crate::app::AppBuilder::with_pool_provider).
///
/// The trait abstracts the *factory*, not the pool *type* — the return type is
/// fixed at `Pool<AsyncPgConnection>` for now. Swapping to a different backend
/// (e.g. `MySQL`, `SQLite`) would require generic `Pool<C>` propagation through
/// `Db` / `DbState` / `AppState` and is intentionally out of scope.
///
/// Providers that only implement [`DatabasePoolProvider::create_pool`] still
/// participate in primary/replica topology: the default
/// [`DatabasePoolProvider::create_topology`] uses the custom primary pool and
/// builds the configured replica role with Autumn's deadpool factory. Override
/// `create_topology` when both roles need custom construction.
///
/// # Example
///
/// ```rust,no_run
/// use autumn_web::config::DatabaseConfig;
/// use autumn_web::db::{DatabasePoolProvider, PoolError};
/// use diesel_async::AsyncPgConnection;
/// use diesel_async::pooled_connection::deadpool::Pool;
///
/// pub struct MetricsPoolProvider;
///
/// impl DatabasePoolProvider for MetricsPoolProvider {
///     async fn create_pool(
///         &self,
///         config: &DatabaseConfig,
///     ) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
///         // Wrap the default pool with custom metrics, then return it.
///         autumn_web::db::create_pool(config)
///     }
/// }
/// ```
pub trait DatabasePoolProvider: Send + Sync + 'static {
    /// Create a connection pool from the resolved [`DatabaseConfig`].
    ///
    /// Returning `Ok(None)` signals that the application should run without a
    /// database — useful for static-site / API-gateway use cases or for
    /// disabling the DB in test contexts.
    fn create_pool(
        &self,
        config: &DatabaseConfig,
    ) -> impl std::future::Future<Output = Result<Option<Pool<RuntimeConnection>>, PoolError>> + Send;

    /// Create primary and optional replica pools from the resolved
    /// [`DatabaseConfig`].
    ///
    /// The default implementation preserves the provider's custom primary pool
    /// and builds a replica pool when `database.replica_url` is configured.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] if either configured role cannot be built.
    fn create_topology(
        &self,
        config: &DatabaseConfig,
    ) -> impl std::future::Future<Output = Result<Option<DatabaseTopology>, PoolError>> + Send {
        async move {
            let Some(primary) = self.create_pool(config).await? else {
                return Ok(None);
            };

            // A custom provider that overrides only `create_pool` still gets its replica
            // built by this default, so the SQLite-replica rule must be enforced here
            // too. Otherwise a provider configuring a distinct or in-memory
            // `database.replica_url` would boot two unrelated SQLite databases and route
            // reads to an empty or stale replica. Reuse the same helper
            // `create_topology`/`create_shard_topology` use, so the rejection rule cannot
            // drift. The primary URL is whatever `effective_primary_url` resolves; a
            // provider returning a pool for a `None` primary URL has no URL to compare,
            // so skip then.
            #[cfg(feature = "sqlite")]
            if let (Some(primary_url), Some(replica_url)) = (
                config.effective_primary_url(),
                config.replica_url.as_deref(),
            ) {
                reject_unusable_sqlite_replica(primary_url, replica_url)?;
            }

            let replica = config
                .replica_url
                .as_deref()
                .map(|url| {
                    build_pool(
                        url,
                        config.effective_replica_pool_size(),
                        config.connect_timeout_secs,
                    )
                })
                .transpose()?;

            Ok(Some(DatabaseTopology::from_pools(primary, replica)))
        }
    }

    /// Create one shard's [`DatabaseTopology`] from its
    /// `[[database.shards]]` entry.
    ///
    /// The default implementation uses Autumn's deadpool factory for both
    /// roles. Override to decorate per-shard pools (metrics wrappers,
    /// circuit breakers) the same way `create_pool` decorates the control
    /// role.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] if either configured role cannot be built.
    fn create_shard_topology(
        &self,
        shard: &crate::config::ShardConfig,
        defaults: &DatabaseConfig,
    ) -> impl std::future::Future<Output = Result<DatabaseTopology, PoolError>> + Send {
        async move { create_shard_topology(shard, defaults) }
    }
}

/// Default [`DatabasePoolProvider`] — the `deadpool + diesel-async` factory.
///
/// Delegates to the free function [`create_pool`]. This is the provider used
/// when no override is installed via
/// [`with_pool_provider`](crate::app::AppBuilder::with_pool_provider).
#[derive(Debug, Default, Clone, Copy)]
pub struct DieselDeadpoolPoolProvider;

impl DieselDeadpoolPoolProvider {
    /// Construct a new default provider.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DatabasePoolProvider for DieselDeadpoolPoolProvider {
    async fn create_pool(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Option<Pool<RuntimeConnection>>, PoolError> {
        create_pool(config)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn replication_adds_the_auto_checkpoint_lock_to_the_pooled_pragmas() {
        // Continuous replication (#1628) needs the replicator to be the only
        // component that ever checkpoints.
        let plain = super::sqlite_connection_pragmas(false, false);
        let replicating = super::sqlite_connection_pragmas(false, true);
        assert!(!plain.contains("wal_autocheckpoint"));
        assert!(replicating.contains("PRAGMA wal_autocheckpoint = 0"));
        for pragmas in [plain, replicating] {
            assert!(pragmas.contains("journal_mode = WAL"));
            assert!(pragmas.contains("foreign_keys = ON"));
            assert!(pragmas.contains("busy_timeout = 5000"));
        }

        // A read-only target still gets no writing pragmas — including under
        // replication, which cannot apply to a database nothing writes to.
        for replicating in [false, true] {
            let read_only = super::sqlite_connection_pragmas(true, replicating);
            assert!(!read_only.contains("journal_mode"));
            assert!(!read_only.contains("wal_autocheckpoint"));
            assert!(read_only.contains("foreign_keys = ON"));
        }
    }

    use super::*;
    use crate::config::DatabaseConfig;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    // ── RequestQueryTimer / Server-Timing db accumulator tests ───

    /// The connection instrumentation records one query per completed
    /// `StartQuery`/`FinishQuery` pair into the request accumulator, summing
    /// elapsed time. Drives the extracted `on_start`/`on_finish` accounting
    /// (the `InstrumentationEvent` itself is non-exhaustive and its
    /// constructors are gated behind an unstable diesel feature, so it cannot
    /// be built in a test). Deterministic: derives finish instants from the
    /// start via `Instant + Duration`, no sleeps.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn request_query_timer_accumulates_finished_queries() {
        let timings = Arc::new(RequestDbTimings::default());
        REQUEST_DB_TIMINGS
            .scope(Arc::clone(&timings), async {
                let mut timer = RequestQueryTimer::default();

                // Query 1: 1200µs.
                let t0 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t0, || "SELECT 1".to_string());
                timer.on_finish(t0.saturating_add(Duration::from_micros(1_200)));

                // Query 2: 800µs.
                let t1 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t1, || "UPDATE users SET name = $1".to_string());
                timer.on_finish(t1.saturating_add(Duration::from_micros(800)));

                // A stray FinishQuery with no matching StartQuery is ignored.
                timer.on_finish(crate::time::MonotonicInstant::ORIGIN);
            })
            .await;

        assert_eq!(
            timings.query_count.load(Ordering::Relaxed),
            2,
            "only completed StartQuery/FinishQuery pairs are counted"
        );
        assert_eq!(
            timings.total_us.load(Ordering::Relaxed),
            2_000,
            "elapsed times accumulate (1200µs + 800µs)"
        );
    }

    /// Outside a `REQUEST_DB_TIMINGS` scope the timer records nothing and does
    /// not panic — the off-request / middleware-disabled path.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn request_query_timer_is_noop_off_request() {
        let mut timer = RequestQueryTimer::default();
        let t0 = crate::time::MonotonicInstant::ORIGIN;
        timer.on_start(t0, || "SELECT 1".to_string());
        // Must not panic even though no task-local accumulator is scoped.
        timer.on_finish(t0.saturating_add(Duration::from_micros(500)));
    }

    /// Approach-(b) guarantee: when no `REQUEST_DB_TIMINGS` scope is active,
    /// `on_start` must be a cheap no-op — it must not invoke the (allocating)
    /// SQL-formatting closure, and must leave no in-flight statement. This is
    /// what makes it safe to leave a `RequestQueryTimer` installed on a pooled
    /// connection that a later opted-out request reuses.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn request_query_timer_on_start_is_cheap_noop_off_request() {
        let mut timer = RequestQueryTimer::default();
        let invoked = std::cell::Cell::new(false);
        let t0 = crate::time::MonotonicInstant::ORIGIN;
        timer.on_start(t0, || {
            invoked.set(true);
            "SELECT 1".to_string()
        });
        assert!(
            !invoked.get(),
            "off-request, on_start must not format the SQL (no allocation)"
        );
        // No in-flight statement was recorded, so on_finish is a no-op.
        timer.on_finish(t0.saturating_add(Duration::from_micros(500)));
    }

    /// Regression for the stale-timer / housekeeping-`SET` bug: a pooled
    /// connection reused across requests keeps a `RequestQueryTimer` installed,
    /// and `Db::checkout` runs a `SET statement_timeout` before handing the
    /// connection over. That housekeeping `SET` must NOT be counted, or every
    /// `Server-Timing` request would report a bogus `+1 query` before any app
    /// SQL. After the `SET`, the first real `SELECT` increments the count to
    /// exactly 1 and only its latency accumulates.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn request_query_timer_excludes_checkout_statement_timeout_set() {
        let timings = Arc::new(RequestDbTimings::default());
        REQUEST_DB_TIMINGS
            .scope(Arc::clone(&timings), async {
                // A timer as it would be freshly installed at checkout.
                let mut timer = RequestQueryTimer::default();

                // Checkout housekeeping `SET` — must not be counted.
                let t0 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t0, || "SET statement_timeout = 5000".to_string());
                timer.on_finish(t0.saturating_add(Duration::from_millis(3)));

                assert_eq!(
                    timings.query_count.load(Ordering::Relaxed),
                    0,
                    "the checkout SET statement_timeout must not be counted"
                );
                assert_eq!(
                    timings.total_us.load(Ordering::Relaxed),
                    0,
                    "the checkout SET contributes no latency to db;dur"
                );

                // First real application query: 600µs.
                let t1 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t1, || "SELECT * FROM users".to_string());
                timer.on_finish(t1.saturating_add(Duration::from_micros(600)));

                assert_eq!(
                    timings.query_count.load(Ordering::Relaxed),
                    1,
                    "the real SELECT is the first counted query"
                );
                assert_eq!(
                    timings.total_us.load(Ordering::Relaxed),
                    600,
                    "only the SELECT's 600µs accumulates; the SET is excluded"
                );
            })
            .await;
    }

    /// `request_db_timing_active` — the predicate `RequestQueryTimer::on_start`
    /// probes to decide whether to record — must report `false` outside a
    /// `REQUEST_DB_TIMINGS` scope (the production / `server_timing` disabled
    /// default) and `true` inside one (the `ServerTimingLayer`-scoped request).
    /// This predicate gates two things: whether `Db::checkout` installs the
    /// timer at all (skipped off-scope so an app's own instrumentation is not
    /// clobbered), and whether a stale timer left on a reused connection does
    /// any work — with the probe `false`, `on_start` is a cheap no-op.
    ///
    /// NOTE: the real `Db::checkout` path needs a live Postgres connection,
    /// which the sandbox cannot provide, so this exercises the predicate
    /// directly.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn request_db_timing_active_reflects_scope() {
        assert!(
            !request_db_timing_active(),
            "no REQUEST_DB_TIMINGS scope active outside the ServerTimingLayer: \
             the timer records nothing"
        );

        let timings = Arc::new(RequestDbTimings::default());
        REQUEST_DB_TIMINGS
            .scope(Arc::clone(&timings), async {
                assert!(
                    request_db_timing_active(),
                    "inside a REQUEST_DB_TIMINGS scope the probe is active: \
                     the timer records queries"
                );
            })
            .await;

        // Scope has ended; the gate reads false again.
        assert!(
            !request_db_timing_active(),
            "the gate is false again once the scope is dropped"
        );
    }

    /// A transaction wraps its inner statements in `BEGIN`/`COMMIT`, which
    /// diesel-async runs through `batch_execute` — emitting a
    /// `StartQuery`/`FinishQuery` pair with that transaction-control SQL, just
    /// like a real query. The timer must skip those so a transaction with one
    /// real `SELECT` reports `desc="1 query"`, not `"3 queries"`, and excludes
    /// begin/commit latency from `db;dur`.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn request_query_timer_excludes_transaction_control_statements() {
        let timings = Arc::new(RequestDbTimings::default());
        REQUEST_DB_TIMINGS
            .scope(Arc::clone(&timings), async {
                let mut timer = RequestQueryTimer::default();

                // BEGIN — must not be counted (10_000µs, would dominate if it
                // leaked into the total).
                let t0 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t0, || "BEGIN".to_string());
                timer.on_finish(t0.saturating_add(Duration::from_millis(10)));

                // The single real query: 700µs.
                let t1 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t1, || "SELECT * FROM users".to_string());
                timer.on_finish(t1.saturating_add(Duration::from_micros(700)));

                // COMMIT — must not be counted.
                let t2 = crate::time::MonotonicInstant::ORIGIN;
                timer.on_start(t2, || "COMMIT".to_string());
                timer.on_finish(t2.saturating_add(Duration::from_millis(10)));
            })
            .await;

        assert_eq!(
            timings.query_count.load(Ordering::Relaxed),
            1,
            "only the real SELECT is counted; BEGIN/COMMIT are excluded"
        );
        assert_eq!(
            timings.total_us.load(Ordering::Relaxed),
            700,
            "only the SELECT's 700µs accumulates; begin/commit latency is excluded"
        );
    }

    /// The checkout install gate: `Db::checkout` calls `set_instrumentation`
    /// (which WHOLESALE REPLACES the connection's instrumentation — clobbering
    /// any app-registered `diesel::connection::set_default_instrumentation`)
    /// ONLY when `request_db_timing_active()` is true. Outside a
    /// `REQUEST_DB_TIMINGS` scope (the `server_timing`-disabled production
    /// default) the gate is false, so the guarded install is skipped and a
    /// pre-existing sentinel instrumentation is left intact. Inside a scope the
    /// timer is installed.
    ///
    /// NOTE: the real `Db::checkout` path needs a live Postgres connection the
    /// sandbox cannot provide, so this models the connection's single
    /// instrumentation slot and drives the exact gate expression from
    /// `Db::checkout`.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn checkout_installs_timer_only_when_scope_active() {
        // Models the connection's single instrumentation slot. "app" stands for
        // an application-registered default (`set_default_instrumentation`);
        // "timer" is autumn's `RequestQueryTimer`. Mirrors the guarded block in
        // `Db::checkout`: `if request_db_timing_active() { install timer }`.
        fn run_checkout_gate(slot: &mut &'static str) {
            if request_db_timing_active() {
                *slot = "timer";
            }
        }

        // Off-scope (server_timing disabled / production default): the gate is
        // false, so the app's instrumentation must survive untouched.
        assert!(!request_db_timing_active());
        let mut slot = "app";
        run_checkout_gate(&mut slot);
        assert_eq!(
            slot, "app",
            "off-scope checkout must NOT clobber the app's instrumentation"
        );

        // Inside a scope (server_timing enabled, handler wrapped by the layer):
        // the gate is true, so autumn installs its RequestQueryTimer.
        let timings = Arc::new(RequestDbTimings::default());
        REQUEST_DB_TIMINGS
            .scope(Arc::clone(&timings), async {
                let mut slot = "app";
                run_checkout_gate(&mut slot);
                assert_eq!(
                    slot, "timer",
                    "inside a REQUEST_DB_TIMINGS scope autumn installs its timer"
                );
            })
            .await;
    }

    /// Unit coverage for the uncounted-statement classifier used by the timer:
    /// leading-token match is case-insensitive and covers savepoint/release, the
    /// two-word `START TRANSACTION` form, and any leading `SET` (both
    /// `SET TRANSACTION` and the checkout `SET statement_timeout` housekeeping),
    /// while real application queries are counted. `UPDATE ... SET ...` is
    /// counted because `SET` is not its leading token.
    #[cfg(feature = "db")]
    #[test]
    fn is_uncounted_statement_classifies_statements() {
        for sql in [
            "BEGIN",
            "begin",
            "  COMMIT",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT diesel_savepoint_0",
            "SAVEPOINT diesel_savepoint_1",
            "RELEASE SAVEPOINT diesel_savepoint_0",
            "start transaction",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET statement_timeout = 5000",
            "set statement_timeout = 0",
        ] {
            assert!(
                RequestQueryTimer::is_uncounted_statement(sql),
                "{sql:?} should be treated as an uncounted housekeeping/tx statement"
            );
        }

        for sql in [
            "SELECT 1",
            "UPDATE users SET name = $1",
            "INSERT INTO t VALUES (1)",
            "DELETE FROM t WHERE id = 1",
            "",
        ] {
            assert!(
                !RequestQueryTimer::is_uncounted_statement(sql),
                "{sql:?} should be treated as a countable query"
            );
        }
    }

    /// Finding-1 regression: the capture path stores the parameterised
    /// statement WITHOUT diesel's trailing `-- binds: [...]` annotation, so the
    /// per-row executions of an N+1 pattern (which differ only in their bind
    /// values) collapse to a single template and `detect_n_plus_one` catches
    /// them. Without the strip, each `-- binds: [N]` would normalise to a
    /// distinct template and the N+1 would go undetected. Locks the fix with no
    /// live database.
    #[cfg(feature = "db")]
    #[test]
    fn strip_bind_annotation_lets_detect_n_plus_one_see_per_row_repetition() {
        use crate::inspector::{QueryRecord, detect_n_plus_one};

        // Construct QueryRecords exactly as `record_request_db_query` does:
        // the captured `sql` is diesel's `DebugQuery` `Display` output run
        // through `strip_bind_annotation`.
        fn record(diesel_display: &str) -> QueryRecord {
            QueryRecord {
                sql: strip_bind_annotation(diesel_display).to_owned(),
                params: Vec::new(),
                elapsed_ms: 1,
                location: String::new(),
            }
        }

        // The `-- binds:` marker (and the leading whitespace before it) is
        // stripped; the `$N` placeholder is preserved.
        assert_eq!(
            strip_bind_annotation("SELECT * FROM books WHERE author_id = $1 -- binds: [1]"),
            "SELECT * FROM books WHERE author_id = $1"
        );
        // Zero-bind annotation is stripped too.
        assert_eq!(
            strip_bind_annotation("SELECT * FROM authors -- binds: []"),
            "SELECT * FROM authors"
        );
        // No marker (transaction-control / synthetic input) → unchanged.
        assert_eq!(strip_bind_annotation("SELECT 1"), "SELECT 1");
        assert_eq!(strip_bind_annotation("BEGIN"), "BEGIN");

        // A classic N+1: one parent query, then the same per-row child query
        // executed once per row with a different bind value each time.
        let n_plus_one = vec![
            record("SELECT * FROM authors -- binds: []"),
            record("SELECT * FROM books WHERE author_id = $1 -- binds: [1]"),
            record("SELECT * FROM books WHERE author_id = $1 -- binds: [2]"),
            record("SELECT * FROM books WHERE author_id = $1 -- binds: [3]"),
        ];
        let warning = detect_n_plus_one(&n_plus_one, 3)
            .expect("three identical per-row templates should trip the N+1 detector");
        assert_eq!(
            warning.count, 3,
            "all three per-row executions collapse to a single template"
        );
        assert_eq!(
            warning.sql_template, "select * from books where author_id = $1",
            "the reported template is the parameterised statement, bind-free"
        );

        // Distinct query templates must NOT trip the detector, even at the
        // same total query count.
        let distinct = vec![
            record("SELECT * FROM authors -- binds: []"),
            record("SELECT * FROM books WHERE id = $1 -- binds: [1]"),
            record("UPDATE users SET name = $1 WHERE id = $2 -- binds: [\"x\", 2]"),
        ];
        assert!(
            detect_n_plus_one(&distinct, 3).is_none(),
            "three distinct query templates must not be reported as N+1"
        );
    }

    // ── after_commit tests ───────────────────────────────────────

    #[tokio::test]
    async fn register_after_commit_outside_tx_runs_eagerly() {
        // When called outside a db.tx block, the callback should run immediately.
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        register_after_commit(move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn register_after_commit_eager_failure_increments_failure_counter() {
        let before = AFTER_COMMIT_FAILURES_TOTAL.load(Ordering::Relaxed);

        register_after_commit(|| async {
            Err(crate::AutumnError::internal_server_error_msg(
                "deliberate eager after-commit failure",
            ))
        })
        .await;

        let after = AFTER_COMMIT_FAILURES_TOTAL.load(Ordering::Relaxed);
        assert!(
            after > before,
            "eager after_commit failures should be counted for recovery signals"
        );
    }

    #[tokio::test]
    async fn register_after_commit_inside_scope_defers_until_drained() {
        // Inside a task-local scope (simulating Db::tx), callbacks are deferred.
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        let registry = Arc::new(std::sync::Mutex::new(Vec::<CommitCallback>::new()));

        // Simulate being inside a db.tx by setting the task-local
        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(move || async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            })
            .await;

        // Callback must NOT have run yet
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Drain and run the callbacks (simulating post-commit)
        let callbacks: Vec<CommitCallback> = {
            let mut reg = registry.lock().unwrap();
            std::mem::take(&mut *reg)
        };
        for cb in callbacks {
            cb().await.unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn register_after_commit_on_rollback_callbacks_dropped() {
        // Callbacks registered inside a tx scope that is NOT drained are dropped.
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        let registry = Arc::new(std::sync::Mutex::new(Vec::<CommitCallback>::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(move || async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            })
            .await;

        // Simulate rollback: drop the callbacks without running them
        drop(registry);

        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn register_after_commit_callbacks_run_in_registration_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let registry = Arc::new(std::sync::Mutex::new(Vec::<CommitCallback>::new()));

        let o1 = order.clone();
        let o2 = order.clone();
        let o3 = order.clone();

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(move || async move {
                    o1.lock().unwrap().push(1);
                    Ok(())
                })
                .await;
                register_after_commit(move || async move {
                    o2.lock().unwrap().push(2);
                    Ok(())
                })
                .await;
                register_after_commit(move || async move {
                    o3.lock().unwrap().push(3);
                    Ok(())
                })
                .await;
            })
            .await;

        let callbacks: Vec<CommitCallback> = {
            let mut reg = registry.lock().unwrap();
            std::mem::take(&mut *reg)
        };
        for cb in callbacks {
            cb().await.unwrap();
        }

        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn production_after_commit_drain_preserves_registration_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let (release_first, wait_first) = tokio::sync::oneshot::channel::<()>();

        let first_order = order.clone();
        let second_order = order.clone();
        let callbacks: Vec<CommitCallback> = vec![
            Box::new(move || {
                Box::pin(async move {
                    wait_first
                        .await
                        .expect("test should release first callback");
                    first_order.lock().unwrap().push(1);
                    Ok(())
                })
            }),
            Box::new(move || {
                Box::pin(async move {
                    second_order.lock().unwrap().push(2);
                    Ok(())
                })
            }),
        ];

        let drain = spawn_committed_after_commit_callbacks(callbacks)
            .expect("non-empty callback list should spawn a drain task");
        tokio::task::yield_now().await;

        assert_eq!(
            *order.lock().unwrap(),
            Vec::<u32>::new(),
            "later callbacks must wait for earlier callbacks to finish"
        );

        release_first
            .send(())
            .expect("first callback receiver alive");
        drain.await.expect("drain task should not panic");

        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn production_after_commit_drain_isolates_panicking_callbacks() {
        let before = AFTER_COMMIT_FAILURES_TOTAL.load(Ordering::Relaxed);
        let ran_later = Arc::new(AtomicU64::new(0));
        let later = ran_later.clone();

        let callbacks: Vec<CommitCallback> = vec![
            Box::new(|| Box::pin(async { panic!("deliberate after_commit panic") })),
            Box::new(move || {
                Box::pin(async move {
                    later.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
        ];

        let drain = spawn_committed_after_commit_callbacks(callbacks)
            .expect("non-empty callback list should spawn a drain task");
        drain.await.expect("panicking callback should be isolated");

        assert_eq!(
            ran_later.load(Ordering::SeqCst),
            1,
            "later callbacks must still run after an earlier callback panics"
        );
        let after = AFTER_COMMIT_FAILURES_TOTAL.load(Ordering::Relaxed);
        assert!(
            after > before,
            "panicking after_commit callbacks must increment the failure counter"
        );
    }

    #[tokio::test]
    async fn db_tx_rejects_ambient_after_commit_registry() {
        let registry = Arc::new(std::sync::Mutex::new(Vec::<CommitCallback>::new()));

        let err = AFTER_COMMIT_REGISTRY
            .scope(registry, async {
                reject_ambient_after_commit_registry_for_tx().expect_err(
                    "starting Db::tx inside an ambient transaction registry should fail",
                )
            })
            .await;

        assert!(
            err.to_string().contains("Nested Db::tx calls"),
            "unexpected nested transaction error: {err}"
        );
    }

    #[tokio::test]
    async fn register_after_commit_callback_error_is_swallowed() {
        // A failing callback is logged but doesn't panic or propagate.
        let registry = Arc::new(std::sync::Mutex::new(Vec::<CommitCallback>::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(|| async {
                    Err(crate::AutumnError::internal_server_error_msg(
                        "deliberate error",
                    ))
                })
                .await;
            })
            .await;

        let callbacks: Vec<CommitCallback> = {
            let mut reg = registry.lock().unwrap();
            std::mem::take(&mut *reg)
        };
        // Running a failing callback should not panic
        for cb in callbacks {
            let _ = cb().await;
        }
    }

    // ── Pool provider trait tests ────────────────────────────────

    /// No-op provider for tests — always returns `Ok(None)` regardless of the
    /// supplied config. Verifies the trait actually overrides the default
    /// (which would otherwise build a pool from the URL).
    struct NoOpPoolProvider;

    impl DatabasePoolProvider for NoOpPoolProvider {
        async fn create_pool(
            &self,
            _config: &DatabaseConfig,
        ) -> Result<Option<Pool<crate::db::RuntimeConnection>>, PoolError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn pool_provider_trait_returns_supplied_pool() {
        // Even with a configured URL, the no-op provider returns None — proving
        // the trait can replace the default factory's behaviour.
        let config = DatabaseConfig {
            url: Some("postgres://localhost/ignored".to_owned()),
            ..Default::default()
        };
        let provider = NoOpPoolProvider;
        let pool = provider
            .create_pool(&config)
            .await
            .expect("no-op provider should succeed");
        assert!(
            pool.is_none(),
            "no-op provider must override default behaviour"
        );
    }

    #[tokio::test]
    async fn default_pool_provider_matches_free_function() {
        let config = DatabaseConfig::default();
        let via_provider = DieselDeadpoolPoolProvider::new()
            .create_pool(&config)
            .await
            .expect("default provider should succeed");
        let via_function = create_pool(&config).expect("free fn should succeed");
        assert_eq!(via_provider.is_none(), via_function.is_none());
    }

    // ── Backend-neutral test targets ─────────────────────────────
    //
    // The pool/topology mechanics below — max_size, the connect-timeout →
    // wait/create mapping, replica retention, the read-pool fallback — are
    // backend-independent, so they must hold on whichever backend
    // `RuntimeConnection` resolves to. `crate::test_urls::primary` spells the
    // target per backend; before it, an inline `postgres://…` made every one
    // of them panic in `build_sqlite_pool` under `--features sqlite`, unseen
    // because no CI lane ran the lib tests there.
    use crate::test_urls::primary as test_primary_url;

    /// A replica target to pair with `primary`.
    ///
    /// Postgres-only, and deliberately so: `database_backend_consistency`
    /// refuses any `replica_url` alongside a `SQLite` primary, so every caller is
    /// `#[cfg(not(feature = "sqlite"))]` and there is no `SQLite` spelling to
    /// give. The `SQLite` side of the replica contract lives in the
    /// `create_topology_*_sqlite_replica` tests.
    #[cfg(not(feature = "sqlite"))]
    fn test_replica_url(_primary: &str, name: &str) -> String {
        format!("postgres://localhost/{name}")
    }

    // ── Pool creation tests ──────────────────────────────────────

    #[tokio::test]
    async fn default_pool_provider_respects_url_config() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("test")),
            ..Default::default()
        };
        let provider = DieselDeadpoolPoolProvider::new();
        let pool = provider
            .create_pool(&config)
            .await
            .expect("default provider should succeed");
        assert!(
            pool.is_some(),
            "default provider should return Some when url is provided"
        );
    }

    #[test]
    fn create_pool_with_no_url_returns_none() {
        let config = DatabaseConfig::default();
        let pool = create_pool(&config).expect("should not fail with no URL");
        assert!(pool.is_none());
    }

    #[test]
    fn create_pool_with_url_returns_some() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("test")),
            ..Default::default()
        };
        let pool = create_pool(&config).expect("should build pool from valid config");
        assert!(pool.is_some());
    }

    // In the default (Postgres) build a SQLite target has no runtime pool, so
    // pool construction must refuse at boot with an actionable message pointing
    // at the `--features sqlite` build — never reaching a first-query failure or
    // panic. Under `--features sqlite` the same target instead builds a real
    // pool (see the `sqlite_boot_serve` integration test), so this refusal
    // contract only applies to the default build.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn create_pool_with_sqlite_url_fails_fast() {
        let config = DatabaseConfig {
            url: Some("sqlite:///var/lib/app.db".into()),
            ..Default::default()
        };
        // The Ok variant (`Option<Pool>`) is not `Debug`, so match rather than
        // use `expect_err`.
        let Err(err) = create_pool(&config) else {
            panic!("sqlite target must refuse at pool build");
        };
        assert!(
            matches!(err, PoolError::UnsupportedBackend(_)),
            "expected UnsupportedBackend, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("SQLite") && msg.contains("--features sqlite"),
            "message must be actionable and name the sqlite build, got: {msg}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn create_topology_with_sqlite_url_fails_fast() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let Err(err) = create_topology(&config) else {
            panic!("sqlite target must refuse at topology build");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
    }

    // Under `--features sqlite` the same targets that refuse above instead build
    // a real pool/topology over `SyncConnectionWrapper<SqliteConnection>`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_pool_with_sqlite_url_builds_under_feature() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let pool = create_pool(&config).expect("sqlite pool builds under the feature");
        assert!(pool.is_some(), "a configured sqlite url yields a pool");
    }

    // A Postgres URL in a sqlite build is a misconfiguration and must refuse.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_pool_with_postgres_url_refuses_under_sqlite_feature() {
        let config = DatabaseConfig {
            url: Some("postgres://localhost/test".into()),
            ..Default::default()
        };
        let Err(err) = create_pool(&config) else {
            panic!("a Postgres url must refuse under the sqlite feature");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
    }

    // The end-to-end contract the helper exists for: no refusal this module
    // produces may carry a password into the boot log.
    #[cfg(feature = "sqlite")]
    #[test]
    fn pool_refusals_never_echo_a_password() {
        for url in [
            "postgres://user:hunter2@localhost/app",
            "mysql://user:hunter2@localhost/app",
            // The secret in the query string rather than the userinfo.
            "mysql://localhost/app?password=hunter2",
            "postgres://localhost/app?sslpassword=hunter2",
            // The libpq keyword/value form: no URL shape at all.
            "host=db user=app password=hunter2",
            // ...and the same, malformed, so neither parser accepts it.
            "host=db user=app password='hunter2",
        ] {
            let config = DatabaseConfig {
                url: Some(url.into()),
                ..Default::default()
            };
            let Err(err) = create_pool(&config) else {
                panic!("a non-sqlite target must refuse: {url}");
            };
            let msg = err.to_string();
            assert!(
                !msg.contains("hunter2"),
                "refusal leaked the password: {msg}"
            );
            assert!(msg.contains("****"), "refusal must show it masked: {msg}");
        }
    }

    // A target that names NEITHER backend must refuse too, not be opened as a
    // file whose name happens to be that string. `normalize_sqlite_target`
    // strips only the `sqlite:`/`sqlite://` schemes and passes anything else
    // through verbatim, so before this guard a `mysql:app.db` URL — or a typo
    // like `sqllite:///app.db` — became a SQLite *filename*. Worse, every other
    // consumer of the URL (`autumn doctor`, the generator's DDL mapping,
    // `autumn migrate`) classifies it through `DatabaseBackend::detect`, which
    // returns `None` for these shapes: the tooling would report "no recognized
    // backend" while the runtime quietly served a database out of a junk file.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_pool_with_unrecognized_url_refuses_under_sqlite_feature() {
        for url in [
            "mysql://localhost/app",
            "mysql:app.db",
            // A near-miss typo of the sqlite scheme.
            "sqllite:///var/lib/app.db",
            // A bare filesystem path: deliberately NOT a recognized target (see
            // `DatabaseBackend`), so it must not silently become one here.
            "/var/lib/app.db",
        ] {
            let config = DatabaseConfig {
                url: Some(url.into()),
                ..Default::default()
            };
            let Err(err) = create_pool(&config) else {
                panic!("an unrecognized url must refuse under the sqlite feature: {url}");
            };
            assert!(
                matches!(err, PoolError::UnsupportedBackend(_)),
                "expected UnsupportedBackend for {url}, got: {err:?}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains("sqlite:") && msg.contains(url),
                "message must name the offending target and the accepted scheme, got: {msg}"
            );
        }
    }

    // …but the two scheme-less spellings `normalize_sqlite_target` explicitly
    // maps to `:memory:` are NOT "unrecognized": `DatabaseBackend::detect`
    // classifies by scheme and these carry none, yet the pool has always taken
    // them, `run_pending_sqlite` lists `:memory:` among its accepted spellings,
    // and `TestApp::with_transactional_db` hands its URL straight to
    // `create_pool`. Refusing them would be a regression, so the guard admits
    // them explicitly. Neither can become a stray file.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_pool_still_accepts_bare_in_memory_spellings() {
        for url in [":memory:", ""] {
            let config = DatabaseConfig {
                url: Some(url.into()),
                ..Default::default()
            };
            let pool = create_pool(&config)
                .unwrap_or_else(|e| panic!("bare in-memory target {url:?} must build: {e}"));
            assert!(
                pool.is_some(),
                "bare in-memory target {url:?} must yield a pool"
            );
            // Private in-memory: every connection is its own database, so the
            // pool must stay single-slot (same rule `build_sqlite_pool` applies
            // to the scheme-spelled forms).
            assert_eq!(
                pool.expect("pool").status().max_size,
                1,
                "bare in-memory target {url:?} must be single-slot"
            );
        }
    }

    // A SQLite runtime has no primary/replica replication in this pool
    // architecture, so a separate replica pool would serve reads from an empty
    // database. `create_topology` must reject an in-memory or distinct-file
    // replica with an actionable boot error (addresses Codex P2).
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_topology_rejects_in_memory_sqlite_replica() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".into()),
            replica_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let Err(err) = create_topology(&config) else {
            panic!("an in-memory sqlite replica must be rejected");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("separate read replica") && msg.contains("primary"),
            "message must be actionable: {msg}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn create_topology_rejects_distinct_file_sqlite_replica() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/primary.db".into()),
            replica_url: Some("sqlite:///var/lib/replica.db".into()),
            ..Default::default()
        };
        let Err(err) = create_topology(&config) else {
            panic!("a distinct-file sqlite replica must be rejected");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
    }

    // A replica that normalizes to the SAME file as the primary is the same
    // database — harmless — so it is allowed and the topology builds.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_topology_allows_same_file_sqlite_replica() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".into()),
            replica_url: Some("sqlite:///var/lib/app.db".into()),
            ..Default::default()
        };
        let topology = create_topology(&config)
            .expect("same-file sqlite replica must be allowed")
            .expect("a configured primary yields a topology");
        assert!(
            topology.replica().is_some(),
            "same-file replica pool should be built"
        );
    }

    // The no-replica happy path still builds a topology fine under the feature.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_topology_sqlite_primary_only_builds() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let topology = create_topology(&config)
            .expect("sqlite primary-only topology builds")
            .expect("a configured primary yields a topology");
        assert!(topology.replica().is_none(), "no replica configured");
    }

    // A custom provider that overrides ONLY `create_pool` still has its replica
    // built by the trait DEFAULT `create_topology`. That default path must apply
    // the same SQLite-replica rule as the free `create_topology` /
    // `create_shard_topology`, or a provider could boot a distinct/in-memory
    // replica and route reads to an unrelated, empty database (addresses Codex
    // P2). This minimal provider delegates `create_pool` to the default factory
    // and leaves `create_topology` as the trait default — exactly the path under
    // test.
    #[cfg(feature = "sqlite")]
    struct PrimaryOnlyProvider;

    #[cfg(feature = "sqlite")]
    impl DatabasePoolProvider for PrimaryOnlyProvider {
        async fn create_pool(
            &self,
            config: &DatabaseConfig,
        ) -> Result<Option<Pool<RuntimeConnection>>, PoolError> {
            create_pool(config)
        }
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn default_provider_topology_rejects_distinct_file_sqlite_replica() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/primary.db".into()),
            replica_url: Some("sqlite:///var/lib/replica.db".into()),
            ..Default::default()
        };
        let Err(err) = PrimaryOnlyProvider.create_topology(&config).await else {
            panic!("the default-topology path must reject a distinct-file sqlite replica");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("separate read replica") && msg.contains("primary"),
            "message must be actionable: {msg}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn default_provider_topology_rejects_in_memory_sqlite_replica() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".into()),
            replica_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let Err(err) = PrimaryOnlyProvider.create_topology(&config).await else {
            panic!("an in-memory sqlite replica must be rejected by the default topology");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
    }

    // A same-file replica is the same database (harmless) and still builds
    // through the default topology; a primary-only config builds with no replica.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn default_provider_topology_allows_same_file_and_primary_only() {
        let same_file = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".into()),
            replica_url: Some("sqlite:///var/lib/app.db".into()),
            ..Default::default()
        };
        let topology = PrimaryOnlyProvider
            .create_topology(&same_file)
            .await
            .expect("same-file replica must be allowed by the default topology")
            .expect("a configured primary yields a topology");
        assert!(
            topology.replica().is_some(),
            "same-file replica pool should be built"
        );

        let primary_only = DatabaseConfig {
            primary_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let topology = PrimaryOnlyProvider
            .create_topology(&primary_only)
            .await
            .expect("primary-only default topology builds")
            .expect("a configured primary yields a topology");
        assert!(topology.replica().is_none(), "no replica configured");
    }

    // The per-shard replica is subject to the SAME SQLite-replica rule as the
    // control database (addresses Codex P2): an in-memory or distinct-file shard
    // replica would route reads to an unrelated, empty database, so it must be
    // rejected; a shard with only a primary (or a same-file replica) must build.
    #[cfg(feature = "sqlite")]
    #[test]
    fn create_shard_topology_rejects_in_memory_sqlite_replica() {
        let defaults = DatabaseConfig::default();
        let shard = crate::config::ShardConfig {
            name: "shard0".into(),
            primary_url: "sqlite:///var/lib/shard0.db".into(),
            replica_url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        let Err(err) = create_shard_topology(&shard, &defaults) else {
            panic!("an in-memory sqlite shard replica must be rejected");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("separate read replica") && msg.contains("primary"),
            "message must be actionable: {msg}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn create_shard_topology_rejects_distinct_file_sqlite_replica() {
        let defaults = DatabaseConfig::default();
        let shard = crate::config::ShardConfig {
            name: "shard0".into(),
            primary_url: "sqlite:///var/lib/shard0-primary.db".into(),
            replica_url: Some("sqlite:///var/lib/shard0-replica.db".into()),
            ..Default::default()
        };
        let Err(err) = create_shard_topology(&shard, &defaults) else {
            panic!("a distinct-file sqlite shard replica must be rejected");
        };
        assert!(matches!(err, PoolError::UnsupportedBackend(_)), "{err:?}");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn create_shard_topology_allows_same_file_sqlite_replica() {
        let defaults = DatabaseConfig::default();
        let shard = crate::config::ShardConfig {
            name: "shard0".into(),
            primary_url: "sqlite:///var/lib/shard0.db".into(),
            replica_url: Some("sqlite:///var/lib/shard0.db".into()),
            ..Default::default()
        };
        let topology = create_shard_topology(&shard, &defaults)
            .expect("same-file sqlite shard replica must be allowed");
        assert!(
            topology.replica().is_some(),
            "same-file shard replica pool should be built"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn create_shard_topology_sqlite_primary_only_builds() {
        let defaults = DatabaseConfig::default();
        let shard = crate::config::ShardConfig {
            name: "shard0".into(),
            primary_url: "sqlite::memory:".into(),
            ..Default::default()
        };
        let topology = create_shard_topology(&shard, &defaults)
            .expect("sqlite primary-only shard topology builds");
        assert!(topology.replica().is_none(), "no shard replica configured");
    }

    // `file::memory:` (and its query-string form) is a private in-memory target
    // and must be forced single-slot; a shared-cache in-memory DB is shareable
    // and must NOT be (addresses Codex P1).
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_target_is_memory_covers_file_memory() {
        assert!(sqlite_target_is_memory(":memory:"));
        assert!(sqlite_target_is_memory("file::memory:"));
        assert!(sqlite_target_is_memory("file::memory:?foo=bar"));
        assert!(sqlite_target_is_memory("file:app?mode=memory"));
        // Shared-cache in-memory databases are shareable across connections.
        assert!(!sqlite_target_is_memory("file::memory:?cache=shared"));
        assert!(!sqlite_target_is_memory(
            "file:app?mode=memory&cache=shared"
        ));
        // Plain file targets are not in-memory.
        assert!(!sqlite_target_is_memory("/var/lib/app.db"));
    }

    // `sqlite_target_is_any_in_memory` is the broader predicate the
    // startup-migration reject uses: it classifies EVERY in-memory spelling —
    // private AND shared-cache — as in-memory, because none of them survive the
    // transient migration connection closing to reach the runtime pool (issue
    // #1614 follow-up). It must diverge from `sqlite_target_is_memory` (pool
    // sizing) precisely on `cache=shared`, which sizing keeps NOT-in-memory so a
    // shared-cache DB is never forced single-slot.
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_target_is_any_in_memory_covers_shared_cache() {
        // Private in-memory spellings — in-memory for BOTH predicates.
        assert!(sqlite_target_is_any_in_memory(":memory:"));
        assert!(sqlite_target_is_any_in_memory("sqlite::memory:"));
        assert!(sqlite_target_is_any_in_memory("sqlite://:memory:"));
        assert!(sqlite_target_is_any_in_memory("sqlite://"));
        assert!(sqlite_target_is_any_in_memory("file::memory:"));
        assert!(sqlite_target_is_any_in_memory("file::memory:?foo=bar"));
        // Shared-cache in-memory: `any_in_memory` → true (rejected for
        // migrations), but pool sizing's `sqlite_target_is_memory` → false (NOT
        // forced single-slot). This divergence is the whole point.
        assert!(sqlite_target_is_any_in_memory("file::memory:?cache=shared"));
        assert!(sqlite_target_is_any_in_memory(
            "file:app?mode=memory&cache=shared"
        ));
        assert!(!sqlite_target_is_memory("file::memory:?cache=shared"));
        assert!(!sqlite_target_is_memory(
            "file:app?mode=memory&cache=shared"
        ));
        // File-backed targets are never in-memory under either predicate.
        assert!(!sqlite_target_is_any_in_memory("sqlite:///var/lib/app.db"));
        assert!(!sqlite_target_is_any_in_memory("/var/lib/app.db"));
        assert!(!sqlite_target_is_any_in_memory("file:/var/lib/app.db"));
    }

    // The transient SQLite migration connection must carry `PRAGMA busy_timeout`
    // (mirroring the runtime pool's per-connection setup) so a concurrent
    // migrator or a briefly-held write lock WAITS instead of failing immediately
    // with SQLITE_BUSY and aborting `auto_migrate_sqlite` (Codex P1). Proof: a
    // fresh migration connection reports the configured non-zero timeout.
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_migration_connection_sets_busy_timeout() {
        use diesel::RunQueryDsl as _;

        #[derive(diesel::QueryableByName)]
        struct BusyTimeout {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            timeout: i32,
        }

        // A tempfile-backed target (not `:memory:`) so the connection maps to a
        // real database the pragma read reflects.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("migration.db");
        let url = format!("sqlite://{}", db_path.display());

        let mut conn = super::establish_sqlite_migration_connection(&url)
            .expect("establish sqlite migration connection");
        let rows: Vec<BusyTimeout> = diesel::sql_query("PRAGMA busy_timeout")
            .load(&mut conn)
            .expect("read busy_timeout pragma");
        let timeout = rows
            .into_iter()
            .next()
            .expect("busy_timeout pragma returns a row")
            .timeout;
        assert_eq!(
            timeout, 5000,
            "the migration connection must carry the configured busy_timeout (ms) \
             so concurrent/locked migrators wait rather than hitting SQLITE_BUSY"
        );
    }

    // A read-only URI target (`mode=ro` / `immutable`) must be detected so the
    // per-connection setup batch skips the write-affecting `journal_mode = WAL`
    // pragma, which otherwise fails with "attempt to write a readonly database"
    // and 503s the whole pool (Codex P2).
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_target_is_read_only_detects_ro_and_immutable() {
        assert!(sqlite_target_is_read_only("file:/srv/ref.db?mode=ro"));
        // Case-insensitive key/value.
        assert!(sqlite_target_is_read_only("file:/srv/ref.db?MODE=RO"));
        assert!(sqlite_target_is_read_only("file:/srv/ref.db?immutable=1"));
        assert!(sqlite_target_is_read_only(
            "file:/srv/ref.db?immutable=true"
        ));
        assert!(sqlite_target_is_read_only(
            "file:/srv/ref.db?immutable=TRUE"
        ));
        // Read-only marker alongside other params.
        assert!(sqlite_target_is_read_only(
            "file:/srv/ref.db?cache=shared&mode=ro"
        ));

        // A plain file, an in-memory target, and a writable/shared-cache target
        // are NOT read-only.
        assert!(!sqlite_target_is_read_only("/var/lib/app.db"));
        assert!(!sqlite_target_is_read_only(":memory:"));
        assert!(!sqlite_target_is_read_only("file::memory:?cache=shared"));
        assert!(!sqlite_target_is_read_only("file:app?mode=memory"));
        assert!(!sqlite_target_is_read_only("file:/srv/ref.db?mode=rwc"));
        assert!(!sqlite_target_is_read_only("file:/srv/ref.db?immutable=0"));
        // A value that merely CONTAINS `ro` must not trip the substring trap.
        assert!(!sqlite_target_is_read_only("file:/srv/ro-data.db"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn file_memory_sqlite_pool_is_single_slot() {
        let config = DatabaseConfig {
            primary_url: Some("file::memory:".into()),
            pool_size: 5,
            ..Default::default()
        };
        let pool = create_pool(&config)
            .expect("file::memory: sqlite pool builds")
            .expect("a configured url yields a pool");
        assert_eq!(
            pool.status().max_size,
            1,
            "a private in-memory target must be forced single-slot"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn shared_cache_in_memory_sqlite_pool_is_not_forced_single_slot() {
        let config = DatabaseConfig {
            primary_url: Some("file::memory:?cache=shared".into()),
            pool_size: 5,
            ..Default::default()
        };
        let pool = create_pool(&config)
            .expect("shared-cache in-memory sqlite pool builds")
            .expect("a configured url yields a pool");
        assert_eq!(
            pool.status().max_size,
            5,
            "a shared-cache in-memory target must respect the configured size"
        );
    }

    // The runtime landed (#1905); the tree described it as pending for another
    // release. Each phrase below is one a reader takes as the current contract,
    // and each was false under `--features sqlite`. This is a ratchet, not a
    // style rule: a "planned"/"not yet wired" claim about a SUBSYSTEM (sessions
    // #1908, jobs #1907, backup #1909) is still accurate and is not matched.
    #[test]
    fn no_source_doc_still_calls_the_sqlite_runtime_unwired() {
        let sources = [
            ("autumn/src/db.rs", include_str!("db.rs")),
            ("autumn/src/config.rs", include_str!("config.rs")),
            ("autumn/Cargo.toml", include_str!("../Cargo.toml")),
            (
                "docs/guide/sqlite-in-production.md",
                include_str!("../../docs/guide/sqlite-in-production.md"),
            ),
        ];
        // Split, because this file is one of the files scanned: a phrase
        // written whole here would be a finding against itself.
        let stale = [
            ["until the pool", "rework lands"].join(" "),
            ["the runtime pool", "is not yet wired"].join(" "),
            ["recognized-but", "not-yet-wired"].join("-"),
            ["PR1 is", "a no-op refactor"].join(" "),
            ["Update (this", "release)"].join(" "),
        ];
        for (name, text) in sources {
            for phrase in &stale {
                assert!(
                    !text.contains(phrase.as_str()),
                    "{name} still says {phrase:?}: the SQLite runtime is wired"
                );
            }
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn normalize_sqlite_target_strips_schemes() {
        assert_eq!(normalize_sqlite_target("sqlite::memory:"), ":memory:");
        assert_eq!(normalize_sqlite_target("sqlite://:memory:"), ":memory:");
        assert_eq!(normalize_sqlite_target("sqlite://"), ":memory:");
        assert_eq!(
            normalize_sqlite_target("sqlite:///var/lib/app.db"),
            "/var/lib/app.db"
        );
        assert_eq!(normalize_sqlite_target("sqlite:app.db"), "app.db");
        assert_eq!(
            normalize_sqlite_target("file:/var/lib/app.db"),
            "file:/var/lib/app.db"
        );
    }

    #[test]
    fn pool_respects_max_size() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("test")),
            pool_size: 5,
            ..Default::default()
        };
        let pool = create_pool(&config)
            .expect("should build pool")
            .expect("should be Some");
        assert_eq!(pool.status().max_size, 5);
    }

    #[test]
    fn pool_clamps_size_to_one_if_zero() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("test")),
            pool_size: 0,
            ..Default::default()
        };
        let pool = create_pool(&config)
            .expect("should build pool")
            .expect("should be Some");
        assert_eq!(
            pool.status().max_size,
            1,
            "Pool size should be clamped to 1"
        );
    }

    // ── Db extractor tests ───────────────────────────────────────

    // Postgres-only: `database_backend_consistency` refuses ANY `replica_url`
    // alongside a SQLite primary ("read replicas require the postgres
    // backend"), so a two-pool topology is unreachable under the backend flip.
    // Running this under `sqlite` would only assert pool mechanics for a
    // configuration no validated deployment can have; the SQLite side of the
    // replica contract is asserted by `create_topology_rejects_*_sqlite_replica`
    // and `create_topology_allows_same_file_sqlite_replica` instead.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn database_topology_builds_primary_and_replica_pools() {
        let primary = test_primary_url("primary");
        let config = DatabaseConfig {
            primary_url: Some(primary.clone()),
            replica_url: Some(test_replica_url(&primary, "replica")),
            primary_pool_size: Some(6),
            replica_pool_size: Some(2),
            ..Default::default()
        };

        let topology = create_topology(&config)
            .expect("topology should build")
            .expect("topology should be configured");

        assert_eq!(topology.primary().status().max_size, 6);
        assert_eq!(
            topology.replica().expect("replica pool").status().max_size,
            2
        );
        assert_eq!(topology.read().status().max_size, 2);
    }

    #[test]
    fn database_topology_single_url_builds_only_primary_pool() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("single")),
            pool_size: 5,
            ..Default::default()
        };

        let topology = create_topology(&config)
            .expect("topology should build")
            .expect("topology should be configured");

        assert_eq!(topology.primary().status().max_size, 5);
        assert!(topology.replica().is_none());
        assert_eq!(topology.read().status().max_size, 5);
    }

    #[test]
    fn config_runtime_drift_pool_applies_connect_timeout_to_wait_and_create() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("test")),
            connect_timeout_secs: 7,
            ..Default::default()
        };
        let pool = create_pool(&config)
            .expect("should build pool")
            .expect("should be Some");

        let timeouts = pool.timeouts();
        assert_eq!(timeouts.wait, Some(Duration::from_secs(7)));
        assert_eq!(timeouts.create, Some(Duration::from_secs(7)));
    }

    #[derive(Clone)]
    struct TestDbState;

    impl DbState for TestDbState {
        fn pool(&self) -> Option<&Pool<crate::db::RuntimeConnection>> {
            None
        }
    }

    #[derive(Clone)]
    struct TestReadState {
        primary: Pool<crate::db::RuntimeConnection>,
    }

    impl DbState for TestReadState {
        fn pool(&self) -> Option<&Pool<crate::db::RuntimeConnection>> {
            Some(&self.primary)
        }
    }

    #[test]
    fn database_topology_read_pool_falls_back_to_primary() {
        let config = DatabaseConfig {
            url: Some(test_primary_url("read-fallback")),
            pool_size: 3,
            ..Default::default()
        };
        let primary = create_pool(&config).unwrap().unwrap();
        let state = TestReadState { primary };

        assert_eq!(state.read_pool().expect("read pool").status().max_size, 3);
    }

    #[tokio::test]
    async fn db_extractor_rejects_when_no_pool() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use axum::routing::get;
        use tower::ServiceExt;

        async fn handler(_db: Db) -> &'static str {
            "ok"
        }

        let app = Router::new()
            .route("/", get(handler))
            .with_state(TestDbState);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn database_topology_primary_only_has_no_replica() {
        let config = DatabaseConfig {
            primary_url: Some(test_primary_url("db")),
            ..DatabaseConfig::default()
        };
        let topology = create_topology(&config).unwrap().unwrap();

        let primary = topology.primary().clone();

        let new_topology = DatabaseTopology::primary_only(primary);
        assert!(
            new_topology.replica().is_none(),
            "primary_only must set replica to None"
        );
    }

    // Postgres-only: `database_backend_consistency` refuses ANY `replica_url`
    // alongside a SQLite primary ("read replicas require the postgres
    // backend"), so a two-pool topology is unreachable under the backend flip.
    // Running this under `sqlite` would only assert pool mechanics for a
    // configuration no validated deployment can have; the SQLite side of the
    // replica contract is asserted by `create_topology_rejects_*_sqlite_replica`
    // and `create_topology_allows_same_file_sqlite_replica` instead.
    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn database_topology_from_pools_retains_replica() {
        let primary = test_primary_url("primary");
        let config = DatabaseConfig {
            primary_url: Some(primary.clone()),
            replica_url: Some(test_replica_url(&primary, "db_replica")),
            ..DatabaseConfig::default()
        };
        let topology = create_topology(&config).unwrap().unwrap();

        let primary = topology.primary().clone();
        let replica = topology.replica().cloned();

        let new_topology = DatabaseTopology::from_pools(primary, replica);
        assert!(
            new_topology.replica().is_some(),
            "from_pools must preserve the replica pool"
        );
    }

    // ── scrub_sql tests ───────────────────────────────────────────────────────

    #[test]
    fn scrub_sql_strips_string_literals() {
        assert_eq!(
            super::scrub_sql("SELECT * FROM users WHERE name = 'Alice'"),
            "SELECT * FROM users WHERE name = '?'"
        );
    }

    #[test]
    fn scrub_sql_strips_numeric_literals() {
        assert_eq!(
            super::scrub_sql("SELECT * FROM orders WHERE id = 42"),
            "SELECT * FROM orders WHERE id = ?"
        );
    }

    #[test]
    fn scrub_sql_preserves_pg_positional_params() {
        assert_eq!(
            super::scrub_sql("SELECT * FROM t WHERE x = $1 AND y = $2"),
            "SELECT * FROM t WHERE x = $1 AND y = $2"
        );
    }

    #[test]
    fn scrub_sql_does_not_stomp_identifiers() {
        // "table1" should not be replaced because it's not preceded by a separator
        assert_eq!(
            super::scrub_sql("SELECT * FROM table1 WHERE active = true"),
            "SELECT * FROM table1 WHERE active = true"
        );
    }

    #[test]
    fn scrub_sql_multiple_literals_in_one_query() {
        assert_eq!(
            super::scrub_sql("INSERT INTO users (name, age) VALUES ('Bob', 30)"),
            "INSERT INTO users (name, age) VALUES ('?', ?)"
        );
    }

    #[test]
    fn scrub_sql_handles_escaped_single_quotes() {
        assert_eq!(
            super::scrub_sql("SELECT * FROM t WHERE s = 'it''s a test'"),
            "SELECT * FROM t WHERE s = '?'"
        );
    }

    #[test]
    fn scrub_sql_empty_string() {
        assert_eq!(super::scrub_sql(""), "");
    }

    // ── Bug fixes: exponent suffix, dollar-quoted strings, E-string escapes ──

    #[test]
    fn scrub_sql_scientific_notation_integer_exponent() {
        // 1e6 should be fully redacted to ? (the "e6" part is the exponent)
        assert_eq!(
            super::scrub_sql("SELECT * FROM t WHERE n = 1e6"),
            "SELECT * FROM t WHERE n = ?"
        );
    }

    #[test]
    fn scrub_sql_scientific_notation_float_exponent() {
        // 2.5E-4 should be fully redacted: digit + decimal + E + sign + digit(s)
        assert_eq!(
            super::scrub_sql("SELECT * FROM t WHERE n = 2.5E-4"),
            "SELECT * FROM t WHERE n = ?"
        );
    }

    #[test]
    fn scrub_sql_scientific_notation_uppercase_positive_exponent() {
        // 3E+10 — uppercase E with explicit + sign
        assert_eq!(
            super::scrub_sql("SELECT * FROM t WHERE n = 3E+10"),
            "SELECT * FROM t WHERE n = ?"
        );
    }

    #[test]
    fn scrub_sql_dollar_quoted_anonymous() {
        // $$...$$ dollar-quoted string: content must be fully redacted
        assert_eq!(super::scrub_sql("SELECT $$secret value$$"), "SELECT '?'");
    }

    #[test]
    fn scrub_sql_dollar_quoted_with_tag() {
        // $tag$...$tag$ — tagged dollar-quoted string
        assert_eq!(
            super::scrub_sql("SELECT $body$hello world$body$"),
            "SELECT '?'"
        );
    }

    #[test]
    fn scrub_sql_dollar_quoted_does_not_affect_positional_params() {
        // $1, $2 positional params must still pass through unmodified
        assert_eq!(
            super::scrub_sql("SELECT $1, $2 FROM $$secret$$ WHERE id = $3"),
            "SELECT $1, $2 FROM '?' WHERE id = $3"
        );
    }

    #[test]
    fn scrub_sql_estring_backslash_escaped_quote() {
        // E'it\'s secret' — backslash-escaped quote inside E'' string
        assert_eq!(
            super::scrub_sql(r"SELECT E'it\'s secret' FROM t"),
            "SELECT '?' FROM t"
        );
    }

    #[test]
    fn scrub_sql_estring_uppercase() {
        // Uppercase E prefix variant E'...'
        assert_eq!(
            super::scrub_sql("SELECT E'hello world' FROM t"),
            "SELECT '?' FROM t"
        );
    }

    #[test]
    fn scrub_sql_estring_multiple_backslash_escapes() {
        // Multiple backslash sequences inside one E'' literal
        assert_eq!(
            super::scrub_sql(r"SELECT E'line1\nline2' FROM t"),
            "SELECT '?' FROM t"
        );
    }

    #[test]
    fn scrub_sql_leading_dot_numeric_literals() {
        assert_eq!(super::scrub_sql("SELECT .5"), "SELECT ?");
        assert_eq!(super::scrub_sql("SELECT .25 + .75"), "SELECT ? + ?");
        assert_eq!(super::scrub_sql("SELECT t.col"), "SELECT t.col");
        assert_eq!(
            super::scrub_sql("SELECT schema.table.col"),
            "SELECT schema.table.col"
        );
    }

    #[test]
    fn scrub_sql_estring_doubled_quote_escape() {
        assert_eq!(
            super::scrub_sql("SELECT E'it''s secret' FROM t"),
            "SELECT '?' FROM t"
        );
    }

    #[test]
    fn scrub_sql_numeric_literal_underscore_grouping() {
        assert_eq!(super::scrub_sql("SELECT 5_432_000"), "SELECT ?");
        assert_eq!(super::scrub_sql("SELECT 1_000.5_0"), "SELECT ?");
        assert_eq!(super::scrub_sql("SELECT col_5_val"), "SELECT col_5_val");
        assert_eq!(super::scrub_sql("SELECT col_5"), "SELECT col_5");
    }

    // ── Transaction isolation + retry (issue #1202) ─────────────────

    #[derive(Debug)]
    struct FakeDbErrorInfo {
        message: &'static str,
    }

    impl diesel::result::DatabaseErrorInformation for FakeDbErrorInfo {
        fn message(&self) -> &str {
            self.message
        }
        fn details(&self) -> Option<&str> {
            None
        }
        fn hint(&self) -> Option<&str> {
            None
        }
        fn table_name(&self) -> Option<&str> {
            None
        }
        fn column_name(&self) -> Option<&str> {
            None
        }
        fn constraint_name(&self) -> Option<&str> {
            None
        }
        fn statement_position(&self) -> Option<i32> {
            None
        }
    }

    fn diesel_db_error(
        kind: diesel::result::DatabaseErrorKind,
        message: &'static str,
    ) -> diesel::result::Error {
        diesel::result::Error::DatabaseError(kind, Box::new(FakeDbErrorInfo { message }))
    }

    // ── TxOptions builder ────────────────────────────────────────

    #[test]
    fn tx_options_default_is_read_committed_no_retry() {
        let opts = TxOptions::default();
        assert_eq!(opts.isolation, IsolationLevel::ReadCommitted);
        assert!(!opts.read_only);
        assert!(!opts.deferrable);
        assert_eq!(
            opts.max_attempts, 1,
            "default must behave exactly like today's tx(): one attempt, no retry"
        );
    }

    #[test]
    fn tx_options_serializable_enables_retry() {
        let opts = TxOptions::serializable();
        assert_eq!(opts.isolation, IsolationLevel::Serializable);
        assert!(
            opts.max_attempts > 1,
            "serializable() should default to retrying since retry is the point"
        );
    }

    #[test]
    fn tx_options_repeatable_read_enables_retry() {
        let opts = TxOptions::repeatable_read();
        assert_eq!(opts.isolation, IsolationLevel::RepeatableRead);
        assert!(opts.max_attempts > 1);
    }

    #[test]
    fn tx_options_builder_chains() {
        let opts = TxOptions::serializable()
            .read_only()
            .deferrable()
            .max_attempts(9)
            .initial_backoff(Duration::from_millis(3))
            .max_backoff(Duration::from_millis(30));
        assert!(opts.read_only);
        assert!(opts.deferrable);
        assert_eq!(opts.max_attempts, 9);
        assert_eq!(opts.initial_backoff, Duration::from_millis(3));
        assert_eq!(opts.max_backoff, Duration::from_millis(30));
    }

    #[test]
    fn tx_options_max_attempts_clamps_to_at_least_one() {
        // A zero here must never produce a loop that skips the closure entirely.
        assert_eq!(TxOptions::default().max_attempts(0).max_attempts, 1);
    }

    #[test]
    fn tx_options_effective_max_attempts_clamps_struct_literal_bypass() {
        // `max_attempts` is a public field, so struct-literal update syntax can
        // set it to 0 directly, bypassing the `max_attempts()` builder's clamp.
        // The retry loop must still treat this as "run once", not "never run".
        let opts = TxOptions {
            max_attempts: 0,
            ..TxOptions::default()
        };
        assert_eq!(opts.max_attempts, 0, "the raw field is not itself clamped");
        assert_eq!(
            opts.effective_max_attempts(),
            1,
            "the retry loop's accessor must clamp a struct-literal 0 to 1"
        );
    }

    // ── Backoff ──────────────────────────────────────────────────

    #[test]
    fn retry_backoff_base_doubles_per_attempt() {
        let initial = Duration::from_millis(5);
        let max = Duration::from_secs(60);
        assert_eq!(
            retry_backoff_base(initial, max, 1),
            Duration::from_millis(5)
        );
        assert_eq!(
            retry_backoff_base(initial, max, 2),
            Duration::from_millis(10)
        );
        assert_eq!(
            retry_backoff_base(initial, max, 3),
            Duration::from_millis(20)
        );
        assert_eq!(
            retry_backoff_base(initial, max, 4),
            Duration::from_millis(40)
        );
    }

    #[test]
    fn retry_backoff_base_caps_at_max() {
        let initial = Duration::from_millis(5);
        let max = Duration::from_millis(50);
        // 5,10,20,40 then capped at 50 for all higher attempts.
        assert_eq!(
            retry_backoff_base(initial, max, 5),
            Duration::from_millis(50)
        );
        assert_eq!(retry_backoff_base(initial, max, 40), max);
        // Huge attempt must not panic on shift overflow.
        assert_eq!(retry_backoff_base(initial, max, u32::MAX), max);
    }

    #[test]
    fn retry_backoff_delay_stays_within_jitter_bounds_and_cap() {
        let initial = Duration::from_millis(100);
        let max = Duration::from_secs(10);
        for attempt in 1..=6u32 {
            let base = retry_backoff_base(initial, max, attempt);
            for _ in 0..64 {
                let d = retry_backoff_delay(initial, max, attempt);
                // Jitter is +/-20% of base, never exceeding the cap.
                assert!(
                    d >= base.mul_f64(0.8) && d <= base.mul_f64(1.2),
                    "delay {d:?} out of jitter bounds for base {base:?}"
                );
                assert!(d <= max, "delay {d:?} exceeded cap {max:?}");
            }
        }
    }

    // ── Retryable-error classification ───────────────────────────

    #[test]
    fn serialization_failure_is_retryable() {
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::SerializationFailure,
            "could not serialize access due to read/write dependencies among transactions",
        ));
        assert!(is_retryable_txn_error(&err));
    }

    #[test]
    fn deadlock_is_retryable() {
        // Postgres 40P01 is not mapped to a dedicated DatabaseErrorKind, so it
        // is caught via an exact match against Postgres's own invariant
        // primary message for this condition.
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::Unknown,
            "deadlock detected",
        ));
        assert!(is_retryable_txn_error(&err));
    }

    #[test]
    fn deadlock_message_is_matched_case_and_whitespace_insensitively() {
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::Unknown,
            "  Deadlock Detected  ",
        ));
        assert!(is_retryable_txn_error(&err));
    }

    #[test]
    fn unknown_kind_message_merely_mentioning_deadlock_is_not_retryable() {
        // Regression (PR #1581 review, round 2): an `Unknown`-kind Postgres
        // error from application SQL (e.g. `RAISE EXCEPTION`) whose message
        // merely *mentions* "deadlock detected" or "40001" as part of a
        // longer business message must not be retried -- only an exact match
        // against Postgres's own invariant primary message counts.
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::Unknown,
            "order 40001 could not be processed: deadlock detected in workflow",
        ));
        assert!(!is_retryable_txn_error(&err));
    }

    #[test]
    fn unknown_kind_40001_text_alone_is_not_retryable() {
        // A bare "40001" in an Unknown-kind message is no longer treated as a
        // retry signal -- genuine 40001s are already reliably classified via
        // the DatabaseErrorKind::SerializationFailure fast path (diesel-async
        // maps the real SQLSTATE), so this text pattern is dead weight that
        // only added false-positive risk.
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::Unknown,
            "ERROR: 40001: could not serialize access",
        ));
        assert!(!is_retryable_txn_error(&err));
    }

    #[test]
    fn unique_violation_is_not_retryable() {
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            "duplicate key value violates unique constraint",
        ));
        assert!(!is_retryable_txn_error(&err));
    }

    #[test]
    fn unique_violation_with_magic_substring_in_message_is_not_retryable() {
        // Regression: a non-retryable `DatabaseErrorKind` must never be
        // misclassified just because its message/constraint text happens to
        // contain a magic substring like "40001" or "deadlock detected".
        let err: AutumnError = AutumnError::internal_server_error(diesel_db_error(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            "duplicate key value violates unique constraint \"orders_40001_key\"",
        ));
        assert!(!is_retryable_txn_error(&err));
    }

    #[test]
    fn plain_internal_error_is_not_retryable() {
        let err = AutumnError::internal_server_error_msg("something unrelated broke");
        assert!(!is_retryable_txn_error(&err));
    }

    #[test]
    fn domain_error_with_magic_substring_and_no_db_error_is_not_retryable() {
        // Regression (PR #1581 review): a plain domain/validation error — no
        // diesel::result::Error or tokio_postgres error anywhere in its
        // chain — must never be retried just because its message happens to
        // contain a magic substring like "40001" or "deadlock detected".
        // Retry requires a structured signal, never bare text.
        let err = AutumnError::bad_request_msg(
            "order 40001 could not be processed: deadlock detected in workflow",
        );
        assert!(!is_retryable_txn_error(&err));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("app error")]
    struct WrappedDbError(#[source] diesel::result::Error);

    #[test]
    fn serialization_failure_wrapped_in_custom_error_is_retryable() {
        // A custom `E` (e.g. an app error enum with a `#[from] diesel::result::Error`
        // variant) is not itself a `diesel::result::Error`, so it isn't found by
        // a top-level-only downcast. `downcast_chain_ref` walks `source()` to
        // find it, so wrapped diesel errors are still classified structurally
        // — no need to fall back to message scanning for this common case.
        let err: AutumnError = AutumnError::internal_server_error(WrappedDbError(diesel_db_error(
            diesel::result::DatabaseErrorKind::SerializationFailure,
            "could not serialize access due to read/write dependencies among transactions",
        )));
        assert!(is_retryable_txn_error(&err));
    }

    #[test]
    fn unique_violation_wrapped_in_custom_error_is_not_retryable() {
        let err: AutumnError = AutumnError::internal_server_error(WrappedDbError(diesel_db_error(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            "duplicate key value violates unique constraint",
        )));
        assert!(!is_retryable_txn_error(&err));
    }

    // ── Attempt accounting ───────────────────────────────────────

    #[test]
    fn retry_decision_retries_until_attempts_exhausted() {
        // A retryable error keeps retrying while attempts remain, then stops.
        let max = 3;
        assert_eq!(retry_decision(1, max, true), RetryDecision::Retry);
        assert_eq!(retry_decision(2, max, true), RetryDecision::Retry);
        assert_eq!(
            retry_decision(3, max, true),
            RetryDecision::Stop,
            "the final attempt must stop even when the error is retryable (exhausted)"
        );
    }

    #[test]
    fn retry_decision_stops_immediately_on_non_retryable() {
        assert_eq!(retry_decision(1, 5, false), RetryDecision::Stop);
    }

    #[test]
    fn retry_decision_single_attempt_never_retries() {
        // max_attempts == 1 (the default) must behave exactly like today's tx().
        assert_eq!(retry_decision(1, 1, true), RetryDecision::Stop);
    }
}

// ── Postgres TLS (sslmode) support ───────────────────────────────────────────

/// TLS support for the Postgres pool, driven by `sslmode` in the database URL.
///
/// diesel-async's default `AsyncPgConnection::establish` hardcodes
/// `tokio_postgres::NoTls`, which makes `sslmode=require` fail on every
/// connection with "no TLS implementation configured". This module plugs a
/// rustls-backed connector into the pool via
/// [`diesel_async::pooled_connection::ManagerConfig::custom_setup`] when the
/// URL asks for TLS. Both URL (`postgres://…?sslmode=require`) and
/// keyword/value (`host=… sslmode=require`) connection strings are
/// recognized. The synchronous migration/startup-wait path shares the same
/// connector through [`super::establish_migration_connection`] — the
/// bundled libpq has no SSL support, so the native `PgConnection` path
/// cannot reach TLS-only servers at all.
///
/// | `sslmode`                     | behavior |
/// | ----------------------------- | -------- |
/// | absent, `disable`, `prefer`   | unchanged: the default `NoTls` path (plaintext), exactly as before this module existed |
/// | `require`                     | TLS. The connection is encrypted and the handshake signatures are verified, but the server's certificate **chain/identity is not** — matching what `libpq`/`psql` do for `require` (self-signed and private-CA servers work) |
/// | `verify-full`                 | TLS with full chain **and** hostname verification against the Mozilla root store (`webpki-roots`), plus any `sslrootcert=<PEM file>` from the URL |
/// | `verify-ca`                   | rejected with guidance: chain-only verification is not implemented; use `verify-full` (stricter) or `require` |
///
/// `require` combined with `sslrootcert` is also rejected rather than
/// silently ignoring the CA file: `libpq` documents that combination as
/// upgrading to certificate verification, so dropping the file would
/// silently weaken what the operator asked for.
// `pub(crate)` so the failure-capsule recording pool can reuse the same
// `sslmode` classification when deciding whether a database URL can be teed
// (#1598) instead of re-implementing the parse.
pub(crate) mod tls {
    use std::sync::Arc;

    use diesel::{ConnectionError, ConnectionResult};
    use diesel_async::pooled_connection::SetupCallback;
    use diesel_async::{AsyncConnection as _, AsyncPgConnection};
    use futures::FutureExt as _;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use tokio_postgres_rustls::MakeRustlsConnect;

    /// TLS posture derived from the connection string's `sslmode` (and
    /// `sslrootcert`). See the [module docs](self) for the full table.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TlsPosture {
        /// No TLS machinery: keep diesel-async's default `NoTls` setup path.
        Off,
        /// Encrypt without verifying the server certificate chain
        /// (`libpq` parity for `sslmode=require`).
        Require,
        /// Encrypt and fully verify the certificate chain + hostname.
        VerifyFull {
            /// Optional `sslrootcert` PEM file to trust in addition to the
            /// Mozilla root store.
            root_cert: Option<String>,
        },
        /// A recognized-but-unsupported combination; every connection attempt
        /// fails loudly with this reason instead of silently downgrading.
        Unsupported { reason: String },
    }

    impl TlsPosture {
        /// Classify a database URL / keyword-value connection string.
        pub fn from_database_url(database_url: &str) -> Self {
            let params = ssl_params(database_url);
            // Last occurrence wins, matching libpq/tokio-postgres semantics.
            let get = |key: &str| {
                params
                    .iter()
                    .rev()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            };
            let root_cert = get("sslrootcert").map(str::to_owned);
            match get("sslmode") {
                Some("require") => {
                    if root_cert.is_some() {
                        Self::Unsupported {
                            reason: "sslmode=require with sslrootcert is not supported: \
                                     PostgreSQL treats that combination as requiring \
                                     certificate verification against the CA file, which \
                                     this pool implements only for sslmode=verify-full. \
                                     Use sslmode=verify-full to verify the certificate, or \
                                     sslmode=require without sslrootcert to encrypt without \
                                     verifying it."
                                .to_owned(),
                        }
                    } else {
                        Self::Require
                    }
                }
                Some("verify-full") => Self::VerifyFull { root_cert },
                Some("verify-ca") => Self::Unsupported {
                    reason: "sslmode=verify-ca is not supported: chain-only verification \
                             (without hostname checking) is not implemented. Use \
                             sslmode=verify-full (stricter) or sslmode=require (encrypts \
                             without verifying the certificate)."
                        .to_owned(),
                },
                // Absent, `disable`, `prefer`, or anything unrecognized:
                // preserve the pre-TLS behavior exactly (including the error
                // tokio-postgres itself raises for invalid values).
                _ => Self::Off,
            }
        }
    }

    /// Build the pool's custom connection-setup callback for a TLS posture.
    // Only the default (Postgres) `build_pool` arm installs this custom TLS
    // setup; unused in a `--features sqlite` build (SQLite has no TLS transport).
    #[cfg_attr(feature = "sqlite", allow(dead_code))]
    pub(super) fn setup_callback(posture: TlsPosture) -> SetupCallback<AsyncPgConnection> {
        Box::new(move |url: &str| {
            let posture = posture.clone();
            let url = url.to_owned();
            async move { establish(&url, posture).await }.boxed()
        })
    }

    /// Establish an [`AsyncPgConnection`] honoring `posture` — the single
    /// connect path shared by the pool's setup callback and the synchronous
    /// migration/wait-check wrapper
    /// ([`super::establish_migration_connection`]).
    pub(super) async fn establish(
        url: &str,
        posture: TlsPosture,
    ) -> ConnectionResult<AsyncPgConnection> {
        match posture {
            // Defensive: `build_pool` never installs the callback for
            // `Off`, but fall back to the stock path if it ever does.
            TlsPosture::Off => AsyncPgConnection::establish(url).await,
            TlsPosture::Require => connect_with(url, relaxed_connector()?).await,
            TlsPosture::VerifyFull { root_cert } => {
                // tokio-postgres's own connection-string parser
                // rejects `verify-*` and `sslrootcert`, so hand it a
                // sanitized string; the real verification lives in
                // the rustls config.
                let sanitized = sanitize_for_verify_full(url);
                connect_with(&sanitized, verifying_connector(root_cert.as_deref())?).await
            }
            TlsPosture::Unsupported { reason } => {
                Err(ConnectionError::InvalidConnectionUrl(reason))
            }
        }
    }

    async fn connect_with(
        url: &str,
        tls: MakeRustlsConnect,
    ) -> ConnectionResult<AsyncPgConnection> {
        let (client, connection) = tokio_postgres::connect(url, tls).await.map_err(|e| {
            // tokio_postgres's Display gives only the error kind ("error
            // performing TLS handshake"); append the source so operators see
            // the actionable cause ("server does not support TLS", a
            // certificate verification failure, …).
            let msg = std::error::Error::source(&e)
                .map_or_else(|| e.to_string(), |source| format!("{e}: {source}"));
            ConnectionError::BadConnection(msg)
        })?;
        AsyncPgConnection::try_from_client_and_connection(client, connection).await
    }

    /// Encrypt-only connector for `sslmode=require`: handshake signatures are
    /// verified but the certificate chain/identity is not, mirroring
    /// `libpq`/`psql` behavior for `require` (which self-signed and
    /// private-CA deployments rely on).
    fn relaxed_connector() -> Result<MakeRustlsConnect, ConnectionError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                ConnectionError::BadConnection(format!("failed to build TLS config: {e}"))
            })?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoServerCertVerification(
                (*provider).clone(),
            )))
            .with_no_client_auth();
        Ok(MakeRustlsConnect::new(config))
    }

    /// Fully verifying connector for `sslmode=verify-full`: certificate chain
    /// and hostname are checked against the Mozilla root store plus any
    /// `sslrootcert` PEM file from the connection string. `webpki-roots` is
    /// used (rather than the platform store) so behavior is deterministic on
    /// every target, including mobile, where no native store is reachable.
    fn verifying_connector(root_cert: Option<&str>) -> Result<MakeRustlsConnect, ConnectionError> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = root_cert {
            use rustls_pki_types::pem::PemObject as _;
            let certs = CertificateDer::pem_file_iter(path).map_err(|e| {
                ConnectionError::BadConnection(format!("failed to read sslrootcert {path}: {e}"))
            })?;
            for cert in certs {
                let cert = cert.map_err(|e| {
                    ConnectionError::BadConnection(format!(
                        "failed to parse sslrootcert {path}: {e}"
                    ))
                })?;
                roots.add(cert).map_err(|e| {
                    ConnectionError::BadConnection(format!(
                        "failed to trust sslrootcert {path}: {e}"
                    ))
                })?;
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                ConnectionError::BadConnection(format!("failed to build TLS config: {e}"))
            })?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(MakeRustlsConnect::new(config))
    }

    /// Whether tokio-postgres would parse this connection string as a URL —
    /// see [`crate::pg_conn_str::is_url`]. Getting this wrong would send
    /// keyword strings whose values embed URL-like tokens
    /// (`password=https://…`) down the URL path, where `url::Url::parse`
    /// fails and the TLS posture silently falls back to [`TlsPosture::Off`].
    /// Shared with config validation so the two never disagree about which
    /// strings are reachable.
    use crate::pg_conn_str::is_url as is_url_connection_string;
    /// Parse a libpq-style `key = value` connection string, mirroring
    /// tokio-postgres's (private) parser — see
    /// [`crate::pg_conn_str::keyword_value_pairs`]. Naive whitespace
    /// splitting would miss `sslmode` in quoted/spaced strings and silently
    /// downgrade the TLS posture to [`TlsPosture::Off`].
    use crate::pg_conn_str::keyword_value_pairs;

    /// Extract query/keyword parameters relevant to TLS from either a
    /// `postgres://…` URL or a `key=value …` connection string.
    fn ssl_params(database_url: &str) -> Vec<(String, String)> {
        if is_url_connection_string(database_url) {
            url::Url::parse(database_url)
                .map(|u| {
                    u.query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            keyword_value_pairs(database_url).unwrap_or_default()
        }
    }

    /// Serialize one value of a keyword/value connection string, quoting it
    /// whenever it would not survive re-parsing as a bare token.
    fn quote_keyword_value(value: &str) -> String {
        if !value.is_empty()
            && !value.contains(|c: char| c.is_whitespace() || c == '\'' || c == '\\')
        {
            return value.to_owned();
        }
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('\'');
        for c in value.chars() {
            if c == '\'' || c == '\\' {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('\'');
        quoted
    }

    /// Rewrite a `verify-full` connection string into one tokio-postgres can
    /// parse: `sslmode` downgraded to `require` (the handshake decision), and
    /// `sslrootcert` removed (consumed by [`verifying_connector`] instead).
    fn sanitize_for_verify_full(database_url: &str) -> String {
        if is_url_connection_string(database_url) {
            let Ok(mut parsed) = url::Url::parse(database_url) else {
                return database_url.to_owned();
            };
            // Rewrite ONLY the sslmode/sslrootcert components, preserving
            // every other raw query component byte-for-byte: decoding and
            // re-serializing through query_pairs()/append_pair() would
            // form-encode spaces as `+`, which libpq/tokio-postgres do not
            // accept in Postgres URI parameters —
            // `options=-c%20search_path%3Dtenant` would be mangled into
            // `options=-c+search_path=tenant`. (The startup-wait splice in
            // migrate.rs avoids those APIs for the same reason.)
            let raw = parsed
                .query()
                .unwrap_or("")
                .split('&')
                .filter(|component| !component.is_empty())
                .filter_map(|component| {
                    let key = component.split('=').next().unwrap_or(component);
                    match key {
                        "sslmode" => Some("sslmode=require"),
                        "sslrootcert" => None,
                        _ => Some(component),
                    }
                })
                .collect::<Vec<_>>()
                .join("&");
            if raw.is_empty() {
                parsed.set_query(None);
            } else {
                parsed.set_query(Some(&raw));
            }
            parsed.to_string()
        } else {
            let Some(pairs) = keyword_value_pairs(database_url) else {
                // Malformed: pass through so tokio-postgres reports its own
                // parse error instead of us inventing a different string.
                return database_url.to_owned();
            };
            pairs
                .into_iter()
                .filter_map(|(k, v)| match k.as_str() {
                    "sslmode" => Some("sslmode=require".to_owned()),
                    "sslrootcert" => None,
                    _ => Some(format!("{k}={}", quote_keyword_value(&v))),
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    /// Accepts any server certificate without validating its chain or
    /// hostname, while still cryptographically verifying the TLS handshake
    /// signatures (the connection is genuinely encrypted to *some* holder of
    /// the presented key — "no identity check", not "no security"). This is
    /// exactly `libpq`/`psql`'s posture for `sslmode=require`; identity
    /// verification is `sslmode=verify-full`'s job.
    #[derive(Debug)]
    struct NoServerCertVerification(CryptoProvider);

    impl ServerCertVerifier for NoServerCertVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn absent_disable_and_prefer_keep_the_default_notls_path() {
            for url in [
                "postgres://user:pass@db.example.com:5432/app",
                "postgres://user:pass@db.example.com:5432/app?sslmode=disable",
                "postgres://user:pass@db.example.com:5432/app?sslmode=prefer",
                "host=db.example.com user=user",
                "host=db.example.com user=user sslmode=disable",
            ] {
                assert_eq!(
                    TlsPosture::from_database_url(url),
                    TlsPosture::Off,
                    "{url} must keep the default NoTls path"
                );
            }
        }

        #[test]
        fn unrecognized_sslmode_values_keep_the_default_path_and_its_errors() {
            // tokio-postgres itself rejects these at connect time; the pool
            // must not mask that with a different TLS decision.
            assert_eq!(
                TlsPosture::from_database_url("postgres://u@h/db?sslmode=allow"),
                TlsPosture::Off
            );
            assert_eq!(
                TlsPosture::from_database_url("postgres://u@h/db?sslmode=bogus"),
                TlsPosture::Off
            );
        }

        #[test]
        fn require_selects_the_relaxed_tls_connector() {
            assert_eq!(
                TlsPosture::from_database_url(
                    "postgres://user:pass@db.example.com:5432/app?sslmode=require"
                ),
                TlsPosture::Require
            );
            assert_eq!(
                TlsPosture::from_database_url("host=db.example.com sslmode=require"),
                TlsPosture::Require
            );
        }

        #[test]
        fn verify_full_selects_the_verifying_connector_with_optional_root() {
            assert_eq!(
                TlsPosture::from_database_url("postgres://u@h/db?sslmode=verify-full"),
                TlsPosture::VerifyFull { root_cert: None }
            );
            assert_eq!(
                TlsPosture::from_database_url(
                    "postgres://u@h/db?sslmode=verify-full&sslrootcert=/etc/ca.pem"
                ),
                TlsPosture::VerifyFull {
                    root_cert: Some("/etc/ca.pem".to_owned())
                }
            );
        }

        #[test]
        fn keyword_strings_with_spaces_around_equals_are_parsed() {
            // Regression: whitespace-splitting missed `sslmode` here and
            // silently downgraded the posture to Off (plaintext).
            assert_eq!(
                TlsPosture::from_database_url("host=db user=u sslmode = require"),
                TlsPosture::Require
            );
            assert_eq!(
                TlsPosture::from_database_url("sslmode = require"),
                TlsPosture::Require
            );
            assert_eq!(
                TlsPosture::from_database_url("host = db sslmode\t=\nverify-full user=u"),
                TlsPosture::VerifyFull { root_cert: None }
            );
        }

        #[test]
        fn keyword_strings_with_quoted_values_are_parsed() {
            assert_eq!(
                TlsPosture::from_database_url("sslmode='verify-full'"),
                TlsPosture::VerifyFull { root_cert: None }
            );
            assert_eq!(
                TlsPosture::from_database_url(
                    "host=db sslmode='verify-full' sslrootcert='/path with space/ca.pem'"
                ),
                TlsPosture::VerifyFull {
                    root_cert: Some("/path with space/ca.pem".to_owned())
                }
            );
            // Backslash escapes work inside and outside quotes, as in
            // tokio-postgres/libpq.
            assert_eq!(
                TlsPosture::from_database_url(r"sslmode=verify-full sslrootcert='/pki/it\'s.pem'"),
                TlsPosture::VerifyFull {
                    root_cert: Some("/pki/it's.pem".to_owned())
                }
            );
            assert_eq!(
                TlsPosture::from_database_url(r"sslmode=verify-full sslrootcert=/pki/my\ ca.pem"),
                TlsPosture::VerifyFull {
                    root_cert: Some("/pki/my ca.pem".to_owned())
                }
            );
        }

        #[test]
        fn malformed_keyword_strings_keep_the_default_path_and_its_errors() {
            // tokio-postgres rejects these at connect time; the posture must
            // not guess a different TLS decision for them.
            for url in [
                "host=db sslmode",            // missing `=` and value
                "host=db sslmode=",           // empty unquoted value
                "host=db sslmode='require",   // unterminated quote
                r"host=db sslmode='require\", // EOF right after escape
            ] {
                assert_eq!(
                    TlsPosture::from_database_url(url),
                    TlsPosture::Off,
                    "{url:?} must keep the default NoTls path"
                );
            }
        }

        #[test]
        fn keyword_strings_with_url_like_values_stay_on_the_keyword_path() {
            // Regression: `contains("://")` sent this whole string to
            // url::Url::parse, which failed, so sslmode=require silently
            // became Off (plaintext). Only `postgres://`/`postgresql://`
            // prefixes are URLs, exactly as in tokio-postgres.
            assert_eq!(
                TlsPosture::from_database_url("host=db password=https://secret sslmode=require"),
                TlsPosture::Require
            );
            assert_eq!(
                TlsPosture::from_database_url(
                    "host=db options='-c foo=bar://baz' sslmode='verify-full'"
                ),
                TlsPosture::VerifyFull { root_cert: None }
            );
            // The alternate scheme spelling is still parsed as a URL.
            assert_eq!(
                TlsPosture::from_database_url("postgresql://u@h/db?sslmode=require"),
                TlsPosture::Require
            );
        }

        #[test]
        fn verify_ca_and_require_with_rootcert_fail_loudly() {
            assert!(matches!(
                TlsPosture::from_database_url("postgres://u@h/db?sslmode=verify-ca"),
                TlsPosture::Unsupported { .. }
            ));
            assert!(matches!(
                TlsPosture::from_database_url(
                    "postgres://u@h/db?sslmode=require&sslrootcert=/etc/ca.pem"
                ),
                TlsPosture::Unsupported { .. }
            ));
        }

        #[test]
        fn last_sslmode_occurrence_wins() {
            assert_eq!(
                TlsPosture::from_database_url("postgres://u@h/db?sslmode=disable&sslmode=require"),
                TlsPosture::Require
            );
        }

        #[test]
        fn sanitize_rewrites_verify_full_and_drops_sslrootcert() {
            let sanitized = sanitize_for_verify_full(
                "postgres://u@h:5432/db?application_name=app&sslmode=verify-full&sslrootcert=/etc/ca.pem",
            );
            assert!(sanitized.contains("sslmode=require"), "{sanitized}");
            assert!(!sanitized.contains("verify-full"), "{sanitized}");
            assert!(!sanitized.contains("sslrootcert"), "{sanitized}");
            assert!(sanitized.contains("application_name=app"), "{sanitized}");

            // Percent-encoded components survive byte-for-byte: form
            // re-encoding would turn %20 into `+`, which libpq and
            // tokio-postgres do not accept as a space in URI parameters.
            let sanitized = sanitize_for_verify_full(
                "postgres://u@h/db?options=-c%20search_path%3Dtenant&sslmode=verify-full&sslrootcert=/etc/ca.pem",
            );
            assert_eq!(
                sanitized, "postgres://u@h/db?options=-c%20search_path%3Dtenant&sslmode=require",
                "raw components must be preserved verbatim"
            );
            // Dropping the only params leaves a clean URL with no `?`.
            assert_eq!(
                sanitize_for_verify_full("postgres://u@h/db?sslrootcert=/etc/ca.pem"),
                "postgres://u@h/db"
            );

            let kv = sanitize_for_verify_full(
                "host=h user=u sslmode=verify-full sslrootcert=/etc/ca.pem dbname=db",
            );
            assert_eq!(kv, "host=h user=u sslmode=require dbname=db");

            // Whitespace/quoting variants normalize to a string
            // tokio-postgres parses to the same parameters.
            let kv = sanitize_for_verify_full(
                r"host=h sslmode = 'verify-full' sslrootcert='/path with space/ca.pem' password='it\'s'",
            );
            assert_eq!(kv, r"host=h sslmode=require password='it\'s'");

            // URL-like values must not push a keyword string onto the URL
            // branch (which would pass it through unsanitized).
            let kv = sanitize_for_verify_full(
                "host=h password=https://secret sslmode=verify-full sslrootcert=/etc/ca.pem",
            );
            assert_eq!(kv, "host=h password=https://secret sslmode=require");
        }

        #[test]
        fn verifying_connector_builds_with_and_without_root_cert_file() {
            assert!(verifying_connector(None).is_ok());
            assert!(
                verifying_connector(Some("/nonexistent/ca.pem")).is_err(),
                "an unreadable sslrootcert must fail loudly"
            );
        }

        #[test]
        fn relaxed_connector_builds() {
            assert!(relaxed_connector().is_ok());
        }
    }
}

// ── Synchronous TLS-aware connections (migrations, wait checks) ───────────────

/// A synchronous diesel Pg connection for migrations and startup wait
/// checks, honoring the connection string's `sslmode` (issue #1585 review).
///
/// diesel's native [`diesel::PgConnection`] connects through libpq, and the
/// workspace bundles libpq **without** SSL support (`pq-sys`'s
/// `bundled_without_openssl`) — so with `sslmode=require`/`verify-full` the
/// async pool (rustls) connects fine while the sync migration path fails at
/// startup before the app ever serves. For those postures this wraps an
/// [`AsyncPgConnection`] established through the pool's own rustls connector
/// in diesel-async's [`AsyncConnectionWrapper`], which provides the full
/// sync diesel API including `MigrationHarness`. With TLS off the native
/// `PgConnection` path is kept byte-identical to the historical behavior.
///
/// [`AsyncConnectionWrapper`]:
///     diesel_async::async_connection_wrapper::AsyncConnectionWrapper
#[allow(clippy::large_enum_variant)] // short-lived, one per migration target
pub(crate) enum MigrationConnection {
    /// The historical libpq-backed connection (TLS posture `Off`).
    Native(diesel::PgConnection),
    /// rustls-backed sync wrapper (TLS posture `Require`/`VerifyFull`).
    Rustls {
        /// Dedicated runtime owning the connection's tokio driver task —
        /// always present, never the caller's ambient runtime (see
        /// [`establish_migration_connection`] for why). Callers must keep
        /// this alive as long as `conn` — dropping it kills the driver and
        /// every query after that hangs or errors. It is safe to drop
        /// normally (no deferred-shutdown wrapper needed): every caller of
        /// [`establish_migration_connection`] reaches it only from a thread
        /// that has never entered any runtime (see that function's doc
        /// comment), and stays off any runtime's "entered" state except
        /// during each individual `block_on` call this connection or `conn`
        /// makes — so a drop between those calls is never "from within an
        /// asynchronous context" the way tokio forbids.
        runtime: tokio::runtime::Runtime,
        conn: diesel_async::async_connection_wrapper::AsyncConnectionWrapper<AsyncPgConnection>,
    },
}

/// Whether migrations/wait checks for `database_url` must go through the
/// rustls wrapper rather than the native libpq path. Split out of
/// [`establish_migration_connection`] so the path selection is unit-testable
/// without a server.
fn migration_connection_needs_rustls(database_url: &str) -> bool {
    !matches!(
        tls::TlsPosture::from_database_url(database_url),
        tls::TlsPosture::Off
    )
}

/// Establish a [`MigrationConnection`] for `database_url`.
///
/// # Thread-safety contract — read before adding a new caller
///
/// This function itself is unconditionally safe to call from any thread: it
/// only ever enters/exits a runtime it just built, on the thread it is
/// running on, never touching an ambient one. But the connection it returns
/// is NOT fully self-contained for the rustls arm — the returned
/// [`AsyncConnectionWrapper`] bridges every subsequent sync diesel call
/// (`MigrationHarness::run_pending_migrations`, plain queries, ...) through
/// its OWN internal `block_on`, and that bridging still panics
/// ("Cannot start a runtime from within a runtime") if invoked from a thread
/// that is itself already inside some OTHER ambient runtime's context — the
/// exact shape of an app's own `.on_startup(|state| async move { ... })`
/// hook calling a migration function directly (TLS-enabled migrations used
/// to panic in production this way; TLS off never hit this arm, so it never
/// surfaced in dev). So this function alone does not make that case safe —
/// **every caller must ensure the connection is established AND used
/// entirely on a thread that has never entered any runtime**, e.g. via
/// [`with_migration_connection!`]/`with_sync_pg_connection!`, which dispatch
/// their whole body (this call plus every query `$body` makes) onto a
/// freshly spawned [`std::thread::scope`] thread. [`hold_migration_lock`]'s
/// caller (the CLI's `autumn migrate`) satisfies this by construction — it
/// never runs inside a tokio runtime at all.
///
/// [`AsyncConnectionWrapper`]:
///     diesel_async::async_connection_wrapper::AsyncConnectionWrapper
/// [`with_migration_connection!`]: crate::migrate
///
/// # Errors
///
/// Returns the underlying [`diesel::ConnectionError`] when the connection
/// cannot be established (including [`TlsPosture::Unsupported`] postures,
/// which fail with their documented guidance).
///
/// [`TlsPosture::Unsupported`]: tls::TlsPosture::Unsupported
pub(crate) fn establish_migration_connection(
    database_url: &str,
) -> Result<MigrationConnection, diesel::ConnectionError> {
    use diesel::Connection as _;
    if !migration_connection_needs_rustls(database_url) {
        return diesel::PgConnection::establish(database_url).map(MigrationConnection::Native);
    }
    let posture = tls::TlsPosture::from_database_url(database_url);
    // Build a dedicated runtime and connect on the calling thread directly, with no
    // further nested thread-spawn: per this function's own contract above, every caller
    // guarantees the calling thread has never entered any runtime, so entering this
    // freshly built one is always safe. It drives the connection's background I/O task
    // for as long as `conn` is used, so it is kept alongside `conn` in
    // [`MigrationConnection::Rustls`]. Dropping it is likewise safe here: by the time it
    // drops, after every query `$body` makes has completed, this thread is not
    // mid-`block_on` on anything.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| {
            diesel::ConnectionError::BadConnection(format!(
                "failed to build a tokio runtime for the TLS migration connection: {e}"
            ))
        })?;
    let inner = runtime.block_on(tls::establish(database_url, posture))?;
    Ok(MigrationConnection::Rustls {
        runtime,
        conn: diesel_async::async_connection_wrapper::AsyncConnectionWrapper::from(inner),
    })
}

/// Establish a synchronous diesel [`diesel::SqliteConnection`] for the `SQLite`
/// startup-migration path (issue #1614, PR3).
///
/// The `SQLite` counterpart to [`establish_migration_connection`], and much
/// thinner: `SQLite` is a single-writer local database, so there is no TLS
/// negotiation and no advisory-lock connection wrapper — diesel's
/// `MigrationHarness` runs directly on a plain `SqliteConnection`. The URL is
/// normalized through the same [`normalize_sqlite_target`] the runtime pool
/// uses, so the migration connection and the pool open the same database file.
///
/// The connection sets `PRAGMA busy_timeout = 5000` right after opening,
/// mirroring the runtime pool's per-connection `custom_setup` (see
/// [`build_sqlite_pool`]): without it, a second concurrent migrator — or a write
/// lock briefly held by the runtime pool — makes this connection fail
/// *immediately* with `SQLITE_BUSY`, and `auto_migrate_sqlite` exits the process.
/// With the timeout, migration statements WAIT up to 5s for the lock to clear
/// instead of aborting; diesel migrations are idempotent, so a migrator that
/// waits and then finds migrations already applied is fine. Only `busy_timeout`
/// is set here — NOT `foreign_keys`/`journal_mode`, because `foreign_keys = ON`
/// can break table-recreating migrations.
///
/// # Errors
///
/// Returns the underlying [`diesel::ConnectionError`] when the database cannot
/// be opened, or [`diesel::ConnectionError::CouldntSetupConfiguration`] if the
/// `busy_timeout` pragma cannot be applied.
#[cfg(feature = "sqlite")]
pub(crate) fn establish_sqlite_migration_connection(
    database_url: &str,
) -> Result<diesel::SqliteConnection, diesel::ConnectionError> {
    use diesel::Connection as _;
    use diesel::connection::SimpleConnection as _;
    let mut conn = diesel::SqliteConnection::establish(&normalize_sqlite_target(database_url))?;
    conn.batch_execute("PRAGMA busy_timeout = 5000;")
        .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
    Ok(conn)
}

#[cfg(test)]
mod migration_connection_tests {
    use super::migration_connection_needs_rustls;

    #[test]
    fn migration_path_selection_follows_the_tls_posture() {
        // NoTls URLs keep the historical native libpq path byte-identical.
        for url in [
            "postgres://u@h/db",
            "postgres://u@h/db?sslmode=disable",
            "postgres://u@h/db?sslmode=prefer",
            "host=db user=u",
        ] {
            assert!(
                !migration_connection_needs_rustls(url),
                "must stay on the native path: {url}"
            );
        }
        // TLS-requiring strings (URL and keyword forms) — and unsupported
        // postures, which must fail with the TLS module's guidance instead
        // of libpq's — go through the rustls wrapper.
        for url in [
            "postgres://u@h/db?sslmode=require",
            "postgres://u@h/db?sslmode=verify-full",
            "postgres://u@h/db?sslmode=verify-full&sslrootcert=/etc/ca.pem",
            "postgres://u@h/db?sslmode=verify-ca",
            "host=db user=u sslmode=require",
            "host=db sslmode = 'verify-full'",
        ] {
            assert!(
                migration_connection_needs_rustls(url),
                "must use the rustls wrapper: {url}"
            );
        }
    }

    // Regression: TLS-enabled migrations panicked with "Cannot start a runtime from
    // within a runtime" when called directly from an app's own async `on_startup` hook
    // rather than from `spawn_blocking`. The rustls arm's connect used to `block_on` on
    // the calling thread, which is one of the ambient runtime's worker threads while an
    // `.on_startup(|state| async move { ... })` body executes. TLS off (dev) never took
    // that arm, so the bug surfaced only with `sslmode=require`/`verify-full`.
    //
    // This exercises `crate::migrate::pending_migrations`, the public entry point apps
    // call, rather than `establish_migration_connection` directly: that bare function is
    // safe only when the whole connect-and-query sequence runs on a thread that never
    // entered a runtime, a guarantee `with_migration_connection!` provides by
    // construction. Calling it directly, as this test used to, no longer represents how
    // any real caller uses it.
    #[tokio::test(flavor = "multi_thread")]
    async fn migration_functions_do_not_panic_from_an_async_caller() {
        const MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");
        // Calling this directly from an `async fn` body -- not wrapped in
        // `spawn_blocking` -- mirrors the shape of an app's own async
        // `on_startup` hook calling `autumn_web::migrate::run_pending(...)`.
        // On a multi-threaded runtime (axum's default), matching production.
        let url = "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db?sslmode=require";
        let result = crate::migrate::pending_migrations(url, MIGRATIONS);
        assert!(
            result.is_err(),
            "an unreachable TLS-requiring host must fail with a connection error \
             (not panic on nested block_on)"
        );
    }

    // Regression guard: `#[tokio::test]`'s default flavor is `current_thread`, so exactly
    // one thread drives this runtime's I/O and timers, and that thread is the one about
    // to call a migration function directly — mirroring an app's own `.on_startup` hook
    // body, not `spawn_blocking`. If `with_migration_connection!` ever went back to
    // running the connect, or the query, on the calling thread instead of a freshly
    // spawned `thread::scope` thread, this would deadlock rather than fail fast: nothing
    // would be left free to drive the connect's I/O. A watchdog thread bounds that —
    // there is no clean way to cancel a genuinely stuck OS thread, so a real regression
    // hard-exits the process instead of hanging the suite.
    #[tokio::test]
    async fn migration_functions_do_not_hang_on_a_current_thread_runtime() {
        const MIGRATIONS: crate::migrate::EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");
        // A plain `sleep`-then-`exit` watchdog can't be cancelled once
        // spawned, so a detached one would keep counting down after this
        // test returns and could later `process::exit` the shared test
        // binary mid-suite, on an unrelated test, if the suite is still
        // running 20s after this test passed. A channel lets the main
        // thread signal "the call returned, stand down" so the watchdog only
        // ever fires on a genuine hang.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if done_rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .is_err()
            {
                eprintln!(
                    "migration_functions_do_not_hang_on_a_current_thread_runtime: \
                     timed out after 20s -- the TLS connect hung instead of failing fast"
                );
                std::process::exit(101);
            }
        });

        let url = "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db?sslmode=require";
        let result = crate::migrate::pending_migrations(url, MIGRATIONS);
        assert!(
            result.is_err(),
            "an unreachable TLS-requiring host must fail with a connection error"
        );

        // Stand the watchdog down and wait for it to exit cleanly, rather
        // than leaving a live "exit(101) in 20s" timer running in the shared
        // test binary after this test has already passed.
        let _ = done_tx.send(());
        let _ = watchdog.join();
    }
}
