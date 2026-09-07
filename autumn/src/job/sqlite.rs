//! Durable job queue for the `SQLite` backend (issue #1907).
//!
//! The Postgres queue coordinates with `LISTEN`/`NOTIFY`, `FOR UPDATE SKIP
//! LOCKED`, and advisory locks. `SQLite` has none of those, and a `SQLite`
//! deployment is single-host, so this backend uses the single-host equivalents:
//!
//! - **The queue is a table** (`autumn_jobs`) in the app's own database file.
//!   Work therefore survives a restart, with no Redis and no Postgres.
//! - **A claim is one statement.** `SQLite` serializes writers, so
//!   `UPDATE … WHERE id = (SELECT … LIMIT 1) RETURNING …` claims exactly one
//!   row for exactly one worker. That is the analog of `FOR UPDATE SKIP
//!   LOCKED`.
//! - **A crash leaves the row reclaimable.** A claim older than
//!   `jobs.sqlite.visibility_timeout_ms` is recovered and re-enqueued, at start
//!   and on an interval.
//! - **Workers poll.** There is no `LISTEN`/`NOTIFY`, so an idle worker rechecks
//!   the table every `jobs.sqlite.poll_interval_ms`.
//!
//! Framework migrations are Postgres SQL and do not run on `SQLite`, so the
//! runtime creates this schema itself at start.
//!
//! Row semantics — statuses, attempt counting, retry backoff, dead-lettering,
//! uniqueness windows, and concurrency limits — match the Postgres backend, so
//! an app moves between the two tiers without changing job code.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;

use super::{DEFAULT_JOB_ADMIN_HISTORY_LIMIT, JobExecutionOutcome, QueueLimits};
use super::{
    EnqueueOutcome, JobAdminBackend, JobAdminBackendEntry, JobAdminFuture, JobAdminMemoryBackend,
    JobAdminPage, JobAdminQuery, JobAdminRecord, JobAdminSnapshot, JobAdminStartDecision,
    JobAdminStatus, JobClient, JobInfo, JobUniquenessWindow, PgLifecycleRecord, QueueSchedule,
    QueueSlots, ResolvedJobConstraints, build_per_job_settings, collect_declared_queues,
    format_job_admin_time, install_job_client, is_final_attempt, job_admin_backend,
    job_payload_identity, job_unique_key, normalize_queue_name, pg_retry_delay_ms,
    record_pg_cancel_after_ack, record_pg_lifecycle_ack_result, run_job_handler,
    should_warn_pin_coverage, validate_unique_job_names, warn_pinned_uncovered_queues,
};
use crate::db::RuntimeConnection;
use crate::state::AppState;
use crate::{AutumnError, AutumnResult};

use diesel_async::pooled_connection::deadpool::Pool;

/// The app's `SQLite` runtime pool.
type SqlitePool = Pool<RuntimeConnection>;

const STATUS_ENQUEUED: &str = "enqueued";
const STATUS_RUNNING: &str = "running";
const STATUS_COMPLETED: &str = "completed";
const STATUS_FAILED: &str = "failed";
/// Terminal state an operator's discard or cancel puts a row in. It leaves the
/// row for audit but out of every dashboard list, as on Postgres.
const STATUS_DISCARDED: &str = "discarded";

/// Most rows one stale-claim sweep recovers.
///
/// Bounded so a large backlog cannot hold the single writer for one long
/// `UPDATE`. The next sweep takes the next batch.
const STALE_RECOVERY_BATCH: usize = 100;

/// Most terminal rows one job-history prune removes, for the same reason.
const HISTORY_PRUNE_BATCH: usize = 500;

/// Longest gap between stale-claim sweeps.
const MAX_MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Shortest gap between stale-claim sweeps, so a tiny visibility timeout cannot
/// turn the sweep into a hot loop.
const MIN_MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// How often expired `autumn_job_tracking` rows are swept.
///
/// Much slower than a stale-claim sweep: an expired record is already invisible
/// to reads, so this bounds table growth rather than correctness.
const TRACKING_CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// How often to sweep stale claims for a given visibility timeout.
///
/// Half the timeout, bounded, so a claim is recovered close to when it actually
/// expires. A fixed interval would make a short `visibility_timeout_ms` mean
/// nothing.
fn maintenance_interval(visibility_timeout_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(visibility_timeout_ms / 2)
        .clamp(MIN_MAINTENANCE_INTERVAL, MAX_MAINTENANCE_INTERVAL)
}

/// Columns every read of `autumn_jobs` returns.
const JOB_SELECT_COLS: &str = "id, name, queue, payload, status, attempt, max_attempts, \
     initial_backoff_ms, enqueued_at, run_at, started_at, finished_at, claimed_by, claimed_at, \
     last_error, traceparent, tracestate";

/// A job row read from the `SQLite` `autumn_jobs` table.
///
/// Timestamps are epoch milliseconds. `SQLite` has no timestamp type, and the
/// runtime stamps every time from the app's injected clock, so integers keep
/// ordering and comparison exact and keep a `#[sim_test]` reproducible.
#[derive(diesel::QueryableByName, Debug, Clone)]
#[allow(dead_code, reason = "columns are read selectively per code path")]
struct SqliteJobRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    queue: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    payload: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    attempt: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    max_attempts: i32,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    initial_backoff_ms: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    enqueued_at: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    run_at: i64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    started_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    finished_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    claimed_by: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    claimed_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    last_error: Option<String>,
    /// W3C `traceparent` captured at enqueue time.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    traceparent: Option<String>,
    /// W3C `tracestate` captured at enqueue time.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    tracestate: Option<String>,
}

/// One `BIGINT` column.
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// The payload of one row, for the admin retry path.
#[derive(diesel::QueryableByName)]
struct PayloadRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    payload: String,
}

impl SqliteJobRow {
    /// Parse the stored payload, or `Null` if it is not valid JSON.
    fn payload_value(&self) -> Value {
        serde_json::from_str::<Value>(&self.payload).unwrap_or(Value::Null)
    }

    fn to_admin_record(&self, status: JobAdminStatus) -> JobAdminRecord {
        let payload = self.payload_value();
        let (principal_id, correlation_id) = job_payload_identity(&payload);
        JobAdminRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            queue: normalize_queue_name(&self.queue),
            status,
            enqueued_at: admin_time(Some(self.enqueued_at)),
            scheduled_for: if status == JobAdminStatus::Scheduled {
                admin_time(Some(self.run_at))
            } else {
                None
            },
            started_at: admin_time(self.started_at),
            finished_at: admin_time(self.finished_at),
            attempt: u32::try_from(self.attempt).unwrap_or(0),
            max_attempts: u32::try_from(self.max_attempts).unwrap_or(1),
            last_error: self.last_error.clone(),
            principal_id,
            correlation_id,
        }
    }
}

/// Render an epoch-millis column the way the dashboard renders Postgres
/// timestamps.
fn admin_time(ms: Option<i64>) -> Option<String> {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(format_job_admin_time)
}

/// Current wall time in epoch milliseconds, from the injected clock.
fn now_ms(state: &AppState) -> i64 {
    state.clock().now().timestamp_millis()
}

/// Handle to the durable `SQLite` queue: the pool, plus a one-time schema
/// guard.
///
/// The schema cannot be created in `start_runtime`, which is synchronous, and
/// creating it in a spawned task would race the first enqueue. So the first
/// caller that needs the table — an enqueue or a worker, whichever runs first —
/// creates it, and everyone else waits on the same cell. A failed attempt
/// leaves the cell empty, so the next caller retries.
#[derive(Clone)]
pub(super) struct SqliteJobQueue {
    pool: SqlitePool,
    schema: Arc<tokio::sync::OnceCell<()>>,
    /// Raised by an enqueue, awaited by an idle worker.
    ///
    /// The enqueue client and the worker loops hold the same handle, so a job
    /// enqueued in this process starts at once instead of waiting out a poll.
    /// Work another process enqueued still waits for the poll — `SQLite` has no
    /// `LISTEN`/`NOTIFY`.
    wake: Arc<tokio::sync::Notify>,
}

impl SqliteJobQueue {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            schema: Arc::new(tokio::sync::OnceCell::new()),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Make sure the queue schema exists, then hand back the pool.
    async fn ready(&self) -> AutumnResult<&SqlitePool> {
        self.schema
            .get_or_try_init(|| ensure_schema(&self.pool))
            .await?;
        Ok(&self.pool)
    }
}

/// Create the queue table and its indexes.
///
/// Idempotent, and safe to run from several processes at once: `SQLite`
/// serializes writers and every statement is `IF NOT EXISTS`.
///
/// # Errors
///
/// Returns an error when the schema cannot be created.
pub(super) async fn ensure_schema(pool: &SqlitePool) -> AutumnResult<()> {
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite jobs pool error: {error}"))
    })?;
    for statement in [
        "CREATE TABLE IF NOT EXISTS autumn_jobs ( \
           id                 TEXT    PRIMARY KEY NOT NULL, \
           name               TEXT    NOT NULL, \
           queue              TEXT    NOT NULL DEFAULT 'default', \
           payload            TEXT    NOT NULL DEFAULT '{}', \
           status             TEXT    NOT NULL DEFAULT 'enqueued', \
           attempt            INTEGER NOT NULL DEFAULT 1, \
           max_attempts       INTEGER NOT NULL DEFAULT 5, \
           initial_backoff_ms BIGINT  NOT NULL DEFAULT 250, \
           enqueued_at        BIGINT  NOT NULL DEFAULT 0, \
           run_at             BIGINT  NOT NULL DEFAULT 0, \
           started_at         BIGINT, \
           finished_at        BIGINT, \
           claimed_by         TEXT, \
           claimed_at         BIGINT, \
           last_error         TEXT, \
           unique_key         TEXT, \
           unique_window      TEXT, \
           unique_ttl_ms      BIGINT, \
           pending_unique_key TEXT, \
           concurrency_key    TEXT, \
           concurrency_limit  INTEGER, \
           traceparent        TEXT, \
           tracestate         TEXT)",
        "CREATE INDEX IF NOT EXISTS idx_autumn_jobs_queue_ready \
         ON autumn_jobs (queue, run_at) WHERE status = 'enqueued'",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_autumn_jobs_unique_inflight \
         ON autumn_jobs (name, unique_key) \
         WHERE unique_key IS NOT NULL AND status IN ('enqueued', 'running')",
        // Serves the TTL dedup guard and the TTL eviction, which carry no
        // status term and so cannot use the partial unique index above. Partial
        // itself, so an app with no unique jobs carries an empty index.
        "CREATE INDEX IF NOT EXISTS idx_autumn_jobs_unique_lookup \
         ON autumn_jobs (name, unique_key) WHERE unique_key IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_autumn_jobs_enqueued_dashboard \
         ON autumn_jobs (enqueued_at DESC) WHERE status = 'enqueued'",
        "CREATE INDEX IF NOT EXISTS idx_autumn_jobs_concurrency_running \
         ON autumn_jobs (name, concurrency_key) WHERE status = 'running'",
        "CREATE INDEX IF NOT EXISTS idx_autumn_jobs_status_finished \
         ON autumn_jobs (status, finished_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_autumn_jobs_stale_recovery \
         ON autumn_jobs (claimed_at) WHERE status = 'running'",
    ] {
        diesel::sql_query(statement)
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "sqlite jobs schema setup failed: {error}"
                ))
            })?;
    }
    // A table an earlier build of this runtime created has no `unique_ttl_ms`.
    // SQLite has no `ADD COLUMN IF NOT EXISTS`, so the error is the check — but
    // only the duplicate-column error means "already migrated". Anything else
    // (writer contention, for one) must propagate, or the schema cell would be
    // marked ready with the column still missing and every enqueue would fail
    // until the process restarted.
    if let Err(error) = diesel::sql_query("ALTER TABLE autumn_jobs ADD COLUMN unique_ttl_ms BIGINT")
        .execute(&mut *conn)
        .await
        && !error.to_string().contains("duplicate column name")
    {
        return Err(AutumnError::internal_server_error_msg(format!(
            "sqlite jobs schema setup failed: {error}"
        )));
    }
    Ok(())
}

/// Insert a job row, honoring the declared uniqueness window.
///
/// Returns [`EnqueueOutcome::Deduplicated`] when a unique job already holds the
/// key, matching the Postgres backend.
///
/// # Errors
///
/// Returns an error when the payload cannot be serialized or the insert fails.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enqueue_job_at(
    queue_handle: &SqliteJobQueue,
    clock: &dyn crate::time::ClockSource,
    id: String,
    name: &str,
    queue: &str,
    payload: Value,
    max_attempts: u32,
    initial_backoff_ms: u64,
    run_at: Option<chrono::DateTime<chrono::Utc>>,
    constraints: &ResolvedJobConstraints,
) -> AutumnResult<EnqueueOutcome> {
    use diesel_async::RunQueryDsl as _;

    let queue = normalize_queue_name(queue);
    let payload_str = serde_json::to_string(&payload).map_err(|error| {
        AutumnError::internal_server_error_msg(format!("serialize job payload: {error}"))
    })?;
    let now = clock.now().timestamp_millis();
    let run_at_ms = run_at.map_or(now, |due| due.timestamp_millis());
    let unique_ttl_ms = match constraints.unique_window {
        Some(JobUniquenessWindow::TtlMs(ms)) => Some(i64::try_from(ms).unwrap_or(i64::MAX)),
        _ => None,
    };
    let has_unique_key = constraints.unique_key.is_some();
    let concurrency_limit = constraints
        .concurrency_limit
        .map(|limit| i32::try_from(limit).unwrap_or(i32::MAX));
    // A limit with no scope shares one pool per job name (NULL concurrency_key).
    let concurrency_key = if constraints.concurrency_limit.is_some() {
        constraints.concurrency_scope.clone()
    } else {
        None
    };
    let (traceparent, tracestate) = super::capture_job_trace_context_for_backend();

    let pool = queue_handle.ready().await?;
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite jobs pool error: {error}"))
    })?;

    if let (Some(ttl), Some(key)) = (unique_ttl_ms, constraints.unique_key.as_deref()) {
        evict_expired_unique_key(&mut conn, name, key, now.saturating_sub(ttl)).await?;
    }

    // The `WHERE` is the dedup check and the partial unique index is the
    // backstop, exactly as on Postgres. A TTL window checks only the time
    // window, so a job that outlives its TTL cannot block a replacement.
    //
    // The two windows get separate query text rather than one `CASE`, because a
    // `CASE` buries the status test where the planner cannot reach the partial
    // unique index — every unique enqueue would then scan the whole table. Both
    // texts take the same three guard binds, in the same order, so the bind
    // chain below stays single.
    let ttl_cutoff = unique_ttl_ms.map(|ttl| now.saturating_sub(ttl));
    let dedup_guard = if unique_ttl_ms.is_some() {
        "NOT EXISTS ( \
           SELECT 1 FROM autumn_jobs dup \
           WHERE dup.name = ? AND dup.unique_key = ? AND dup.enqueued_at > ?)"
    } else {
        // The trailing `? IS NULL` consumes the unused TTL-cutoff bind; it is
        // always true here, and a constant term does not stop the planner from
        // using the `(name, unique_key)` terms above it. A job with no unique
        // key binds NULL, so `dup.unique_key = ?` matches nothing and the guard
        // passes.
        "NOT EXISTS ( \
           SELECT 1 FROM autumn_jobs dup \
           WHERE dup.name = ? AND dup.unique_key = ? \
             AND dup.status IN ('enqueued', 'running') \
             AND ? IS NULL)"
    };
    let inserted = diesel::sql_query(format!(
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at, unique_key, unique_window, unique_ttl_ms, concurrency_key, \
          concurrency_limit, traceparent, tracestate) \
         SELECT ?, ?, ?, ?, '{STATUS_ENQUEUED}', 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
         WHERE {dedup_guard} \
         ON CONFLICT (name, unique_key) \
           WHERE unique_key IS NOT NULL AND status IN ('{STATUS_ENQUEUED}', '{STATUS_RUNNING}') \
           DO NOTHING"
    ))
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Text, _>(&queue)
    .bind::<diesel::sql_types::Text, _>(payload_str)
    .bind::<diesel::sql_types::Integer, _>(i32::try_from(max_attempts).unwrap_or(i32::MAX))
    .bind::<diesel::sql_types::BigInt, _>(i64::try_from(initial_backoff_ms).unwrap_or(i64::MAX))
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(run_at_ms)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(constraints.unique_key.clone())
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
        constraints.unique_window_tag().map(str::to_owned),
    )
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(unique_ttl_ms)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(concurrency_key)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Integer>, _>(concurrency_limit)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(traceparent)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(tracestate)
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(constraints.unique_key.clone())
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(ttl_cutoff)
    .execute(&mut *conn)
    .await
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite job enqueue failed: {error}"))
    })?;

    if inserted == 0 {
        // The conflict target is the dedup index, so a zero row count can only
        // be a unique job that coalesced — never a silently lost row.
        if has_unique_key {
            return Ok(EnqueueOutcome::Deduplicated);
        }
        return Err(AutumnError::internal_server_error_msg(
            "sqlite job enqueue inserted no row",
        ));
    }
    // Wake an idle worker in this process rather than make it wait out a poll.
    queue_handle.wake.notify_one();
    Ok(EnqueueOutcome::Queued)
}

/// Clear a TTL dedup key whose window has elapsed, so it stops occupying the
/// partial unique index and blocking a legitimate replacement enqueue.
///
/// # Errors
///
/// Returns the database error rather than swallowing it. If this fails on
/// writer contention and the insert that follows then reaches the database, the
/// stale key is still in the index, the insert conflicts, and the caller is told
/// its job was deduplicated when the TTL had in fact expired and nothing was
/// queued. A failed enqueue the caller can retry is the honest answer.
async fn evict_expired_unique_key(
    conn: &mut RuntimeConnection,
    name: &str,
    key: &str,
    cutoff: i64,
) -> AutumnResult<()> {
    use diesel_async::RunQueryDsl as _;

    diesel::sql_query(format!(
        "UPDATE autumn_jobs SET unique_key = NULL \
         WHERE name = ? AND unique_key = ? AND unique_window = 'ttl' \
           AND enqueued_at <= ? AND status IN ('{STATUS_ENQUEUED}', '{STATUS_RUNNING}')"
    ))
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Text, _>(key)
    .bind::<diesel::sql_types::BigInt, _>(cutoff)
    .execute(&mut *conn)
    .await
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!(
            "sqlite job unique-key eviction failed: {error}"
        ))
    })?;
    Ok(())
}

/// Claim the oldest ready row on `queue` for `worker_id`.
///
/// One statement: `SQLite` serializes writers, so the select-and-update cannot
/// interleave with another worker's claim. That is the single-writer analog of
/// `FOR UPDATE SKIP LOCKED`.
async fn claim_next_job(
    pool: &SqlitePool,
    worker_id: &str,
    queue: &str,
    now: i64,
) -> Option<SqliteJobRow> {
    use diesel::OptionalExtension as _;
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool.get().await.ok()?;
    // Probe with a read first. The claim is an UPDATE, which opens a write
    // transaction and takes the single writer lock even when it matches
    // nothing — so an idle worker fleet would otherwise contend with the
    // application's own writes on every poll.
    let ready = diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM ( \
           SELECT 1 FROM autumn_jobs \
           WHERE status = '{STATUS_ENQUEUED}' AND run_at <= ? AND queue = ? \
           LIMIT 1)"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::Text, _>(queue)
    .get_result::<CountRow>(&mut *conn)
    .await;
    match ready {
        Ok(row) if row.count == 0 => return None,
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(error = %error, "sqlite job ready probe failed");
            return None;
        }
    }
    let claimed = diesel::sql_query(format!(
        "UPDATE autumn_jobs \
         SET status = '{STATUS_RUNNING}', started_at = ?, claimed_by = ?, claimed_at = ?, \
             pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END, \
             unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END \
         WHERE id = ( \
           SELECT c.id FROM autumn_jobs c \
           WHERE c.status = '{STATUS_ENQUEUED}' AND c.run_at <= ? AND c.queue = ? \
             AND (c.concurrency_limit IS NULL OR ( \
               SELECT COUNT(*) FROM autumn_jobs r \
               WHERE r.status = '{STATUS_RUNNING}' AND r.name = c.name \
                 AND r.concurrency_key IS c.concurrency_key \
             ) < c.concurrency_limit) \
           ORDER BY c.run_at ASC \
           LIMIT 1) \
         RETURNING {JOB_SELECT_COLS}"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::Text, _>(worker_id)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::Text, _>(queue)
    .get_result::<SqliteJobRow>(&mut *conn)
    .await
    .optional();

    match claimed {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(error = %error, "sqlite job claim failed");
            None
        }
    }
}

/// Mark a claimed job completed. Returns whether this worker still held it.
async fn ack_success(
    pool: &SqlitePool,
    now: i64,
    job_id: &str,
    worker_id: &str,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite jobs pool error: {error}"))
    })?;
    diesel::sql_query(format!(
        "UPDATE autumn_jobs \
         SET status = '{STATUS_COMPLETED}', finished_at = ?, claimed_by = NULL, \
             claimed_at = NULL, last_error = NULL \
         WHERE id = ? AND claimed_by = ? AND status = '{STATUS_RUNNING}'"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::Text, _>(job_id)
    .bind::<diesel::sql_types::Text, _>(worker_id)
    .execute(&mut *conn)
    .await
    .map(|rows| rows > 0)
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite job ack failed: {error}"))
    })
}

/// Retry a failed job with exponential backoff, or dead-letter it on the final
/// attempt. Returns whether this worker still held the claim.
async fn nack_failure(
    pool: &SqlitePool,
    now: i64,
    job_id: &str,
    worker_id: &str,
    error: &str,
    row: &SqliteJobRow,
    pending_unique_key: Option<&str>,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite jobs pool error: {error}"))
    })?;

    if is_final_attempt(&row.attempt, &row.max_attempts) {
        return dead_letter_on(&mut conn, now, job_id, worker_id, error).await;
    }

    let delay_ms = pg_retry_delay_ms(row.initial_backoff_ms, row.attempt);
    // Restore a pending-window unique key in the same UPDATE that re-enqueues
    // the row, so there is no window where the row is claimable with no key and
    // a concurrent enqueue slips past the dedup index.
    diesel::sql_query(format!(
        "UPDATE autumn_jobs \
         SET status = '{STATUS_ENQUEUED}', \
             attempt = attempt + 1, \
             run_at = ?, \
             started_at = NULL, \
             finished_at = NULL, \
             claimed_by = NULL, \
             claimed_at = NULL, \
             last_error = ?, \
             unique_key = CASE \
               WHEN ? IS NOT NULL AND NOT EXISTS ( \
                 SELECT 1 FROM autumn_jobs dup \
                 WHERE dup.name = autumn_jobs.name AND dup.unique_key = ? \
                   AND dup.id != autumn_jobs.id \
                   AND dup.status IN ('{STATUS_ENQUEUED}', '{STATUS_RUNNING}')) \
               THEN ? \
               ELSE unique_key \
             END, \
             pending_unique_key = NULL \
         WHERE id = ? AND claimed_by = ? AND status = '{STATUS_RUNNING}'"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now.saturating_add(delay_ms))
    .bind::<diesel::sql_types::Text, _>(error)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(pending_unique_key)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(pending_unique_key)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(pending_unique_key)
    .bind::<diesel::sql_types::Text, _>(job_id)
    .bind::<diesel::sql_types::Text, _>(worker_id)
    .execute(&mut *conn)
    .await
    .map(|rows| rows > 0)
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite job retry failed: {error}"))
    })
}

/// Dead-letter a job whatever its remaining attempts. Panics and unknown job
/// types are terminal.
async fn ack_dead_letter(
    pool: &SqlitePool,
    now: i64,
    job_id: &str,
    worker_id: &str,
    error: &str,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite jobs pool error: {error}"))
    })?;
    dead_letter_on(&mut conn, now, job_id, worker_id, error).await
}

async fn dead_letter_on(
    conn: &mut RuntimeConnection,
    now: i64,
    job_id: &str,
    worker_id: &str,
    error: &str,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    diesel::sql_query(format!(
        "UPDATE autumn_jobs \
         SET status = '{STATUS_FAILED}', finished_at = ?, claimed_by = NULL, \
             claimed_at = NULL, last_error = ? \
         WHERE id = ? AND claimed_by = ? AND status = '{STATUS_RUNNING}'"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::Text, _>(error)
    .bind::<diesel::sql_types::Text, _>(job_id)
    .bind::<diesel::sql_types::Text, _>(worker_id)
    .execute(&mut *conn)
    .await
    .map(|rows| rows > 0)
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite job dead-letter failed: {error}"))
    })
}

/// Re-enqueue rows whose claim outlived the visibility timeout, and
/// dead-letter those with no attempts left.
///
/// This is what makes a crash mid-job recoverable: the worker is gone, but the
/// row is still there.
async fn recover_stale_claims(pool: &SqlitePool, visibility_timeout_ms: u64, state: &AppState) {
    use diesel_async::RunQueryDsl as _;

    let Ok(mut conn) = pool.get().await else {
        tracing::warn!("sqlite stale-claim recovery could not acquire a connection");
        return;
    };
    let now = now_ms(state);
    let cutoff = now.saturating_sub(i64::try_from(visibility_timeout_ms).unwrap_or(i64::MAX));
    // Restore a pending-window unique key with the status change, so the row is
    // never claimable with the key missing.
    //
    // A row on its final attempt is dead-lettered instead of re-enqueued, so
    // there is nothing to restore it onto — but it keeps `pending_unique_key`
    // rather than having it cleared, because that is the only surviving copy
    // and an operator's retry restores the key from it.
    let sql = format!(
        "UPDATE autumn_jobs \
         SET status = CASE WHEN attempt < max_attempts THEN '{STATUS_ENQUEUED}' \
                           ELSE '{STATUS_FAILED}' END, \
             attempt = CASE WHEN attempt < max_attempts THEN attempt + 1 ELSE attempt END, \
             run_at = CASE WHEN attempt < max_attempts THEN ? ELSE run_at END, \
             started_at = NULL, \
             finished_at = CASE WHEN attempt >= max_attempts THEN ? ELSE NULL END, \
             claimed_by = NULL, \
             claimed_at = NULL, \
             last_error = 'visibility timeout expired', \
             unique_key = CASE \
               WHEN attempt < max_attempts AND pending_unique_key IS NOT NULL \
                    AND NOT EXISTS ( \
                      SELECT 1 FROM autumn_jobs dup \
                      WHERE dup.name = autumn_jobs.name \
                        AND dup.unique_key = autumn_jobs.pending_unique_key \
                        AND dup.id != autumn_jobs.id \
                        AND dup.status IN ('{STATUS_ENQUEUED}', '{STATUS_RUNNING}')) \
               THEN pending_unique_key \
               ELSE unique_key \
             END, \
             pending_unique_key = CASE \
               WHEN attempt < max_attempts THEN NULL \
               ELSE pending_unique_key \
             END \
         WHERE id IN ( \
           SELECT id FROM autumn_jobs \
           WHERE status = '{STATUS_RUNNING}' AND claimed_at IS NOT NULL AND claimed_at <= ? \
           LIMIT {STALE_RECOVERY_BATCH}) \
         RETURNING id, name, status, payload"
    );
    let recovered = diesel::sql_query(sql)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .bind::<diesel::sql_types::BigInt, _>(cutoff)
        .load::<RecoveredRow>(&mut *conn)
        .await;
    let rows = match recovered {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(error = %error, "sqlite stale-claim recovery failed");
            return;
        }
    };
    for row in rows {
        if row.status == STATUS_FAILED {
            tracing::warn!(
                job = %row.name,
                job_id = %row.id,
                "sqlite job dead-lettered after its visibility timeout expired"
            );
            state.job_registry.record_failure(
                &row.name,
                "visibility timeout expired".to_owned(),
                true,
            );
            crate::alerts::notify_dead_lettered_job(
                state,
                &row.name,
                &row.id,
                "visibility timeout expired",
            );
            // The row will never run again, so settle its tracked record now.
            // Otherwise the status endpoint reports `running` until the record
            // expires.
            let payload = serde_json::from_str::<Value>(&row.payload).unwrap_or(Value::Null);
            crate::job_tracking::settle_tracked_payload_as_failed(
                state,
                &payload,
                crate::job_tracking::GENERIC_FAILURE_MESSAGE,
            )
            .await;
        } else {
            tracing::warn!(
                job = %row.name,
                job_id = %row.id,
                "sqlite job re-enqueued after its visibility timeout expired"
            );
            // No gauge write here: the row is back in the table and the depth
            // survey publishes it absolutely on its next pass. Postgres does
            // the same.
        }
    }
}

/// The columns stale-claim recovery reports.
#[derive(diesel::QueryableByName)]
struct RecoveredRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    /// Needed to settle the tracked record of a row this sweep dead-letters.
    #[diesel(sql_type = diesel::sql_types::Text)]
    payload: String,
}

/// Wait until the queue schema exists, then hand back the pool.
///
/// Retries every `retry_after` rather than failing the task for good: a first
/// attempt can lose a `busy_timeout` race with another process booting against
/// the same file. Returns `None` only on shutdown.
async fn wait_ready(
    queue_handle: &SqliteJobQueue,
    retry_after: std::time::Duration,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Option<SqlitePool> {
    loop {
        match queue_handle.ready().await {
            Ok(pool) => return Some(pool.clone()),
            Err(error) => {
                tracing::error!(error = %error, "sqlite job queue schema setup failed; retrying");
                tokio::select! {
                    () = shutdown.cancelled() => return None,
                    () = tokio::time::sleep(retry_after) => {}
                }
            }
        }
    }
}

/// Delete `autumn_job_tracking` rows past their expiry.
///
/// Expired rows are already invisible to reads and writes, which filter on
/// `expires_at`, so this only bounds the table's growth. Skipped while the
/// dataset is under a GDPR legal hold, exactly as on Postgres.
async fn cleanup_expired_tracking_rows(pool: &SqlitePool, state: &AppState) {
    use diesel_async::RunQueryDsl as _;

    let registry = state.extension::<crate::gdpr::GdprRegistry>();
    if let Some(reason) = crate::data_retention::legal_hold_for(
        crate::data_retention::RetentionDataset::JobTracking,
        registry.as_deref(),
    ) {
        tracing::debug!(
            reason = %reason,
            "job tracking cleanup skipped: autumn_job_tracking is under legal hold"
        );
        return;
    }
    let Ok(mut conn) = pool.get().await else {
        tracing::warn!("job tracking cleanup could not acquire a connection");
        return;
    };
    // The table exists only once a tracked job has been enqueued, so a missing
    // table is the ordinary "no tracked jobs yet" case, not a fault.
    if let Err(error) = diesel::sql_query("DELETE FROM autumn_job_tracking WHERE expires_at <= ?")
        .bind::<diesel::sql_types::BigInt, _>(now_ms(state))
        .execute(&mut *conn)
        .await
    {
        tracing::debug!(error = %error, "sqlite job tracking cleanup skipped");
    }
}

/// One `(queue, name)` group of the ready backlog.
#[derive(diesel::QueryableByName)]
struct QueueDepthRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    queue: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
    /// The oldest ready job's `run_at`, in epoch milliseconds.
    ///
    /// An instant, not an age: unlike Postgres, both `run_at` and the readiness
    /// filter come from the app's own injected clock, so there is one timeline
    /// and nothing to rebase.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    oldest_ready_at: Option<i64>,
}

/// Refresh the actuator's per-queue depth and per-job-type `queued` gauges from
/// the table.
///
/// Every role runs this, web replicas included, so a process that only enqueues
/// still reports the backlog the host actually holds rather than its own
/// ever-growing local marks.
async fn update_queue_depth_gauges(pool: &SqlitePool, state: &AppState) {
    use diesel_async::RunQueryDsl as _;

    let Ok(mut conn) = pool.get().await else {
        return;
    };
    let rows = diesel::sql_query(format!(
        "SELECT queue, name, COUNT(*) AS count, MIN(run_at) AS oldest_ready_at          FROM autumn_jobs          WHERE status = '{STATUS_ENQUEUED}' AND run_at <= ?          GROUP BY queue, name"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now_ms(state))
    .load::<QueueDepthRow>(&mut *conn)
    .await;
    match rows {
        Ok(rows) => {
            let gauges = super::aggregate_surveyed_job_gauges(rows.into_iter().map(|row| {
                (
                    normalize_queue_name(&row.queue),
                    row.name,
                    u64::try_from(row.count).unwrap_or(0),
                    row.oldest_ready_at.and_then(|ms| u64::try_from(ms).ok()),
                )
            }));
            state.job_registry.set_queue_depth_gauges(&gauges.per_queue);
            state.job_registry.set_queued_counts(&gauges.per_name);
        }
        Err(error) => {
            tracing::warn!(error = %error, "sqlite queue-depth survey failed");
        }
    }
}

/// One `(name, count)` row of the blocked-on-concurrency survey.
#[derive(diesel::QueryableByName)]
struct NameCountRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// Refresh the `blocked_on_concurrency` gauge: ready rows a
/// `#[job(concurrency = N)]` limit is currently holding back.
///
/// Only surveyed when some job declares a limit, so an app without one pays
/// nothing.
async fn update_concurrency_blocked_gauges(pool: &SqlitePool, state: &AppState) {
    use diesel_async::RunQueryDsl as _;

    let Ok(mut conn) = pool.get().await else {
        return;
    };
    let rows = diesel::sql_query(format!(
        "SELECT blocked.name AS name, COUNT(*) AS count \
         FROM autumn_jobs blocked \
         WHERE blocked.status = '{STATUS_ENQUEUED}' \
           AND blocked.run_at <= ? \
           AND blocked.concurrency_limit IS NOT NULL \
           AND ( \
             SELECT COUNT(*) FROM autumn_jobs running \
             WHERE running.status = '{STATUS_RUNNING}' \
               AND running.name = blocked.name \
               AND running.concurrency_key IS blocked.concurrency_key \
           ) >= blocked.concurrency_limit \
         GROUP BY blocked.name"
    ))
    .bind::<diesel::sql_types::BigInt, _>(now_ms(state))
    .load::<NameCountRow>(&mut *conn)
    .await;
    match rows {
        Ok(rows) => {
            let counts: HashMap<String, u64> = rows
                .into_iter()
                .map(|row| (row.name, u64::try_from(row.count).unwrap_or(0)))
                .collect();
            state.job_registry.set_concurrency_blocked_counts(&counts);
        }
        Err(error) => {
            tracing::warn!(error = %error, "sqlite blocked-concurrency survey failed");
        }
    }
}

/// Delete terminal rows past the configured `retention.job_history` window.
///
/// Opt-in: with no window set, history is kept forever, exactly as on Postgres.
/// A row whose TTL dedup hold is still live is kept, or a replacement enqueue
/// would slip past the window it is meant to be deduped by. `unique_ttl_ms` is
/// what makes that check exact: without it a TTL-unique row could never be
/// pruned at all, however long ago it settled.
async fn prune_job_history(pool: &SqlitePool, state: &AppState, window: std::time::Duration) {
    use diesel_async::RunQueryDsl as _;

    let registry = state.extension::<crate::gdpr::GdprRegistry>();
    if let Some(reason) = crate::data_retention::legal_hold_for(
        crate::data_retention::RetentionDataset::JobHistory,
        registry.as_deref(),
    ) {
        tracing::debug!(
            reason = %reason,
            "job history prune skipped: autumn_jobs is under legal hold"
        );
        return;
    }
    let Ok(mut conn) = pool.get().await else {
        return;
    };
    let now = now_ms(state);
    let cutoff = now.saturating_sub(i64::try_from(window.as_millis()).unwrap_or(i64::MAX));
    if let Err(error) = diesel::sql_query(format!(
        "DELETE FROM autumn_jobs \
         WHERE id IN ( \
           SELECT id FROM autumn_jobs \
           WHERE status IN ('{STATUS_COMPLETED}', '{STATUS_FAILED}', '{STATUS_DISCARDED}') \
             AND finished_at IS NOT NULL AND finished_at < ? \
             AND NOT ( \
               unique_key IS NOT NULL AND unique_window = 'ttl' \
               AND enqueued_at > ? - COALESCE(unique_ttl_ms, 0)) \
           LIMIT {HISTORY_PRUNE_BATCH})"
    ))
    .bind::<diesel::sql_types::BigInt, _>(cutoff)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(&mut *conn)
    .await
    {
        tracing::warn!(error = %error, "sqlite job history prune failed");
    }
}

/// Refresh the backlog gauges on an interval.
async fn queue_depth_survey_loop(
    queue_handle: SqliteJobQueue,
    state: AppState,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let Some(pool) = wait_ready(&queue_handle, MAX_MAINTENANCE_INTERVAL, &shutdown).await else {
        return;
    };
    let mut interval = tokio::time::interval(MAX_MAINTENANCE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => update_queue_depth_gauges(&pool, &state).await,
            () = shutdown.cancelled() => break,
        }
    }
}

/// Run one claimed job and settle its row.
async fn execute_job(
    row: SqliteJobRow,
    jobs_by_name: &Arc<RwLock<HashMap<String, JobInfo>>>,
    pool: &SqlitePool,
    worker_id: &str,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) {
    let attempt = u32::try_from(row.attempt).unwrap_or(0);
    let max_attempts = u32::try_from(row.max_attempts).unwrap_or(1);
    let payload = row.payload_value();

    if job_admin.try_record_start(&row.id, attempt) == JobAdminStartDecision::Canceled {
        let ack = nack_failure(
            pool,
            now_ms(state),
            &row.id,
            worker_id,
            "canceled by operator",
            &row,
            None,
        )
        .await;
        record_pg_cancel_after_ack(ack, &row.name, &row.id, state);
        // The cancel reuses the ordinary retry-vs-dead-letter decision, so only
        // settle the tracked record when this really was the terminal attempt.
        if is_final_attempt(&attempt, &max_attempts) {
            crate::job_tracking::settle_tracked_payload_as_failed(
                state,
                &payload,
                "This job was canceled.",
            )
            .await;
        }
        return;
    }
    state.job_registry.record_start(&row.name);

    let job_info_snapshot = jobs_by_name
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&row.name)
        .map(|info| (info.handler, info.uniqueness.clone()));
    let pending_unique_key = job_info_snapshot
        .as_ref()
        .and_then(|(_, uniqueness)| uniqueness.as_ref())
        .filter(|unique| unique.window == JobUniquenessWindow::Pending)
        .map(|unique| job_unique_key(unique, &payload));
    let Some((handler, _)) = job_info_snapshot else {
        // No handler exists on this process, so requeueing would make every
        // worker claim and discard the row until its attempts ran out.
        let error = format!("unknown job '{}'", row.name);
        let ack = ack_dead_letter(pool, now_ms(state), &row.id, worker_id, &error).await;
        record_pg_lifecycle_ack_result(
            ack,
            &row.name,
            &row.id,
            "unknown-type",
            PgLifecycleRecord::Failure { error: &error },
            state,
            job_admin,
        );
        crate::job_tracking::settle_tracked_payload_as_failed(
            state,
            &payload,
            crate::job_tracking::GENERIC_FAILURE_MESSAGE,
        )
        .await;
        return;
    };

    let job_span = super::build_job_consumer_span(&row.name, attempt);
    super::restore_job_trace_context_for_backend(
        &job_span,
        row.traceparent.as_deref(),
        row.tracestate.as_deref(),
    );
    let final_attempt = is_final_attempt(&attempt, &max_attempts);
    let outcome = tracing::Instrument::instrument(
        run_job_handler(&row.name, handler, state.clone(), payload, final_attempt),
        job_span,
    )
    .await;
    settle_outcome(
        outcome,
        &row,
        pool,
        worker_id,
        state,
        job_admin,
        pending_unique_key.as_deref(),
        final_attempt,
    )
    .await;
}

/// Write a finished attempt back to the queue table and record it.
#[allow(clippy::too_many_arguments)]
async fn settle_outcome(
    outcome: JobExecutionOutcome,
    row: &SqliteJobRow,
    pool: &SqlitePool,
    worker_id: &str,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
    pending_unique_key: Option<&str>,
    final_attempt: bool,
) {
    let attempt = u32::try_from(row.attempt).unwrap_or(0);
    match outcome {
        JobExecutionOutcome::Succeeded => {
            let ack = ack_success(pool, now_ms(state), &row.id, worker_id).await;
            record_pg_lifecycle_ack_result(
                ack,
                &row.name,
                &row.id,
                "success",
                PgLifecycleRecord::Success,
                state,
                job_admin,
            );
        }
        JobExecutionOutcome::Failed(error) => {
            let lifecycle = if final_attempt {
                PgLifecycleRecord::Failure { error: &error }
            } else {
                // Mirror the `run_at = now + backoff` the retry UPDATE applies,
                // so the local gauge tracks the retry as scheduled until it is
                // claimable. A zero backoff is due now.
                let delay_ms = pg_retry_delay_ms(row.initial_backoff_ms, row.attempt);
                let ready_at_ms = (delay_ms > 0).then(|| {
                    u64::try_from(now_ms(state))
                        .unwrap_or(0)
                        .saturating_add(u64::try_from(delay_ms).unwrap_or(0))
                });
                PgLifecycleRecord::Retry {
                    error: &error,
                    attempt,
                    ready_at_ms,
                }
            };
            let ack = nack_failure(
                pool,
                now_ms(state),
                &row.id,
                worker_id,
                &error,
                row,
                pending_unique_key,
            )
            .await;
            record_pg_lifecycle_ack_result(
                ack, &row.name, &row.id, "failure", lifecycle, state, job_admin,
            );
        }
        // A panic dead-letters at once whatever the remaining attempts, as on
        // every other backend.
        JobExecutionOutcome::Panicked(error) => {
            tracing::error!(job = %row.name, error = %error, "sqlite job handler panicked");
            let ack = ack_dead_letter(pool, now_ms(state), &row.id, worker_id, &error).await;
            record_pg_lifecycle_ack_result(
                ack,
                &row.name,
                &row.id,
                "panic",
                PgLifecycleRecord::Failure { error: &error },
                state,
                job_admin,
            );
        }
    }
}

/// One worker loop: claim, run, settle, repeat.
#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    queue_handle: SqliteJobQueue,
    worker_id: String,
    jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>>,
    state: AppState,
    job_admin: JobAdminMemoryBackend,
    schedule: QueueSchedule,
    slots: Arc<QueueSlots>,
    poll_interval: std::time::Duration,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut cursor = schedule.cursor();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        // Retry the schema setup on every pass rather than giving up for the
        // life of the process: a first attempt can lose a `busy_timeout` race
        // with another process booting against the same file, and a worker that
        // returned here would drain nothing until someone restarted it.
        let Some(pool) = wait_ready(&queue_handle, poll_interval, &shutdown).await else {
            break;
        };
        // Walk the priority order and reserve a per-queue slot before the claim,
        // so a queue cap is honored across the claim round-trip.
        let order = cursor.next_order();
        let mut handled = false;
        for queue in slots.claimable(&order) {
            let Some(guard) = slots.try_reserve(&queue) else {
                continue;
            };
            match claim_next_job(&pool, &worker_id, &queue, now_ms(&state)).await {
                Some(row) => {
                    execute_job(row, &jobs_by_name, &pool, &worker_id, &state, &job_admin).await;
                    drop(guard);
                    handled = true;
                    break;
                }
                None => drop(guard),
            }
        }
        if !handled {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = queue_handle.wake.notified() => {}
                () = tokio::time::sleep(poll_interval) => {}
            }
        }
    }
}

/// Sweep stale claims on an interval, so a crashed worker's jobs come back.
async fn maintenance_loop(
    queue_handle: SqliteJobQueue,
    visibility_timeout_ms: u64,
    survey_blocked: bool,
    history_window: Option<std::time::Duration>,
    state: AppState,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let interval_duration = maintenance_interval(visibility_timeout_ms);
    let Some(pool) = wait_ready(&queue_handle, interval_duration, &shutdown).await else {
        return;
    };
    // Sweep once at start: a row still marked running belongs to a process that
    // is no longer here.
    recover_stale_claims(&pool, visibility_timeout_ms, &state).await;
    let mut tracking_cleanup = tokio::time::interval(TRACKING_CLEANUP_INTERVAL);
    tracking_cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut interval = tokio::time::interval(interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                recover_stale_claims(&pool, visibility_timeout_ms, &state).await;
                if survey_blocked {
                    update_concurrency_blocked_gauges(&pool, &state).await;
                }
            }
            _ = tracking_cleanup.tick() => {
                cleanup_expired_tracking_rows(&pool, &state).await;
                if let Some(window) = history_window {
                    prune_job_history(&pool, &state, window).await;
                }
            }
            () = shutdown.cancelled() => break,
        }
    }
}

/// Install the process-wide enqueue client, pointed at the durable queue.
fn install_enqueue_client(
    state: &AppState,
    queue_handle: &SqliteJobQueue,
    job_admin: &JobAdminMemoryBackend,
    per_job_settings: HashMap<String, super::JobRuntimeSettings>,
    config: &crate::config::JobConfig,
) {
    install_job_client(
        state,
        JobClient {
            local_sender: None,
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            sqlite: Some(queue_handle.clone()),
            registry: state.job_registry.clone(),
            job_admin: job_admin.clone(),
            default_max_attempts: config.max_attempts,
            default_initial_backoff_ms: config.initial_backoff_ms,
            per_job_settings,
            interceptor: state
                .extension::<Arc<dyn crate::interceptor::JobInterceptor>>()
                .map(|arc| (*arc).clone()),
            entropy: state.entropy_arc(),
            clock: state.clock_arc(),
            resilience_config: state
                .extension::<crate::config::AutumnConfig>()
                .map(|c| Arc::new(c.resilience.clone())),
        },
    );
}

/// Resolve which queues this process drains, in what order, with what caps.
///
/// Same rules as every other backend: configured priority first, then declared
/// queues, then the `[jobs] pin` subset for this process.
fn queue_topology(
    config: &crate::config::JobConfig,
    jobs_by_name: &Arc<RwLock<HashMap<String, JobInfo>>>,
    run_workers: bool,
) -> (QueueSchedule, Arc<QueueSlots>) {
    let (mut schedule, unconfigured) =
        QueueSchedule::effective(&config.queues, &collect_declared_queues(jobs_by_name));
    for queue in &unconfigured {
        tracing::warn!(
            queue = %queue,
            "job declares queue '{queue}' which is not in [jobs] queues; draining it at \
             lowest priority. Add it to the configured queue list to control its priority.",
        );
    }
    let uncovered = schedule.retain_pinned(&config.pin);
    // Only worker roles claim queues, so a web replica must not warn about
    // queues it will never drain.
    if should_warn_pin_coverage(run_workers, &config.pin) {
        warn_pinned_uncovered_queues(&uncovered, &config.pin, schedule.names().is_empty());
    }
    let mut limits = QueueLimits::from_config(&config.queues);
    limits.retain_queues(&schedule.names());
    let slots = QueueSlots::new(config.workers.max(1), limits);
    (schedule, slots)
}

/// Start the durable `SQLite` job runtime.
///
/// # Errors
///
/// Returns an error when no database pool is configured, or when the queue
/// schema cannot be created.
pub(super) fn start_runtime(
    jobs: Vec<JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    config: &crate::config::JobConfig,
    run_workers: bool,
) -> AutumnResult<()> {
    validate_unique_job_names(&jobs).map_err(|error| {
        AutumnError::internal_server_error_msg(format!("invalid jobs configuration: {error}"))
    })?;

    let pool = state.pool().cloned().ok_or_else(|| {
        AutumnError::internal_server_error_msg(
            "jobs.backend=sqlite requires a configured database; set database.url to a \
             sqlite:// target or call AppBuilder::with_pool()",
        )
    })?;

    let queue_handle = SqliteJobQueue::new(pool);
    let job_admin = JobAdminMemoryBackend::new().with_clock(state.clock_arc());
    let per_job_settings = build_per_job_settings(&jobs);
    let jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>> = Arc::new(RwLock::new(
        jobs.into_iter().map(|j| (j.name.clone(), j)).collect(),
    ));

    {
        let guard = jobs_by_name
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in guard.values() {
            state
                .job_registry
                .register_on_queue(&job.name, &normalize_queue_name(&job.queue));
        }
    }

    if job_admin_backend(state).is_none() {
        state.insert_extension(JobAdminBackendEntry(Arc::new(SqliteJobAdminBackend {
            queue: queue_handle.clone(),
            registry: state.job_registry.clone(),
            clock: state.clock_arc(),
        })));
    }

    let (schedule, slots) = queue_topology(config, &jobs_by_name, run_workers);

    install_enqueue_client(state, &queue_handle, &job_admin, per_job_settings, config);

    // Backend-derived actuator gauges: survey the table on an interval. Spawned
    // for ALL roles — before the web-role return below — so an enqueue-only
    // replica reports the shared backlog rather than its own local marks.
    {
        let queue_handle = queue_handle.clone();
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            queue_depth_survey_loop(queue_handle, state, shutdown).await;
        });
    }

    // A web replica installs the enqueue client but drains nothing: another
    // process on the host runs the workers.
    if !run_workers {
        return Ok(());
    }

    let visibility_timeout_ms = config.sqlite.visibility_timeout_ms;
    let survey_blocked = jobs_by_name
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .any(|job| job.concurrency.is_some());
    // Opt-in, and read from the same `retention.job_history` window the
    // Postgres sweep uses. Unset means history is kept forever, as on Postgres.
    let history_window = state
        .extension::<crate::config::AutumnConfig>()
        .and_then(|config| config.retention.job_history.clone())
        .and_then(|window| crate::config::parse_duration_str(&window).ok());
    let poll_interval = std::time::Duration::from_millis(config.sqlite.poll_interval_ms.max(1));
    let worker_count = config.workers.max(1);

    {
        let queue_handle = queue_handle.clone();
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            maintenance_loop(
                queue_handle,
                visibility_timeout_ms,
                survey_blocked,
                history_window,
                state,
                shutdown,
            )
            .await;
        });
    }

    for _ in 0..worker_count {
        let queue_handle = queue_handle.clone();
        let jobs_by_name = Arc::clone(&jobs_by_name);
        let state = state.clone();
        let job_admin = job_admin.clone();
        let shutdown = shutdown.clone();
        let schedule = schedule.clone();
        let slots = Arc::clone(&slots);
        tokio::spawn(async move {
            let worker_id = format!("{}:{}", std::process::id(), state.entropy().uuid_v4());
            worker_loop(
                queue_handle,
                worker_id,
                jobs_by_name,
                state,
                job_admin,
                schedule,
                slots,
                poll_interval,
                shutdown,
            )
            .await;
        });
    }

    Ok(())
}

/// Job dashboard backed by the `autumn_jobs` table.
///
/// Unlike the in-process default, this reports the whole queue, so every
/// process on the host sees the same picture.
struct SqliteJobAdminBackend {
    queue: SqliteJobQueue,
    /// Registry whose per-queue waiting gauges an admin cancel must decrement.
    registry: crate::actuator::JobRegistry,
    /// Injected clock for the snapshot window boundaries.
    clock: Arc<dyn crate::time::ClockSource>,
}

impl SqliteJobAdminBackend {
    async fn snapshot(&self, query: &JobAdminQuery) -> AutumnResult<JobAdminSnapshot> {
        let mut conn = self.queue.ready().await?.get().await.map_err(|error| {
            AutumnError::internal_server_error_msg(format!("sqlite admin pool error: {error}"))
        })?;
        let per_page = i64::try_from(query.per_page.clamp(1, 100)).unwrap_or(10);
        let now = self.clock.now().timestamp_millis();

        // `enqueued` and `scheduled` split one status by due time.
        let enqueued = admin_page(
            &mut conn,
            STATUS_ENQUEUED,
            Some(("run_at <= ?", now)),
            "enqueued_at",
            JobAdminStatus::Enqueued,
            query.enqueued_page,
            per_page,
        )
        .await?;
        let scheduled = admin_page(
            &mut conn,
            STATUS_ENQUEUED,
            Some(("run_at > ?", now)),
            "run_at",
            JobAdminStatus::Scheduled,
            query.scheduled_page,
            per_page,
        )
        .await?;
        let running = admin_page(
            &mut conn,
            STATUS_RUNNING,
            None,
            "started_at",
            JobAdminStatus::Running,
            query.running_page,
            per_page,
        )
        .await?;
        let completed = admin_page(
            &mut conn,
            STATUS_COMPLETED,
            Some(("finished_at >= ?", now.saturating_sub(24 * 3_600_000))),
            "finished_at",
            JobAdminStatus::Completed,
            query.completed_page,
            per_page,
        )
        .await?;
        let failed = admin_page(
            &mut conn,
            STATUS_FAILED,
            Some(("finished_at >= ?", now.saturating_sub(7 * 24 * 3_600_000))),
            "finished_at",
            JobAdminStatus::Failed,
            query.failed_page,
            per_page,
        )
        .await?;

        Ok(JobAdminSnapshot {
            enqueued,
            scheduled,
            running,
            completed,
            failed,
            schedules: Vec::new(),
            bounded_history_limit: DEFAULT_JOB_ADMIN_HISTORY_LIMIT,
        })
    }

    async fn retry_failed(&self, id: &str) -> AutumnResult<()> {
        use diesel::OptionalExtension as _;
        use diesel_async::RunQueryDsl as _;

        // Snapshot the tracking record before the UPDATE makes the retry
        // visible to workers, so the reset can detect a retry that finishes
        // faster than this function returns.
        //
        // The read is scoped so its checkout is released before the tracking
        // call below. The tracking store is backed by this same pool, and that
        // pool is one slot for a private in-memory target, so holding this
        // connection across the call would deadlock until
        // `database.connect_timeout_secs` and then silently skip the reset.
        let pre_retry_row = {
            let mut conn = self.queue.ready().await?.get().await.map_err(|error| {
                AutumnError::internal_server_error_msg(format!("sqlite admin pool error: {error}"))
            })?;
            diesel::sql_query(format!(
                "SELECT payload FROM autumn_jobs WHERE id = ? AND status = '{STATUS_FAILED}'"
            ))
            .bind::<diesel::sql_types::Text, _>(id)
            .get_result::<PayloadRow>(&mut *conn)
            .await
            .optional()
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "sqlite admin retry failed: {error}"
                ))
            })?
        };
        let retry_snapshot = match &pre_retry_row {
            Some(row) => {
                let payload = serde_json::from_str::<Value>(&row.payload).unwrap_or(Value::Null);
                crate::job_tracking::capture_retry_snapshot(&payload).await
            }
            None => None,
        };

        // A fresh checkout for the write, released again before
        // `apply_retry_reset` needs one.
        let mut conn = self.queue.ready().await?.get().await.map_err(|error| {
            AutumnError::internal_server_error_msg(format!("sqlite admin pool error: {error}"))
        })?;
        // The UPDATE also restores a `pending`-window dedup key: claiming moved
        // it to `pending_unique_key`, and re-enqueueing without it would leave
        // the retried job undeduplicated while it waits.
        let now = self.clock.now().timestamp_millis();
        let updated = diesel::sql_query(format!(
            "UPDATE autumn_jobs \
             SET status = '{STATUS_ENQUEUED}', attempt = 1, run_at = ?, enqueued_at = ?, \
                 started_at = NULL, finished_at = NULL, \
                 claimed_by = NULL, claimed_at = NULL, last_error = NULL, \
                 unique_key = CASE \
                   WHEN pending_unique_key IS NOT NULL AND NOT EXISTS ( \
                     SELECT 1 FROM autumn_jobs dup \
                     WHERE dup.name = autumn_jobs.name \
                       AND dup.unique_key = autumn_jobs.pending_unique_key \
                       AND dup.id != autumn_jobs.id \
                       AND dup.status IN ('{STATUS_ENQUEUED}', '{STATUS_RUNNING}')) \
                   THEN pending_unique_key \
                   ELSE unique_key \
                 END, \
                 pending_unique_key = NULL \
             WHERE id = ? AND status = '{STATUS_FAILED}' \
             RETURNING payload"
        ))
        .bind::<diesel::sql_types::BigInt, _>(now)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .bind::<diesel::sql_types::Text, _>(id)
        .get_result::<PayloadRow>(&mut *conn)
        .await
        .optional()
        .map_err(|error| {
            // The retried row keeps its unique key, so re-enqueueing while an
            // equivalent job is in flight trips the partial unique index.
            // Surface that as an actionable conflict. SQLite names the offending
            // columns ("UNIQUE constraint failed: autumn_jobs.name, …"), never
            // the index, so match on that rather than on the index name.
            if error.to_string().contains("UNIQUE constraint failed") {
                AutumnError::bad_request_msg(
                    "an equivalent unique job is already pending or running; \
                     retry after it settles",
                )
            } else {
                AutumnError::internal_server_error_msg(format!(
                    "sqlite admin retry failed: {error}"
                ))
            }
        })?;
        let Some(row) = updated else {
            return Err(AutumnError::not_found_msg(format!(
                "job '{id}' not found or not in failed state"
            )));
        };
        // Same pool, same reason as above: let the write's checkout go before
        // the tracking store asks for one.
        drop(conn);
        // The record is terminal from the original run. Reset it to pending so
        // the retried attempt's progress calls surface.
        let payload = serde_json::from_str::<Value>(&row.payload).unwrap_or(Value::Null);
        crate::job_tracking::apply_retry_reset(&payload, retry_snapshot).await;
        Ok(())
    }

    async fn discard_failed(&self, id: &str) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.queue.ready().await?.get().await.map_err(|error| {
            AutumnError::internal_server_error_msg(format!("sqlite admin pool error: {error}"))
        })?;
        let updated = diesel::sql_query(format!(
            "UPDATE autumn_jobs SET status = '{STATUS_DISCARDED}', finished_at = ? \
             WHERE id = ? AND status = '{STATUS_FAILED}'"
        ))
        .bind::<diesel::sql_types::BigInt, _>(self.clock.now().timestamp_millis())
        .bind::<diesel::sql_types::Text, _>(id)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("sqlite admin discard failed: {error}"))
        })?;
        if updated == 0 {
            return Err(AutumnError::not_found_msg(format!(
                "job '{id}' not found or not in failed state"
            )));
        }
        Ok(())
    }

    async fn cancel_enqueued(&self, id: &str) -> AutumnResult<()> {
        use diesel::OptionalExtension as _;
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.queue.ready().await?.get().await.map_err(|error| {
            AutumnError::internal_server_error_msg(format!("sqlite admin pool error: {error}"))
        })?;
        let now = self.clock.now().timestamp_millis();
        // The `status = 'enqueued'` guard means this only ever transitions a
        // still-unclaimed row, so the gauge decrement below can never
        // double-count against the cancel a claimed job records at ack time.
        let cancelled = diesel::sql_query(format!(
            "UPDATE autumn_jobs SET status = '{STATUS_DISCARDED}', finished_at = ? \
             WHERE id = ? AND status = '{STATUS_ENQUEUED}' \
             RETURNING payload, name, run_at"
        ))
        .bind::<diesel::sql_types::BigInt, _>(now)
        .bind::<diesel::sql_types::Text, _>(id)
        .get_result::<CancelRow>(&mut *conn)
        .await
        .optional()
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("sqlite admin cancel failed: {error}"))
        })?;
        let Some(row) = cancelled else {
            return Err(AutumnError::not_found_msg(format!(
                "job '{id}' not found or not in enqueued state"
            )));
        };
        // Remove the per-queue waiting mark this job pushed at enqueue time, or
        // a phantom depth lingers on this process. A still-future `run_at` was
        // a scheduled mark, a ready one a ready mark.
        if row.run_at > now {
            self.registry.record_cancel_scheduled(&row.name);
        } else {
            self.registry.record_cancel(&row.name);
        }
        // Release the checkout before the tracking store asks this same pool
        // for one. That pool is a single slot for a private in-memory target,
        // so holding it here would stall the cancel until
        // `database.connect_timeout_secs` and then report success with the
        // job's public status still pending.
        drop(conn);
        // An operator can cancel before any worker claims the job, which never
        // reaches the handler — settle the tracked record here too.
        let payload = serde_json::from_str::<Value>(&row.payload).unwrap_or(Value::Null);
        crate::job_tracking::settle_tracked_payload_as_failed_globally(
            &payload,
            "This job was canceled.",
        )
        .await;
        Ok(())
    }
}

/// Row returned when an admin cancel discards a still-enqueued job. Carries
/// what settling the tracked record and decrementing the right gauge need.
#[derive(diesel::QueryableByName)]
struct CancelRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    payload: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    run_at: i64,
}

/// One paginated status group of the dashboard.
///
/// `sort_col` and `extra` are `&'static str` literals from this module's own
/// call sites, never user input, so embedding them is safe.
async fn admin_page(
    conn: &mut RuntimeConnection,
    status: &str,
    extra: Option<(&'static str, i64)>,
    sort_col: &'static str,
    admin_status: JobAdminStatus,
    page: u64,
    per_page: i64,
) -> AutumnResult<JobAdminPage> {
    use diesel_async::RunQueryDsl as _;

    let page = page.max(1);
    let offset = i64::try_from(
        page.saturating_sub(1)
            .saturating_mul(u64::try_from(per_page).unwrap_or(10)),
    )
    .unwrap_or(0);
    let filter = extra.map_or_else(String::new, |(clause, _)| format!(" AND {clause}"));
    // A scheduled list reads best soonest-due first; every other list is
    // newest-first.
    let direction = if admin_status == JobAdminStatus::Scheduled {
        "ASC"
    } else {
        "DESC"
    };

    let count_query = diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM autumn_jobs WHERE status = ?{filter}"
    ))
    .bind::<diesel::sql_types::Text, _>(status);
    let total = match extra {
        Some((_, value)) => {
            count_query
                .bind::<diesel::sql_types::BigInt, _>(value)
                .get_result::<CountRow>(&mut *conn)
                .await
        }
        None => count_query.get_result::<CountRow>(&mut *conn).await,
    }
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite admin count: {error}"))
    })?
    .count;

    let rows_query = diesel::sql_query(format!(
        "SELECT {JOB_SELECT_COLS} FROM autumn_jobs WHERE status = ?{filter} \
         ORDER BY {sort_col} {direction} LIMIT ? OFFSET ?"
    ))
    .bind::<diesel::sql_types::Text, _>(status);
    let rows = match extra {
        Some((_, value)) => {
            rows_query
                .bind::<diesel::sql_types::BigInt, _>(value)
                .bind::<diesel::sql_types::BigInt, _>(per_page)
                .bind::<diesel::sql_types::BigInt, _>(offset)
                .load::<SqliteJobRow>(&mut *conn)
                .await
        }
        None => {
            rows_query
                .bind::<diesel::sql_types::BigInt, _>(per_page)
                .bind::<diesel::sql_types::BigInt, _>(offset)
                .load::<SqliteJobRow>(&mut *conn)
                .await
        }
    }
    .map_err(|error| {
        AutumnError::internal_server_error_msg(format!("sqlite admin page: {error}"))
    })?;

    Ok(JobAdminPage::new(
        rows.iter()
            .map(|row| row.to_admin_record(admin_status))
            .collect(),
        u64::try_from(total).unwrap_or(0),
        page,
        u64::try_from(per_page).unwrap_or(10),
    ))
}

impl JobAdminBackend for SqliteJobAdminBackend {
    fn snapshot(&self, query: JobAdminQuery) -> JobAdminFuture<'_, JobAdminSnapshot> {
        Box::pin(async move { self.snapshot(&query).await })
    }

    fn retry(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.retry_failed(&id).await })
    }

    fn discard(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.discard_failed(&id).await })
    }

    fn cancel(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.cancel_enqueued(&id).await })
    }
}
