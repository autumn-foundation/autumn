//! Tracked job handles: unguessable-token status polling for `#[job]`.
//!
//! [`enqueue_tracked`] hands the caller a
//! [`TrackedJobHandle`] carrying a public, unguessable token distinct from the
//! internal job id. Inside the job handler, [`JobContext::current`] exposes
//! progress reporting (`set_progress`) and lets the handler record a terminal
//! result or a user-safe error. A [`JobTrackingStore`] persists that state,
//! keyed by a hash of the token, with a configurable TTL.

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
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use axum::response::IntoResponse as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::time::{ClockSource, SystemClock};
use crate::{AppState, AutumnError, AutumnResult};

/// Who is allowed to poll a tracked job's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackedJobOwner {
    /// No owner bound — the token itself is the capability.
    Anonymous,
    /// Bound to a specific (unauthenticated) session id.
    Session(String),
    /// Bound to an authenticated user/principal id.
    User(String),
}

impl TrackedJobOwner {
    /// Derive an owner binding from the current request's session: the
    /// authenticated user id if logged in (per `state.auth_session_key()`),
    /// else the raw (anonymous) session id.
    ///
    /// Binding to the session id means only *this* browser session — not
    /// other anonymous callers, and not other sessions once the user logs in
    /// elsewhere — may poll the status.
    pub async fn from_session(session: &crate::session::Session, state: &AppState) -> Self {
        if let Some(user_id) = session.get(state.auth_session_key()).await {
            return Self::User(user_id);
        }
        // A session with no prior cookie is only persisted (and only gets a
        // Set-Cookie on the response) if something dirties it during this
        // request. Reading `session.id()` alone does not — so without
        // forcing it, the id we bind to here would never actually reach the
        // browser as a cookie, and the next poll request would present a
        // different session entirely, getting the same 404 as an
        // unauthorized caller.
        if !session.is_cookie_backed().await {
            session.touch().await;
        }
        Self::Session(session.id().await)
    }

    /// Whether a request authenticated as `session` may poll a record bound
    /// to this owner.
    async fn authorizes(&self, session: &crate::session::Session, state: &AppState) -> bool {
        match self {
            Self::Anonymous => true,
            Self::User(expected) => {
                session.get(state.auth_session_key()).await.as_deref() == Some(expected.as_str())
            }
            Self::Session(expected) => &session.id().await == expected,
        }
    }
}

/// Lifecycle status of a tracked job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackedJobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl TrackedJobStatus {
    /// Terminal statuses stop htmx polling and are subject to TTL expiry.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// A snapshot of a tracked job's progress and (if terminal) its result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedJobRecord {
    pub status: TrackedJobStatus,
    pub progress_pct: Option<u8>,
    pub progress_message: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub owner: TrackedJobOwner,
    pub updated_at: DateTime<Utc>,
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ── Shared record-mutation logic ──────────────────────────────────────────────
//
// Each `JobTrackingStore` impl below applies these same transitions through
// its own read-modify-write mechanics (in-memory mutex, Redis GET+SET,
// Postgres SELECT+UPDATE); sharing the transition logic itself means the
// three backends can never silently drift on what a given status change
// actually does to a record.

const fn apply_mark_running(record: &mut TrackedJobRecord) {
    if !record.status.is_terminal() {
        record.status = TrackedJobStatus::Running;
    }
}

fn apply_set_progress(record: &mut TrackedJobRecord, pct: u8, message: Option<String>) {
    if !record.status.is_terminal() {
        record.progress_pct = Some(pct);
        record.progress_message = message;
    }
}

fn apply_complete(record: &mut TrackedJobRecord, result: Value) {
    record.status = TrackedJobStatus::Succeeded;
    record.result = Some(result);
    record.error = None;
}

fn apply_fail(record: &mut TrackedJobRecord, error: String) {
    record.status = TrackedJobStatus::Failed;
    record.error = Some(error);
    record.result = None;
}

/// Persists tracked-job progress/result, keyed by a hash of the public token.
///
/// Dyn-safe (boxed-future methods) so it can be installed as an
/// `Arc<dyn JobTrackingStore>` `AppState` extension, mirroring
/// [`crate::auth::ApiTokenStore`] and [`crate::job::JobAdminBackend`].
pub trait JobTrackingStore: Send + Sync + 'static {
    /// Create a new pending record for `key` (the token hash).
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>>;

    /// Transition a pending record to running.
    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>>;

    /// Record progress. `pct` is clamped to `0..=100`.
    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>>;

    /// Mark the record succeeded with a small JSON result payload.
    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>>;

    /// Mark the record failed with a user-safe error message.
    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>>;

    /// Fetch the current record, or `None` if unknown or expired.
    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>>;

    /// Reset `key` back to a fresh `pending` record for a retried attempt —
    /// but only if the stored record's `updated_at` still equals
    /// `expected_updated_at` (the value read just before the retry decision
    /// was made). If a worker has already re-executed and settled the
    /// record in the meantime, `updated_at` will have moved on and this is a
    /// no-op, so a fast retry can never clobber the fresher terminal write
    /// with a stale `pending` reset.
    fn reset_for_retry<'a>(
        &'a self,
        key: &'a str,
        owner: TrackedJobOwner,
        expected_updated_at: DateTime<Utc>,
    ) -> BoxFut<'a, AutumnResult<()>>;
}

/// `AppState` extension carrying the installed [`JobTrackingStore`].
#[derive(Clone)]
pub struct JobTrackingStoreEntry(pub Arc<dyn JobTrackingStore>);

// ── Job context (progress reporting from inside a handler) ───────────────────

tokio::task_local! {
    static CURRENT_JOB_CONTEXT: JobContext;
}

/// A generic, user-safe failure message persisted when a tracked job's
/// handler fails or panics without calling
/// [`JobContext::set_user_error`].
pub(crate) const GENERIC_FAILURE_MESSAGE: &str = "The job failed.";

struct JobContextInner {
    key: String,
    store: Arc<dyn JobTrackingStore>,
    result: Mutex<Option<Value>>,
    user_error: Mutex<Option<String>>,
}

/// Ambient handle a `#[job]` handler uses to report progress and to record a
/// terminal result or a user-safe error for a tracked job.
///
/// [`JobContext::current`] always returns a value. For a job enqueued via
/// plain [`crate::job::enqueue`] (not tracked), it is a no-op: every method is
/// a harmless no-op and [`is_tracked`](Self::is_tracked) reports `false`.
#[derive(Clone)]
pub struct JobContext(Option<Arc<JobContextInner>>);

impl JobContext {
    pub(crate) fn tracked(key: String, store: Arc<dyn JobTrackingStore>) -> Self {
        Self(Some(Arc::new(JobContextInner {
            key,
            store,
            result: Mutex::new(None),
            user_error: Mutex::new(None),
        })))
    }

    pub(crate) const fn none() -> Self {
        Self(None)
    }

    /// The ambient context for the currently-executing job, or a no-op
    /// context when called outside a job or for an untracked job.
    #[must_use]
    pub fn current() -> Self {
        CURRENT_JOB_CONTEXT
            .try_with(Clone::clone)
            .unwrap_or_else(|_| Self::none())
    }

    /// Whether this context is bound to a tracked job's status record.
    #[must_use]
    pub const fn is_tracked(&self) -> bool {
        self.0.is_some()
    }

    /// Report progress. `pct` is clamped to `0..=100`. A no-op for an
    /// untracked context.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying store write fails.
    pub async fn set_progress(&self, pct: u8, message: Option<&str>) -> AutumnResult<()> {
        let Some(inner) = &self.0 else {
            return Ok(());
        };
        inner
            .store
            .set_progress(&inner.key, pct, message.map(str::to_owned))
            .await
    }

    /// Record the JSON result to persist when the job succeeds. A no-op for
    /// an untracked context.
    ///
    /// # Panics
    ///
    /// Panics if the internal result mutex is poisoned (only possible if a
    /// previous holder panicked while holding it).
    pub fn set_result(&self, result: Value) {
        if let Some(inner) = &self.0 {
            *inner
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        }
    }

    /// Record the user-safe error message to persist if the job ultimately
    /// fails (its last attempt, or a panic). A no-op for an untracked
    /// context.
    ///
    /// # Panics
    ///
    /// Panics if the internal error mutex is poisoned (only possible if a
    /// previous holder panicked while holding it).
    pub fn set_user_error(&self, message: impl Into<String>) {
        if let Some(inner) = &self.0 {
            *inner
                .user_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.into());
        }
    }

    /// Persist the terminal success result. A no-op for an untracked context.
    pub(crate) async fn settle_success(&self) {
        let Some(inner) = &self.0 else {
            return;
        };
        let result = inner
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or(Value::Null);
        let _ = inner.store.complete(&inner.key, result).await;
    }

    /// Persist the terminal failure, using `default_message` if the handler
    /// never called [`Self::set_user_error`]. A no-op for an untracked
    /// context.
    pub(crate) async fn settle_failure(&self, default_message: &str) {
        let Some(inner) = &self.0 else {
            return;
        };
        let message = inner
            .user_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| default_message.to_owned());
        let _ = inner.store.fail(&inner.key, message).await;
    }
}

/// Run `future` with `ctx` as the ambient [`JobContext`], so
/// [`JobContext::current`] resolves it from anywhere inside `future`
/// (including across `.await` points and nested async calls).
pub(crate) async fn scope<F: Future>(ctx: JobContext, future: F) -> F::Output {
    CURRENT_JOB_CONTEXT.scope(ctx, future).await
}

// ── Tracked-payload envelope ──────────────────────────────────────────────────

const ENVELOPE_MARKER: &str = "__autumn_tracked";
const ENVELOPE_KEY: &str = "k";
const ENVELOPE_ARGS: &str = "args";

/// Wrap `args` in the tracked-job envelope carrying the tracking key (a hash
/// of the raw token — never the raw token itself, so a leaked payload, admin
/// record, or queue dump never exposes the polling capability).
pub(crate) fn wrap_tracked_payload(key: &str, args: &Value) -> Value {
    serde_json::json!({
        ENVELOPE_MARKER: { ENVELOPE_KEY: key },
        ENVELOPE_ARGS: args,
    })
}

/// If `payload` is a tracked-job envelope, remove and return `(Some(key),
/// inner_args)`; otherwise return `(None, payload)` unchanged.
///
/// This is the single place a job handler's payload is unwrapped before
/// execution — called from the one choke point all three job backends run
/// handlers through.
pub(crate) fn take_tracked_payload(payload: Value) -> (Option<String>, Value) {
    let Value::Object(mut obj) = payload else {
        return (None, payload);
    };
    let key = obj
        .get(ENVELOPE_MARKER)
        .and_then(Value::as_object)
        .and_then(|marker| marker.get(ENVELOPE_KEY))
        .and_then(Value::as_str)
        .map(str::to_owned);
    match key {
        Some(key) => {
            let inner = obj.remove(ENVELOPE_ARGS).unwrap_or(Value::Null);
            (Some(key), inner)
        }
        None => (None, Value::Object(obj)),
    }
}

/// Borrowing counterpart of [`take_tracked_payload`], for callers (uniqueness
/// hashing, principal/correlation extraction) that only need to read fields
/// off the inner args without consuming the payload.
///
/// Agrees with [`take_tracked_payload`] on the fallback for a malformed
/// envelope (marker present, `args` missing): both report an empty
/// (`Value::Null`) inner payload rather than one falling back to the whole
/// wrapper while the other falls back to nothing.
pub(crate) fn split_tracked_payload(payload: &Value) -> (Option<&str>, &Value) {
    static NULL_ARGS: Value = Value::Null;
    let Some(obj) = payload.as_object() else {
        return (None, payload);
    };
    let Some(key) = obj
        .get(ENVELOPE_MARKER)
        .and_then(Value::as_object)
        .and_then(|marker| marker.get(ENVELOPE_KEY))
        .and_then(Value::as_str)
    else {
        return (None, payload);
    };
    (Some(key), obj.get(ENVELOPE_ARGS).unwrap_or(&NULL_ARGS))
}

// ── Global tracking-store install/resolve ─────────────────────────────────────

static GLOBAL_TRACKING_STORE: OnceLock<RwLock<Option<Arc<dyn JobTrackingStore>>>> = OnceLock::new();

const DEFAULT_TRACKING_TTL_SECS: u64 = 86_400;

/// Install `store` as this app's tracking store: both an `AppState`
/// extension (for [`tracking_store_from_state`], used where a state is
/// already in hand — e.g. inside the job-execution choke point) and the
/// process-global fallback used by the free `enqueue_tracked` functions,
/// which have no `AppState` to resolve an extension from — mirroring
/// [`crate::job::install_job_client`].
pub(crate) fn install_tracking_store(state: &AppState, store: Arc<dyn JobTrackingStore>) {
    state.insert_extension(JobTrackingStoreEntry(store.clone()));
    let lock = GLOBAL_TRACKING_STORE.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = lock.write() {
        *guard = Some(store);
    }
}

/// Install a default in-memory tracking store if this app doesn't already
/// have one installed. Called whenever a job runtime starts so
/// `enqueue_tracked` works even when a `JobClient` is constructed directly
/// (as the backend starters and their tests do) rather than through
/// `crate::job::start_runtime`, which installs a config-driven store first
/// (making this a no-op fallback in that path).
///
/// Also reinstalls when the process-global fallback is missing even though
/// `state` already carries an extension: `crate::job::clear_global_job_client`
/// resets [`GLOBAL_TRACKING_STORE`] but has no `AppState` to strip the
/// now-stale extension from, so relying on the extension alone would leave
/// the free `enqueue_tracked` functions permanently unable to resolve a
/// store after a clear-and-restart cycle that reuses the same `AppState`.
pub(crate) fn ensure_tracking_store_installed(state: &AppState) {
    if tracking_store_from_state(state).is_none() || global_tracking_store().is_none() {
        install_tracking_store(
            state,
            Arc::new(InMemoryJobTrackingStore::new(DEFAULT_TRACKING_TTL_SECS)),
        );
    }
}

/// Durable tracked-job store for the `SQLite` backend (issue #1907).
///
/// Same contract as [`PgJobTrackingStore`], over a table in the app's own
/// database file, so a tracked job's status survives a restart and is visible
/// to every process on the host — which a web/worker split needs. Timestamps
/// are epoch milliseconds, from the injected clock, matching the durable
/// `SQLite` job queue.
///
/// The runtime creates the table on first use: framework migrations are
/// Postgres SQL and do not run on `SQLite`.
#[cfg(feature = "sqlite")]
pub struct SqliteJobTrackingStore {
    pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
    ttl_secs: u64,
    clock: Arc<dyn ClockSource>,
    schema: Arc<tokio::sync::OnceCell<()>>,
}

#[cfg(feature = "sqlite")]
#[derive(diesel::QueryableByName)]
struct SqliteTrackingRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    record: String,
    /// The token the compare-and-swap in `try_update_once` writes against.
    ///
    /// A counter, not the timestamp: two writes inside one millisecond — or any
    /// write under a clock that does not advance, which is every `#[sim_test]`
    /// — leave `updated_at` unchanged, so a stale writer's swap would still
    /// match and overwrite the fresher record.
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    version: i64,
}

/// How many times a tracked-job update re-reads after losing its swap.
///
/// Only overlapping attempts of one job contend, so a couple of rounds is
/// plenty; the bound is what keeps a pathological loop finite.
#[cfg(feature = "sqlite")]
const CAS_RETRIES: usize = 5;

#[cfg(feature = "sqlite")]
impl SqliteJobTrackingStore {
    /// Construct a store backed by `pool`, expiring records `ttl_secs` after
    /// their last write.
    #[must_use]
    pub fn new(
        pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
        ttl_secs: u64,
    ) -> Self {
        Self {
            pool,
            ttl_secs,
            clock: Arc::new(SystemClock),
            schema: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Replace the clock used to stamp writes and evaluate expiry.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    fn now_ms(&self) -> i64 {
        self.clock.now().timestamp_millis()
    }

    fn expires_at_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_add(
            i64::try_from(self.ttl_secs)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000),
        )
    }

    /// Check out a connection, creating the table on the first use.
    ///
    /// A failed attempt leaves the cell empty, so the next call retries.
    async fn conn(
        &self,
    ) -> AutumnResult<diesel_async::pooled_connection::deadpool::Object<crate::db::RuntimeConnection>>
    {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.pool.get().await.map_err(|error| {
            AutumnError::internal_server_error_msg(format!("job tracking pool error: {error}"))
        })?;
        if self.schema.initialized() {
            return Ok(conn);
        }
        for statement in [
            "CREATE TABLE IF NOT EXISTS autumn_job_tracking ( \
               key        TEXT   PRIMARY KEY NOT NULL, \
               record     TEXT   NOT NULL, \
               updated_at BIGINT NOT NULL, \
               expires_at BIGINT NOT NULL, \
               version    BIGINT NOT NULL DEFAULT 0)",
            "CREATE INDEX IF NOT EXISTS idx_autumn_job_tracking_expires_at \
             ON autumn_job_tracking (expires_at)",
        ] {
            diesel::sql_query(statement)
                .execute(&mut *conn)
                .await
                .map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "job tracking schema setup failed: {error}"
                    ))
                })?;
        }
        // A table an earlier build created has no `version`. SQLite has no
        // `ADD COLUMN IF NOT EXISTS`, so the error is the check — but only the
        // duplicate-column case means "already migrated"; anything else must
        // propagate and leave the cell retryable.
        if let Err(error) = diesel::sql_query(
            "ALTER TABLE autumn_job_tracking ADD COLUMN version BIGINT NOT NULL DEFAULT 0",
        )
        .execute(&mut *conn)
        .await
            && !error.to_string().contains("duplicate column name")
        {
            return Err(AutumnError::internal_server_error_msg(format!(
                "job tracking schema setup failed: {error}"
            )));
        }
        let _ = self.schema.set(());
        Ok(conn)
    }

    /// Read-modify-write under a compare-and-swap. A no-op if the key is
    /// unknown or expired.
    ///
    /// Delivery is at-least-once, so two attempts of one job can overlap after
    /// a visibility timeout. Without the swap the older attempt could read a
    /// running record, the newer one write `succeeded`, and the older one's
    /// blind `UPDATE` put `running` back — leaving a finished job reporting as
    /// running forever. `SQLite` serializes the writes but not the `SELECT`
    /// before them, so the guard has to be in the statement.
    ///
    /// A lost swap re-reads and reapplies rather than dropping the write: the
    /// mutation may be a `complete`, which must not be lost. Reapplying cannot
    /// clobber the winner, because `try_update_once` leaves an already-settled
    /// record alone. `f` is therefore `Fn`, not `FnOnce`.
    async fn update(
        &self,
        key: &str,
        f: impl Fn(&mut TrackedJobRecord) + Send + Sync,
    ) -> AutumnResult<()> {
        for _ in 0..CAS_RETRIES {
            if self.try_update_once(key, &f).await? {
                return Ok(());
            }
        }
        // Never `Ok(())`: the caller would take a dropped `complete` or `fail`
        // for a settled job, and the status endpoint would sit at running until
        // the record expired.
        Err(AutumnError::internal_server_error_msg(format!(
            "job tracking update lost its compare-and-swap {CAS_RETRIES} times"
        )))
    }

    /// One read-modify-write attempt. Returns whether the swap landed.
    async fn try_update_once(
        &self,
        key: &str,
        // `&F` crosses an await, so `F` has to be `Sync` as well as `Send`.
        f: &(impl Fn(&mut TrackedJobRecord) + Send + Sync),
    ) -> AutumnResult<bool> {
        use diesel::OptionalExtension as _;
        use diesel_async::RunQueryDsl as _;

        // One clock sample for both the record's `updated_at` and the column.
        // `reset_for_retry` compares the column against the value it read out
        // of the record, so two samples that straddle a millisecond make that
        // compare-and-swap match nothing and leave an admin-retried job stuck.
        let now = self.clock.now();
        let now_ms = now.timestamp_millis();
        let mut conn = self.conn().await?;
        let row = diesel::sql_query(
            "SELECT record, version FROM autumn_job_tracking \
             WHERE key = ? AND expires_at > ?",
        )
        .bind::<diesel::sql_types::Text, _>(key)
        .bind::<diesel::sql_types::BigInt, _>(now_ms)
        .get_result::<SqliteTrackingRow>(&mut *conn)
        .await
        .optional()
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("job tracking select failed: {error}"))
        })?;

        // Nothing to update, and nothing to retry.
        let Some(row) = row else {
            return Ok(true);
        };
        let mut record =
            serde_json::from_str::<TrackedJobRecord>(&row.record).map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking deserialize failed: {error}"
                ))
            })?;
        // A settled record is final. `apply_complete` and `apply_fail` replace
        // the status unconditionally — unlike `apply_set_progress`, which
        // already checks — so without this a stale attempt of the same job
        // could flip the authoritative attempt's `failed` to `succeeded`, or
        // the reverse. Delivery is at-least-once, so that overlap is ordinary.
        // Only `reset_for_retry` moves a record out of a terminal state, and it
        // does not come through here.
        if record.status.is_terminal() {
            return Ok(true);
        }
        f(&mut record);
        record.updated_at = now;
        let payload = serde_json::to_string(&record).map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "job tracking serialize failed: {error}"
            ))
        })?;

        // `version = ?` is the swap, and the write bumps it: it matches only
        // while no one else has written since the row above was read.
        let written = diesel::sql_query(
            "UPDATE autumn_job_tracking \
             SET record = ?, updated_at = ?, expires_at = ?, version = version + 1 \
             WHERE key = ? AND version = ?",
        )
        .bind::<diesel::sql_types::Text, _>(&payload)
        .bind::<diesel::sql_types::BigInt, _>(now_ms)
        .bind::<diesel::sql_types::BigInt, _>(self.expires_at_ms(now_ms))
        .bind::<diesel::sql_types::Text, _>(key)
        .bind::<diesel::sql_types::BigInt, _>(row.version)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("job tracking update failed: {error}"))
        })?;
        Ok(written > 0)
    }

    /// Serialize a fresh pending record for `owner`, stamped `now`.
    ///
    /// Takes the instant rather than reading the clock, so the caller writes
    /// the same value into the record and the column.
    fn pending_record(owner: TrackedJobOwner, now: DateTime<Utc>) -> AutumnResult<String> {
        let record = TrackedJobRecord {
            status: TrackedJobStatus::Pending,
            progress_pct: None,
            progress_message: None,
            result: None,
            error: None,
            owner,
            updated_at: now,
        };
        serde_json::to_string(&record).map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "job tracking serialize failed: {error}"
            ))
        })
    }
}

#[cfg(feature = "sqlite")]
impl JobTrackingStore for SqliteJobTrackingStore {
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            use diesel_async::RunQueryDsl as _;

            let now = self.clock.now();
            let now_ms = now.timestamp_millis();
            let payload = Self::pending_record(owner, now)?;
            let mut conn = self.conn().await?;
            diesel::sql_query(
                "INSERT INTO autumn_job_tracking (key, record, updated_at, expires_at, version) \
                 VALUES (?, ?, ?, ?, 0) \
                 ON CONFLICT(key) DO UPDATE SET \
                     record = excluded.record, \
                     updated_at = excluded.updated_at, \
                     expires_at = excluded.expires_at, \
                     version = autumn_job_tracking.version + 1",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::Text, _>(&payload)
            .bind::<diesel::sql_types::BigInt, _>(now_ms)
            .bind::<diesel::sql_types::BigInt, _>(self.expires_at_ms(now_ms))
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking insert failed: {error}"
                ))
            })?;
            Ok(())
        })
    }

    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move { self.update(key, apply_mark_running).await })
    }

    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let pct = pct.min(100);
            self.update(key, |record| {
                apply_set_progress(record, pct, message.clone());
            })
            .await
        })
    }

    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.update(key, |record| apply_complete(record, result.clone()))
                .await
        })
    }

    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.update(key, |record| apply_fail(record, error.clone()))
                .await
        })
    }

    fn reset_for_retry<'a>(
        &'a self,
        key: &'a str,
        owner: TrackedJobOwner,
        expected_updated_at: DateTime<Utc>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            use diesel_async::RunQueryDsl as _;

            let now = self.clock.now();
            let now_ms = now.timestamp_millis();
            let payload = Self::pending_record(owner, now)?;
            let mut conn = self.conn().await?;
            // Compare-and-swap: the reset applies only while nothing has
            // written since `expected_updated_at` was read, so a retry that
            // settles faster than this call returns is never clobbered.
            diesel::sql_query(
                "UPDATE autumn_job_tracking \
                 SET record = ?, updated_at = ?, expires_at = ?, version = version + 1 \
                 WHERE key = ? AND updated_at = ?",
            )
            .bind::<diesel::sql_types::Text, _>(&payload)
            .bind::<diesel::sql_types::BigInt, _>(now_ms)
            .bind::<diesel::sql_types::BigInt, _>(self.expires_at_ms(now_ms))
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::BigInt, _>(expected_updated_at.timestamp_millis())
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking reset failed: {error}"
                ))
            })?;
            Ok(())
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>> {
        Box::pin(async move {
            use diesel::OptionalExtension as _;
            use diesel_async::RunQueryDsl as _;

            let now_ms = self.now_ms();
            let mut conn = self.conn().await?;
            let row = diesel::sql_query(
                "SELECT record, version FROM autumn_job_tracking \
                 WHERE key = ? AND expires_at > ?",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::BigInt, _>(now_ms)
            .get_result::<SqliteTrackingRow>(&mut *conn)
            .await
            .optional()
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking select failed: {error}"
                ))
            })?;

            row.map(|row| {
                serde_json::from_str::<TrackedJobRecord>(&row.record).map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "job tracking deserialize failed: {error}"
                    ))
                })
            })
            .transpose()
        })
    }
}

/// Build the tracking store matching `config.backend`.
///
/// Honors `config.tracking.ttl_secs`. Redis when `backend = "redis"` and a
/// valid URL is configured; `SQLite` when `backend = "sqlite"` and `state` has
/// a pool; Postgres when `backend = "postgres"` and `state` has a pool;
/// in-memory otherwise — including as a fallback when the selected backend is
/// not actually reachable or configured, which is logged rather than fatal,
/// since the job runtime itself raises the real error for that case.
fn store_for_config(
    state: &AppState,
    config: &crate::config::JobConfig,
) -> Arc<dyn JobTrackingStore> {
    // Only read when the `db` feature's match arm below exists; without it
    // (e.g. `redis`-only builds) `state` would otherwise be unused.
    let _ = state;
    match config.backend.as_str() {
        #[cfg(feature = "redis")]
        "redis" => {
            if let Some(store) = build_redis_tracking_store(config) {
                return Arc::new(store);
            }
            tracing::warn!(
                "jobs.backend=redis but jobs.redis.url is not configured; falling back to an \
                 in-memory job tracking store (tracked job status will not survive a restart)"
            );
        }
        // The durable SQLite queue keeps tracked-job records in the same file,
        // so a status survives a restart and every process on the host reads
        // the same record — which a web/worker split needs (issue #1907).
        #[cfg(feature = "sqlite")]
        "sqlite" => {
            if let Some(pool) = state.pool() {
                return Arc::new(
                    SqliteJobTrackingStore::new(pool.clone(), config.tracking.ttl_secs)
                        .with_clock(state.clock_arc()),
                );
            }
            tracing::warn!(
                "jobs.backend=sqlite but no database pool is configured; falling back to an \
                 in-memory job tracking store (tracked job status will not survive a restart)"
            );
        }
        // The Postgres tracking store persists to a Postgres table; under the
        // `sqlite` feature `state.pool()` is a SQLite pool that cannot satisfy
        // its Postgres connection type. The Postgres job backend itself is
        // refused earlier under sqlite (see `start_postgres_runtime`), so this
        // arm simply does not exist there.
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
        "postgres" => {
            if let Some(pool) = state.pool() {
                return Arc::new(PgJobTrackingStore::new(
                    pool.clone(),
                    config.tracking.ttl_secs,
                ));
            }
            tracing::warn!(
                "jobs.backend=postgres but no database pool is configured; falling back to an \
                 in-memory job tracking store (tracked job status will not survive a restart)"
            );
        }
        _ => {}
    }
    Arc::new(InMemoryJobTrackingStore::new(config.tracking.ttl_secs))
}

/// Install a tracking store built from `config` (honoring
/// `jobs.tracking.ttl_secs`) if this app doesn't already have one installed.
///
/// Called by `crate::job::start_runtime` before dispatching to a
/// backend-specific starter, so the config-driven TTL wins over
/// [`ensure_tracking_store_installed`]'s hardcoded default (that function
/// runs afterward, inside `install_job_client`, and is a no-op once a store
/// is already present).
///
/// See [`ensure_tracking_store_installed`] for why this also reinstalls when
/// the global fallback is missing, even if `state`'s extension is present.
pub(crate) fn ensure_tracking_store_installed_from_config(
    state: &AppState,
    config: &crate::config::JobConfig,
) {
    if tracking_store_from_state(state).is_none() || global_tracking_store().is_none() {
        install_tracking_store(state, store_for_config(state, config));
    }
}

/// Resolve the tracking store from `state`'s extensions.
pub(crate) fn tracking_store_from_state(state: &AppState) -> Option<Arc<dyn JobTrackingStore>> {
    state
        .extension::<JobTrackingStoreEntry>()
        .map(|entry| entry.0.clone())
}

/// Resolve the process-global tracking store used by the free
/// `enqueue_tracked` functions (which have no `AppState`).
pub(crate) fn global_tracking_store() -> Option<Arc<dyn JobTrackingStore>> {
    GLOBAL_TRACKING_STORE.get()?.read().ok()?.clone()
}

/// Reset the process-global tracking store, mirroring
/// [`crate::job::clear_global_job_client`].
pub(crate) fn clear_global_tracking_store() {
    if let Some(lock) = GLOBAL_TRACKING_STORE.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    } else {
        let _ = GLOBAL_TRACKING_STORE.set(RwLock::new(None));
    }
}

/// Reject a payload shaped like the tracked-job envelope: a top-level object
/// carrying a `__autumn_tracked` field.
///
/// Only [`wrap_tracked_payload`] (via `enqueue_tracked`/`enqueue_tracked_for`)
/// may construct a payload with this shape. Every other enqueue entry point
/// must call this first so that a plain job's `Args` struct can never
/// coincidentally collide with — and be silently misinterpreted as — a
/// tracked job's envelope by [`take_tracked_payload`]/[`split_tracked_payload`].
pub(crate) fn reject_reserved_envelope_marker(payload: &Value) -> AutumnResult<()> {
    let collides = payload
        .as_object()
        .is_some_and(|obj| obj.contains_key(ENVELOPE_MARKER));
    if collides {
        return Err(AutumnError::bad_request_msg(format!(
            "job payload must not contain a top-level '{ENVELOPE_MARKER}' field; this name is \
             reserved for autumn_web::job::enqueue_tracked"
        )));
    }
    Ok(())
}

/// If `payload` is a tracked-job envelope, settle its tracking record to
/// `failed` with `message`.
///
/// Used by each backend's execute path when it short-circuits before
/// `run_job_handler` runs (an admin-canceled job, an unregistered job name)
/// — without this, those paths are the only way a tracked job's status
/// record could be left stuck at `pending`/`running` until TTL expiry, even
/// though the job itself already reached a terminal outcome.
pub(crate) async fn settle_tracked_payload_as_failed(
    state: &AppState,
    payload: &Value,
    message: &str,
) {
    settle_tracked_payload_with_store(tracking_store_from_state(state), payload, message).await;
}

/// Like [`settle_tracked_payload_as_failed`], but resolves the store from
/// the process-global fallback instead of an `AppState` extension.
///
/// For use by admin backends (`RedisJobAdminBackend`, `PgJobAdminBackend`, and
/// the `SQLite` one) that operate directly against a queue backend with no
/// `AppState` in hand — an operator cancelling a job that hasn't reached a
/// worker yet goes through these paths, not `run_job_handler`.
pub(crate) async fn settle_tracked_payload_as_failed_globally(payload: &Value, message: &str) {
    settle_tracked_payload_with_store(global_tracking_store(), payload, message).await;
}

async fn settle_tracked_payload_with_store(
    store: Option<Arc<dyn JobTrackingStore>>,
    payload: &Value,
    message: &str,
) {
    let (key, _) = split_tracked_payload(payload);
    let Some(key) = key else {
        return;
    };
    if let Some(store) = store {
        let _ = store.fail(key, message.to_owned()).await;
    }
}

/// A tracking record's owner and `updated_at`, captured *before* an admin
/// retry makes the job visible to workers again, so
/// [`apply_retry_reset`] can later detect whether anything wrote to the
/// record in the meantime.
pub(crate) type RetrySnapshot = (TrackedJobOwner, DateTime<Utc>);

/// If `payload` is a tracked-job envelope, read its current tracking record.
///
/// Callers on the admin retry paths must call this *before* re-enqueueing —
/// i.e. before the retried job can possibly be claimed and executed — and
/// pass the result to [`apply_retry_reset`] once the retry is confirmed.
/// Reading it any later (e.g. after the re-enqueue) defeats the purpose: a
/// fast retry could already have run to completion and settled the record,
/// and a read at that point would capture the *fresh* terminal write as the
/// CAS baseline, making the later reset believe nothing has changed since
/// and clobber it anyway.
pub(crate) async fn capture_retry_snapshot(payload: &Value) -> Option<RetrySnapshot> {
    let (key, _) = split_tracked_payload(payload);
    let key = key?;
    let store = global_tracking_store()?;
    let record = store.get(key).await.ok().flatten()?;
    Some((record.owner, record.updated_at))
}

/// If `payload` is a tracked-job envelope and `snapshot` is `Some` (i.e.
/// [`capture_retry_snapshot`] found a record before the retry was
/// re-enqueued), reset the tracking record back to `pending` — preserving
/// the captured owner — now that the admin retry has succeeded.
///
/// `mark_running`/`set_progress` intentionally no-op once a record is
/// terminal, to protect against a stray write from an abandoned attempt
/// overwriting a legitimate final result — but that guard also means a
/// retried job's progress would never surface without this: the public
/// status would stay at its previous `failed` state until the retry itself
/// settles. Resolves the store from the process-global fallback since none
/// of the three admin backends (`JobAdminMemoryBackend`,
/// `RedisJobAdminBackend`, `PgJobAdminBackend`) carry an `AppState`.
///
/// The reset only applies if the record is unchanged since `snapshot` was
/// captured ([`JobTrackingStore::reset_for_retry`] is a compare-and-swap on
/// `updated_at`), so a fast retry that already settled the record before
/// this call runs is left alone rather than stomped back to a stale
/// `pending`.
pub(crate) async fn apply_retry_reset(payload: &Value, snapshot: Option<RetrySnapshot>) {
    let Some((owner, expected_updated_at)) = snapshot else {
        return;
    };
    let (key, _) = split_tracked_payload(payload);
    let Some(key) = key else {
        return;
    };
    let Some(store) = global_tracking_store() else {
        return;
    };
    let _ = store.reset_for_retry(key, owner, expected_updated_at).await;
}

// ── enqueue_tracked ────────────────────────────────────────────────────────────

/// Built-in route prefix for polling a tracked job's status: the full path is
/// `{JOB_STATUS_PATH_PREFIX}{token}`.
pub(crate) const JOB_STATUS_PATH_PREFIX: &str = "/_autumn/jobs/";

/// A handle returned by [`enqueue_tracked`]/[`enqueue_tracked_for`] carrying
/// the public, unguessable token used to poll the job's tracked status.
///
/// The token is distinct from (and never reveals) the internal job id.
#[derive(Debug, Clone)]
pub struct TrackedJobHandle {
    /// The raw, unguessable polling token. Deliver this to the caller (e.g.
    /// embed it in a redirect or JSON response) — it cannot be recovered
    /// later; only its hash is persisted.
    pub token: String,
}

impl TrackedJobHandle {
    /// The path of the built-in status route for this handle's token.
    #[must_use]
    pub fn status_path(&self) -> String {
        format!("{JOB_STATUS_PATH_PREFIX}{}", self.token)
    }
}

/// Enqueue `name` with `payload`, returning a [`TrackedJobHandle`].
///
/// The handle's token is an anonymous capability: anyone holding the token
/// may poll the job's status. Use [`enqueue_tracked_for`] to bind status
/// access to a session or authenticated user instead.
///
/// # Errors
///
/// Returns an internal error when the job runtime or its tracking store are
/// not initialized, when `name` does not match a registered job, or when the
/// active backend rejects the enqueue operation.
pub async fn enqueue_tracked(name: &str, payload: Value) -> AutumnResult<TrackedJobHandle> {
    enqueue_tracked_for(name, payload, TrackedJobOwner::Anonymous).await
}

/// Like [`enqueue_tracked`], binding the tracked status record to `owner` so
/// only a request matching that session/user may poll it.
///
/// # Errors
///
/// See [`enqueue_tracked`].
pub async fn enqueue_tracked_for(
    name: &str,
    payload: Value,
    owner: TrackedJobOwner,
) -> AutumnResult<TrackedJobHandle> {
    let client = crate::job::global_job_client().ok_or_else(|| {
        AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        ))
    })?;
    let store = global_tracking_store().ok_or_else(|| {
        AutumnError::internal_server_error(std::io::Error::other(
            "job tracking store is not initialized; register jobs with AppBuilder::jobs()",
        ))
    })?;

    let token = crate::auth::generate_raw_token();
    let key = crate::auth::hash_api_token(&token);
    store.create(&key, owner).await?;

    let wrapped = wrap_tracked_payload(&key, &payload);
    match client.enqueue_with_outcome(name, wrapped).await {
        Ok(crate::job::EnqueueOutcome::Queued) => {}
        Ok(crate::job::EnqueueOutcome::Deduplicated) => {
            store
                .fail(&key, "An equivalent job is already in progress.".to_owned())
                .await?;
        }
        Ok(crate::job::EnqueueOutcome::Skipped) => {
            // A JobInterceptor completed without ever delivering the job to
            // the backend — the record must not be left at Pending forever
            // for a job that will never actually run.
            store
                .fail(&key, "The job could not be enqueued.".to_owned())
                .await?;
        }
        Err(error) => {
            // The job never entered the queue, so the `Pending` record
            // created above must not be left to linger until TTL expiry —
            // settle it the same way the `Deduplicated` outcome does.
            let _ = store
                .fail(&key, "The job could not be enqueued.".to_owned())
                .await;
            return Err(error);
        }
    }

    Ok(TrackedJobHandle { token })
}

// ── Status route ───────────────────────────────────────────────────────────────

/// The built-in status route's axum path pattern (mounted at
/// `mount_framework_routes` time when `jobs.tracking.route_enabled`).
pub(crate) const JOB_STATUS_ROUTE_PATH: &str = "/_autumn/jobs/{token}";

/// JSON representation of a tracked job's status, returned by the built-in
/// status route to API clients (and reused as the data behind the htmx
/// fragment for browser clients).
#[derive(Debug, Clone, Serialize)]
struct JobStatusDto {
    status: TrackedJobStatus,
    progress: Option<u8>,
    message: Option<String>,
    result: Option<Value>,
    error: Option<String>,
}

impl From<&TrackedJobRecord> for JobStatusDto {
    fn from(record: &TrackedJobRecord) -> Self {
        Self {
            status: record.status,
            progress: record.progress_pct,
            message: record.progress_message.clone(),
            result: record.result.clone(),
            error: record.error.clone(),
        }
    }
}

/// Router for the framework's built-in tracked-job status endpoint.
///
/// Mounted automatically unless `jobs.tracking.route_enabled = false`.
pub(crate) fn status_router() -> axum::Router<AppState> {
    axum::Router::new().route(
        JOB_STATUS_ROUTE_PATH,
        axum::routing::get(job_status_handler),
    )
}

/// `GET /_autumn/jobs/{token}`: resolve the tracked-job record for `token`
/// and return it as JSON for API clients, or an htmx-pollable HTML fragment
/// for browsers (content-negotiated). Unknown, expired, and unauthorized
/// tokens all render the identical 404 so the route is never an
/// existence/ownership oracle.
async fn job_status_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    session: crate::session::Session,
    headers: axum::http::HeaderMap,
) -> AutumnResult<axum::response::Response> {
    let not_found = || AutumnError::not_found_msg("This job status page could not be found.");

    let store = tracking_store_from_state(&state).ok_or_else(not_found)?;
    let key = crate::auth::hash_api_token(&token);
    let record = store.get(&key).await?.ok_or_else(not_found)?;
    if !record.owner.authorizes(&session, &state).await {
        return Err(not_found());
    }

    let path = format!("{JOB_STATUS_PATH_PREFIX}{token}");
    let mut response = render_status_response(&record, &headers, &path);
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

/// Whether the request prefers an htmx-pollable HTML fragment over JSON:
/// true for an htmx request (`HX-Request: true`) or an `Accept` header that
/// explicitly prefers `text/html` over JSON, per
/// [`crate::middleware::error_page_filter::accept_prefers_html`]. An absent,
/// empty, or bare-wildcard (`*/*`) `Accept` header — curl and most
/// `fetch()`/HTTP-client defaults — prefers JSON, since this route is
/// JSON-first for API clients and only a real browser navigation sends an
/// explicit `text/html` preference. Rendering is only possible with the
/// `maud` feature enabled.
#[cfg(feature = "maud")]
fn wants_html_response(headers: &axum::http::HeaderMap) -> bool {
    let is_htmx = headers
        .get("hx-request")
        .is_some_and(|value| value == "true");
    if is_htmx {
        return true;
    }
    // `accept_prefers_html` treats a bare wildcard Accept as "probably a
    // browser" — a reasonable default for its original use (full-page error
    // rendering), where a real browser navigation and a bare `*/*` both
    // reasonably get an HTML page. This status route is JSON-first for API
    // clients, though, and a bare wildcard is exactly what curl and many
    // `fetch()`/HTTP-client defaults send with no real preference — so only
    // defer to it once an explicit `text/html` preference is present; an
    // actual browser navigation always sends one.
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    accept.contains("text/html")
        && crate::middleware::error_page_filter::accept_prefers_html(headers)
}

#[cfg(feature = "maud")]
fn render_status_response(
    record: &TrackedJobRecord,
    headers: &axum::http::HeaderMap,
    path: &str,
) -> axum::response::Response {
    if wants_html_response(headers) {
        status_fragment(record, path).into_response()
    } else {
        axum::Json(JobStatusDto::from(record)).into_response()
    }
}

#[cfg(not(feature = "maud"))]
fn render_status_response(
    record: &TrackedJobRecord,
    _headers: &axum::http::HeaderMap,
    _path: &str,
) -> axum::response::Response {
    axum::Json(JobStatusDto::from(record)).into_response()
}

/// Render the htmx-pollable status fragment.
///
/// While pending/running, the wrapper `div` carries
/// `hx-get={path} hx-trigger="every 2s" hx-swap="outerHTML"` so it re-fetches
/// and replaces itself every 2 seconds with zero app-authored JS. Once the
/// job reaches a terminal state the fragment carries **no** `hx-*`
/// attributes, so htmx has nothing left to poll — it renders the download
/// link (when the result carries a `download_url`) or the failure message.
#[cfg(feature = "maud")]
fn status_fragment(record: &TrackedJobRecord, path: &str) -> maud::Markup {
    let terminal = record.status.is_terminal();
    let (hx_get, hx_trigger, hx_swap) = if terminal {
        (None, None, None)
    } else {
        (Some(path), Some("every 2s"), Some("outerHTML"))
    };
    let pct = record.progress_pct.unwrap_or(0);

    maud::html! {
        div id="autumn-job-status" class="autumn-job-status"
            hx-get=[hx_get] hx-trigger=[hx_trigger] hx-swap=[hx_swap]
        {
            @match record.status {
                TrackedJobStatus::Pending | TrackedJobStatus::Running => {
                    progress class="autumn-job-status__bar" value=(pct) max="100" {}
                    @if let Some(message) = &record.progress_message {
                        p class="autumn-job-status__message" { (message) }
                    } @else {
                        p class="autumn-job-status__message" { (pct) "%" }
                    }
                }
                TrackedJobStatus::Succeeded => {
                    @if let Some(url) = record
                        .result
                        .as_ref()
                        .and_then(|result| result.get("download_url"))
                        .and_then(Value::as_str)
                    {
                        p class="autumn-job-status__success" {
                            a href=(url) download="" { "Download" }
                        }
                    } @else {
                        p class="autumn-job-status__success" { "Completed." }
                    }
                }
                TrackedJobStatus::Failed => {
                    p class="autumn-job-status__error" {
                        (record.error.as_deref().unwrap_or(GENERIC_FAILURE_MESSAGE))
                    }
                }
            }
        }
    }
}

// ── In-memory store ───────────────────────────────────────────────────────────

struct MemoryEntry {
    record: TrackedJobRecord,
    expires_at: DateTime<Utc>,
}

/// Sweep expired entries out of an [`InMemoryJobTrackingStore`] every this
/// many [`InMemoryJobTrackingStore::create`] calls, so long-running processes
/// don't accumulate one dead `HashMap` entry per tracked job forever — `get`
/// already filters expired entries out of reads, but without this they'd
/// never actually be freed. Amortized rather than swept on every write to
/// keep the common-case cost of `create` O(1).
const IN_MEMORY_SWEEP_INTERVAL: u64 = 100;

/// In-memory [`JobTrackingStore`] for development, testing, and the `local`
/// job backend. State is lost on restart and not shared across processes.
#[derive(Clone)]
pub struct InMemoryJobTrackingStore {
    entries: Arc<RwLock<HashMap<String, MemoryEntry>>>,
    ttl: chrono::TimeDelta,
    clock: Arc<dyn ClockSource>,
    creates_since_sweep: Arc<AtomicU64>,
}

impl InMemoryJobTrackingStore {
    /// Construct a store whose records expire `ttl_secs` after their last
    /// write.
    #[must_use]
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            // `TimeDelta::seconds` PANICS above `i64::MAX / 1_000`, so the
            // obvious `try_from(..).unwrap_or(i64::MAX)` saturation crashed on
            // exactly the pathological `ttl_secs` it was meant to absorb.
            ttl: crate::time_math::saturating_time_delta_secs(ttl_secs),
            clock: Arc::new(SystemClock),
            creates_since_sweep: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Replace the clock used to evaluate expiry.
    ///
    /// Defaults to [`SystemClock`]; tests pass a
    /// [`crate::time::FixedClock`] / [`crate::time::TickingClock`] to make
    /// expiry deterministic.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    fn is_live(&self, entry: &MemoryEntry) -> bool {
        entry.expires_at > self.clock.now()
    }

    #[cfg(test)]
    fn raw_entry_count(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[allow(clippy::significant_drop_tightening)]
    fn insert_and_maybe_sweep(&self, key: &str, record: TrackedJobRecord, now: DateTime<Utc>) {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(
            key.to_owned(),
            MemoryEntry {
                record,
                expires_at: crate::time_math::saturating_dt_add(now, self.ttl),
            },
        );
        if self
            .creates_since_sweep
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(IN_MEMORY_SWEEP_INTERVAL)
        {
            guard.retain(|_, entry| entry.expires_at > now);
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    fn with_record_mut<F>(&self, key: &str, f: F)
    where
        F: FnOnce(&mut TrackedJobRecord),
    {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.clock.now();
        if let Some(entry) = guard.get_mut(key)
            && self.is_live(entry)
        {
            f(&mut entry.record);
            entry.record.updated_at = now;
            entry.expires_at = crate::time_math::saturating_dt_add(now, self.ttl);
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    fn reset_for_retry_if_unchanged(
        &self,
        key: &str,
        owner: TrackedJobOwner,
        expected_updated_at: DateTime<Utc>,
    ) {
        let now = self.clock.now();
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.get(key)
            && self.is_live(entry)
            && entry.record.updated_at == expected_updated_at
        {
            guard.insert(
                key.to_owned(),
                MemoryEntry {
                    record: TrackedJobRecord {
                        status: TrackedJobStatus::Pending,
                        progress_pct: None,
                        progress_message: None,
                        result: None,
                        error: None,
                        owner,
                        updated_at: now,
                    },
                    expires_at: crate::time_math::saturating_dt_add(now, self.ttl),
                },
            );
        }
    }
}

impl JobTrackingStore for InMemoryJobTrackingStore {
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let now = self.clock.now();
            let record = TrackedJobRecord {
                status: TrackedJobStatus::Pending,
                progress_pct: None,
                progress_message: None,
                result: None,
                error: None,
                owner,
                updated_at: now,
            };
            self.insert_and_maybe_sweep(key, record, now);
            Ok(())
        })
    }

    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.with_record_mut(key, apply_mark_running);
            Ok(())
        })
    }

    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let pct = pct.min(100);
            self.with_record_mut(key, |record| apply_set_progress(record, pct, message));
            Ok(())
        })
    }

    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.with_record_mut(key, |record| apply_complete(record, result));
            Ok(())
        })
    }

    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.with_record_mut(key, |record| apply_fail(record, error));
            Ok(())
        })
    }

    fn reset_for_retry<'a>(
        &'a self,
        key: &'a str,
        owner: TrackedJobOwner,
        expected_updated_at: DateTime<Utc>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.reset_for_retry_if_unchanged(key, owner, expected_updated_at);
            Ok(())
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>> {
        Box::pin(async move {
            let guard = self
                .entries
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(guard
                .get(key)
                .filter(|entry| self.is_live(entry))
                .map(|entry| TrackedJobRecord {
                    status: entry.record.status,
                    progress_pct: entry.record.progress_pct,
                    progress_message: entry.record.progress_message.clone(),
                    result: entry.record.result.clone(),
                    error: entry.record.error.clone(),
                    owner: entry.record.owner.clone(),
                    updated_at: entry.record.updated_at,
                }))
        })
    }
}

// ── Redis store ───────────────────────────────────────────────────────────────

/// Redis-backed [`JobTrackingStore`].
///
/// Each record is a single JSON blob under `SET … EX ttl_secs`, so a write
/// both persists and refreshes the TTL in one round trip, and Redis itself
/// expires stale records — no sweeper needed.
#[cfg(feature = "redis")]
#[derive(Clone)]
pub struct RedisJobTrackingStore {
    connection: redis::aio::ConnectionManager,
    key_prefix: String,
    ttl_secs: u64,
}

#[cfg(feature = "redis")]
impl RedisJobTrackingStore {
    /// Construct a store over an existing connection, namespacing keys under
    /// `key_prefix` and expiring records `ttl_secs` after their last write.
    #[must_use]
    pub fn new(
        connection: redis::aio::ConnectionManager,
        key_prefix: impl Into<String>,
        ttl_secs: u64,
    ) -> Self {
        Self {
            connection,
            key_prefix: key_prefix.into(),
            ttl_secs,
        }
    }

    fn key_for(&self, key: &str) -> String {
        format!("{}:tracking:{key}", self.key_prefix)
    }

    async fn write(&self, key: &str, record: &TrackedJobRecord) -> AutumnResult<()> {
        use redis::AsyncCommands as _;
        let payload = serde_json::to_string(record).map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "job tracking serialize failed: {error}"
            ))
        })?;
        self.connection
            .clone()
            .set_ex::<_, _, ()>(self.key_for(key), payload, self.ttl_secs.max(1))
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking redis write failed: {error}"
                ))
            })
    }

    async fn read(&self, key: &str) -> AutumnResult<Option<TrackedJobRecord>> {
        use redis::AsyncCommands as _;
        let payload: Option<String> = self
            .connection
            .clone()
            .get(self.key_for(key))
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking redis read failed: {error}"
                ))
            })?;
        payload
            .map(|payload| {
                serde_json::from_str::<TrackedJobRecord>(&payload).map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "job tracking deserialize failed: {error}"
                    ))
                })
            })
            .transpose()
    }

    /// Read-modify-write: a no-op if the key is unknown or expired.
    async fn update(&self, key: &str, f: impl FnOnce(&mut TrackedJobRecord)) -> AutumnResult<()> {
        let Some(mut record) = self.read(key).await? else {
            return Ok(());
        };
        f(&mut record);
        record.updated_at = chrono::Utc::now();
        self.write(key, &record).await
    }

    /// Atomically overwrite the record with `new_record`, but only if the
    /// currently-stored record's `updated_at` still equals
    /// `expected_updated_at` — a compare-and-swap guard evaluated inside a
    /// single Lua script so the check-then-write cannot race against a
    /// concurrent write landing in between.
    async fn write_if_unchanged(
        &self,
        key: &str,
        expected_updated_at: DateTime<Utc>,
        new_record: &TrackedJobRecord,
    ) -> AutumnResult<()> {
        const SCRIPT: &str = r"
local raw = redis.call('GET', KEYS[1])
if not raw then
  return 0
end
local record = cjson.decode(raw)
if record.updated_at ~= ARGV[1] then
  return 0
end
redis.call('SET', KEYS[1], ARGV[2], 'EX', ARGV[3])
return 1
";
        let expected = serde_json::to_string(&expected_updated_at).map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "job tracking serialize failed: {error}"
            ))
        })?;
        // cjson.decode(raw).updated_at yields the unquoted string value, so
        // strip the JSON string quotes serde_json wraps it in above.
        let expected = expected.trim_matches('"').to_owned();
        let payload = serde_json::to_string(new_record).map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "job tracking serialize failed: {error}"
            ))
        })?;
        redis::cmd("EVAL")
            .arg(SCRIPT)
            .arg(1)
            .arg(self.key_for(key))
            .arg(expected)
            .arg(payload)
            .arg(self.ttl_secs.max(1))
            .query_async::<i64>(&mut self.connection.clone())
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking redis reset failed: {error}"
                ))
            })?;
        Ok(())
    }
}

#[cfg(feature = "redis")]
impl JobTrackingStore for RedisJobTrackingStore {
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let record = TrackedJobRecord {
                status: TrackedJobStatus::Pending,
                progress_pct: None,
                progress_message: None,
                result: None,
                error: None,
                owner,
                updated_at: chrono::Utc::now(),
            };
            self.write(key, &record).await
        })
    }

    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move { self.update(key, apply_mark_running).await })
    }

    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let pct = pct.min(100);
            self.update(key, |record| {
                apply_set_progress(record, pct, message.clone());
            })
            .await
        })
    }

    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.update(key, |record| apply_complete(record, result.clone()))
                .await
        })
    }

    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.update(key, |record| apply_fail(record, error.clone()))
                .await
        })
    }

    fn reset_for_retry<'a>(
        &'a self,
        key: &'a str,
        owner: TrackedJobOwner,
        expected_updated_at: DateTime<Utc>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let record = TrackedJobRecord {
                status: TrackedJobStatus::Pending,
                progress_pct: None,
                progress_message: None,
                result: None,
                error: None,
                owner,
                updated_at: chrono::Utc::now(),
            };
            self.write_if_unchanged(key, expected_updated_at, &record)
                .await
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>> {
        Box::pin(async move { self.read(key).await })
    }
}

/// Build a [`RedisJobTrackingStore`] from `[jobs.redis]` config. Returns
/// `None` when no URL is configured or it fails to parse; the connection
/// manager itself connects lazily, so this never blocks on Redis being up.
#[cfg(feature = "redis")]
pub(crate) fn build_redis_tracking_store(
    config: &crate::config::JobConfig,
) -> Option<RedisJobTrackingStore> {
    let url = config
        .redis
        .url
        .clone()
        .filter(|url| !url.trim().is_empty())?;
    let client = crate::redis_tls::open_client(&url).ok()?;
    let connection = redis::aio::ConnectionManager::new_lazy_with_config(
        client,
        redis::aio::ConnectionManagerConfig::new(),
    )
    .ok()?;
    Some(RedisJobTrackingStore::new(
        connection,
        config.redis.key_prefix.clone(),
        config.tracking.ttl_secs,
    ))
}

// ── Postgres store ───────────────────────────────────────────────────────────

/// Postgres-backed [`JobTrackingStore`].
///
/// Each record is a single JSONB blob in `autumn_job_tracking`, keyed by the
/// token hash, with lazy expiry (reads filter on `expires_at`; writes refresh
/// it). See the `create_job_tracking` framework migration.
#[cfg(feature = "db")]
#[derive(Clone)]
pub struct PgJobTrackingStore {
    pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    ttl_secs: u64,
    clock: Arc<dyn ClockSource>,
}

#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct PgTrackingRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    record: String,
}

#[cfg(feature = "db")]
impl PgJobTrackingStore {
    /// Construct a store backed by `pool`, expiring records `ttl_secs` after
    /// their last write.
    #[must_use]
    pub fn new(
        pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        ttl_secs: u64,
    ) -> Self {
        Self {
            pool,
            ttl_secs,
            clock: Arc::new(SystemClock),
        }
    }

    /// Replace the clock used to evaluate expiry.
    ///
    /// Defaults to [`SystemClock`]; tests pass a
    /// [`crate::time::FixedClock`] to make expiry deterministic.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    fn expires_at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        crate::time_math::saturating_dt_add(
            now,
            crate::time_math::saturating_time_delta_secs(self.ttl_secs),
        )
    }

    async fn conn(
        &self,
    ) -> AutumnResult<
        diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    > {
        self.pool.get().await.map_err(|error| {
            AutumnError::internal_server_error_msg(format!("job tracking pool error: {error}"))
        })
    }

    /// Read-modify-write: a no-op if the key is unknown or expired.
    async fn update(&self, key: &str, f: impl FnOnce(&mut TrackedJobRecord)) -> AutumnResult<()> {
        use diesel::OptionalExtension as _;
        use diesel_async::RunQueryDsl as _;

        let now = self.clock.now();
        let mut conn = self.conn().await?;
        let row = diesel::sql_query(
            "SELECT record::TEXT AS record FROM autumn_job_tracking WHERE key = $1 AND expires_at > $2",
        )
        .bind::<diesel::sql_types::Text, _>(key)
        .bind::<diesel::sql_types::Timestamptz, _>(now)
        .get_result::<PgTrackingRow>(&mut *conn)
        .await
        .optional()
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("job tracking select failed: {error}"))
        })?;

        let Some(row) = row else {
            return Ok(());
        };
        let mut record =
            serde_json::from_str::<TrackedJobRecord>(&row.record).map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking deserialize failed: {error}"
                ))
            })?;
        f(&mut record);
        record.updated_at = now;
        let payload = serde_json::to_string(&record).map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "job tracking serialize failed: {error}"
            ))
        })?;

        diesel::sql_query(
            "UPDATE autumn_job_tracking SET record = $2::JSONB, updated_at = $3, expires_at = $4 \
             WHERE key = $1",
        )
        .bind::<diesel::sql_types::Text, _>(key)
        .bind::<diesel::sql_types::Text, _>(&payload)
        .bind::<diesel::sql_types::Timestamptz, _>(now)
        .bind::<diesel::sql_types::Timestamptz, _>(self.expires_at(now))
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("job tracking update failed: {error}"))
        })?;
        Ok(())
    }
}

#[cfg(feature = "db")]
impl JobTrackingStore for PgJobTrackingStore {
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            use diesel_async::RunQueryDsl as _;

            let now = self.clock.now();
            let record = TrackedJobRecord {
                status: TrackedJobStatus::Pending,
                progress_pct: None,
                progress_message: None,
                result: None,
                error: None,
                owner,
                updated_at: now,
            };
            let payload = serde_json::to_string(&record).map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking serialize failed: {error}"
                ))
            })?;
            let mut conn = self.conn().await?;
            diesel::sql_query(
                "INSERT INTO autumn_job_tracking (key, record, updated_at, expires_at) \
                 VALUES ($1, $2::JSONB, $3, $4) \
                 ON CONFLICT (key) DO UPDATE SET \
                     record = EXCLUDED.record, \
                     updated_at = EXCLUDED.updated_at, \
                     expires_at = EXCLUDED.expires_at",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::Text, _>(&payload)
            .bind::<diesel::sql_types::Timestamptz, _>(now)
            .bind::<diesel::sql_types::Timestamptz, _>(self.expires_at(now))
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking insert failed: {error}"
                ))
            })?;
            Ok(())
        })
    }

    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move { self.update(key, apply_mark_running).await })
    }

    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let pct = pct.min(100);
            self.update(key, |record| {
                apply_set_progress(record, pct, message.clone());
            })
            .await
        })
    }

    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.update(key, |record| apply_complete(record, result.clone()))
                .await
        })
    }

    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.update(key, |record| apply_fail(record, error.clone()))
                .await
        })
    }

    fn reset_for_retry<'a>(
        &'a self,
        key: &'a str,
        owner: TrackedJobOwner,
        expected_updated_at: DateTime<Utc>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            use diesel_async::RunQueryDsl as _;

            let now = self.clock.now();
            let record = TrackedJobRecord {
                status: TrackedJobStatus::Pending,
                progress_pct: None,
                progress_message: None,
                result: None,
                error: None,
                owner,
                updated_at: now,
            };
            let payload = serde_json::to_string(&record).map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking serialize failed: {error}"
                ))
            })?;
            let mut conn = self.conn().await?;
            // The WHERE clause is a compare-and-swap guard: it only takes
            // effect if nothing has written to this record (e.g. the
            // retried attempt itself already settling) since
            // `expected_updated_at` was read, so a fast retry can never
            // clobber a fresher terminal write with a stale reset.
            diesel::sql_query(
                "UPDATE autumn_job_tracking SET record = $2::JSONB, updated_at = $3, \
                 expires_at = $4 WHERE key = $1 AND updated_at = $5",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::Text, _>(&payload)
            .bind::<diesel::sql_types::Timestamptz, _>(now)
            .bind::<diesel::sql_types::Timestamptz, _>(self.expires_at(now))
            .bind::<diesel::sql_types::Timestamptz, _>(expected_updated_at)
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking reset failed: {error}"
                ))
            })?;
            Ok(())
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>> {
        Box::pin(async move {
            use diesel::OptionalExtension as _;
            use diesel_async::RunQueryDsl as _;

            let now = self.clock.now();
            let mut conn = self.conn().await?;
            let row = diesel::sql_query(
                "SELECT record::TEXT AS record FROM autumn_job_tracking WHERE key = $1 AND expires_at > $2",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::Timestamptz, _>(now)
            .get_result::<PgTrackingRow>(&mut *conn)
            .await
            .optional()
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "job tracking select failed: {error}"
                ))
            })?;

            row.map(|row| {
                serde_json::from_str::<TrackedJobRecord>(&row.record).map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "job tracking deserialize failed: {error}"
                    ))
                })
            })
            .transpose()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::FixedClock;

    fn store() -> InMemoryJobTrackingStore {
        InMemoryJobTrackingStore::new(86_400)
    }

    // ── TrackedJobOwner::from_session ─────────────────────────────────────

    #[tokio::test]
    async fn from_session_touches_a_fresh_non_cookie_backed_session_so_its_cookie_is_set() {
        let session = crate::session::Session::new_for_test_without_cookie(
            "fresh-session-id".to_owned(),
            HashMap::new(),
        );
        let state = AppState::for_test();

        let owner = TrackedJobOwner::from_session(&session, &state).await;

        assert_eq!(
            owner,
            TrackedJobOwner::Session("fresh-session-id".to_owned())
        );
        assert!(
            session.has_pending_changes().await,
            "from_session must dirty a fresh, non-cookie-backed session so the browser \
             actually receives a cookie for the id the tracked job is bound to"
        );
    }

    #[tokio::test]
    async fn from_session_does_not_redundantly_touch_an_already_cookie_backed_session() {
        let session =
            crate::session::Session::new_for_test("existing-session-id".to_owned(), HashMap::new());
        let state = AppState::for_test();

        let owner = TrackedJobOwner::from_session(&session, &state).await;

        assert_eq!(
            owner,
            TrackedJobOwner::Session("existing-session-id".to_owned())
        );
        assert!(
            !session.has_pending_changes().await,
            "an already cookie-backed session doesn't need a forced re-save"
        );
    }

    #[tokio::test]
    async fn from_session_prefers_the_authenticated_user_id_over_the_session_id() {
        let session = crate::session::Session::new_for_test_without_cookie(
            "fresh-session-id".to_owned(),
            HashMap::new(),
        );
        let state = AppState::for_test();
        session.insert(state.auth_session_key(), "user-42").await;

        let owner = TrackedJobOwner::from_session(&session, &state).await;

        assert_eq!(owner, TrackedJobOwner::User("user-42".to_owned()));
    }

    #[tokio::test]
    async fn in_memory_store_with_extreme_ttl_does_not_panic() {
        // Regression (issue #1611): `jobs.tracking.ttl_secs` is plain config,
        // so a pathological value reached two panicking chrono APIs —
        // `TimeDelta::seconds` panics above `i64::MAX / 1_000`, and
        // `DateTime<Utc> + TimeDelta` panics when the sum leaves the
        // representable range. Both must clamp to a far-future (effectively
        // non-expiring) deadline rather than crash the process.
        for ttl_secs in [
            u64::MAX,
            u64::try_from(i64::MAX).unwrap_or(u64::MAX),
            // Just past `TimeDelta`'s `i64::MAX / 1_000` second ceiling.
            9_223_372_036_854_776_u64,
        ] {
            let store = InMemoryJobTrackingStore::new(ttl_secs);

            // Every write path stamps `expires_at = now + ttl`.
            store
                .create("k1", TrackedJobOwner::Anonymous)
                .await
                .unwrap();
            store.mark_running("k1").await.unwrap();
            store
                .set_progress("k1", 50, Some("half".to_owned()))
                .await
                .unwrap();
            let record = store.get("k1").await.unwrap().expect("record");
            store
                .reset_for_retry("k1", TrackedJobOwner::Anonymous, record.updated_at)
                .await
                .unwrap();
            store
                .complete("k1", serde_json::json!({"ok": true}))
                .await
                .unwrap();

            assert!(
                store.get("k1").await.unwrap().is_some(),
                "a record written with an extreme TTL must be present and unexpired"
            );
        }
    }

    #[test]
    fn extreme_ttl_secs_clamps_to_a_representable_time_delta() {
        // The shared helper is what keeps `TimeDelta::seconds`' panic out of
        // the tracking store's constructors.
        assert_eq!(
            crate::time_math::saturating_time_delta_secs(86_400),
            chrono::TimeDelta::seconds(86_400)
        );
        assert_eq!(
            crate::time_math::saturating_time_delta_secs(u64::MAX),
            chrono::TimeDelta::MAX
        );
    }

    #[tokio::test]
    async fn create_then_get_roundtrips_pending_with_owner() {
        let store = store();
        store
            .create("k1", TrackedJobOwner::User("user:42".to_owned()))
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Pending);
        assert_eq!(record.owner, TrackedJobOwner::User("user:42".to_owned()));
        assert!(record.progress_pct.is_none());
        assert!(record.result.is_none());
    }

    #[tokio::test]
    async fn reset_for_retry_applies_when_the_record_is_unchanged() {
        let store = store();
        store
            .create("k1", TrackedJobOwner::User("user:42".to_owned()))
            .await
            .unwrap();
        store.fail("k1", "boom".to_owned()).await.unwrap();
        let stale = store.get("k1").await.unwrap().expect("record");
        assert_eq!(stale.status, TrackedJobStatus::Failed);

        store
            .reset_for_retry("k1", stale.owner.clone(), stale.updated_at)
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Pending);
        assert_eq!(record.owner, TrackedJobOwner::User("user:42".to_owned()));
    }

    #[tokio::test]
    async fn reset_for_retry_is_a_no_op_when_a_fresher_write_already_landed() {
        // Simulates the race Codex flagged: the retried job is re-enqueued,
        // claimed, and settles to a terminal status before the admin retry
        // path gets around to calling `reset_for_retry` with the
        // `updated_at` it read *before* deciding to retry.
        let store = store();
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        store.fail("k1", "boom".to_owned()).await.unwrap();
        let stale = store.get("k1").await.unwrap().expect("record");
        assert_eq!(stale.status, TrackedJobStatus::Failed);

        // The retried attempt runs to completion in the meantime.
        store
            .complete("k1", serde_json::json!({"already": "done"}))
            .await
            .unwrap();

        // The admin retry path's reset, still holding the pre-retry
        // snapshot, must not clobber the fresher terminal result.
        store
            .reset_for_retry("k1", stale.owner, stale.updated_at)
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(
            record.status,
            TrackedJobStatus::Succeeded,
            "a reset computed from a stale read must not overwrite a write that landed since"
        );
        assert_eq!(record.result, Some(serde_json::json!({"already": "done"})));
    }

    #[tokio::test]
    async fn capture_retry_snapshot_before_enqueue_protects_a_fast_retry_from_being_reset() {
        // A snapshot taken *after* re-enqueueing can itself already observe
        // the retried job's terminal write (it settled faster than the
        // admin retry path got around to reading it), which would make the
        // CAS trivially "unchanged" and reset the fresh terminal record
        // right back to `pending`. `capture_retry_snapshot` must be called
        // *before* the retry is made visible so it captures the original
        // `failed` record instead.
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let store: Arc<dyn JobTrackingStore> = Arc::new(InMemoryJobTrackingStore::new(60));
        let state = AppState::for_test().with_profile("dev");
        install_tracking_store(&state, store.clone());

        let key = "retry-snapshot-key";
        store.create(key, TrackedJobOwner::Anonymous).await.unwrap();
        store.fail(key, "boom".to_owned()).await.unwrap();
        let payload = wrap_tracked_payload(key, &serde_json::json!({}));

        // Captured while the record is still the original `failed` one —
        // i.e. before the retry is re-enqueued/made visible.
        let snapshot = capture_retry_snapshot(&payload).await;

        // The retry runs to completion before `apply_retry_reset` is called.
        store
            .complete(key, serde_json::json!({"already": "done"}))
            .await
            .unwrap();

        apply_retry_reset(&payload, snapshot).await;

        let record = store.get(key).await.unwrap().expect("record");
        assert_eq!(
            record.status,
            TrackedJobStatus::Succeeded,
            "a snapshot captured before the retry was exposed must not let apply_retry_reset \
             clobber a terminal write that landed since"
        );
        assert_eq!(record.result, Some(serde_json::json!({"already": "done"})));

        crate::job::clear_global_job_client();
    }

    // ── shared mutation logic (single source of truth for all 3 backends) ────

    #[test]
    fn apply_functions_implement_the_documented_transitions() {
        let mut record = TrackedJobRecord {
            status: TrackedJobStatus::Pending,
            progress_pct: None,
            progress_message: None,
            result: None,
            error: None,
            owner: TrackedJobOwner::Anonymous,
            updated_at: chrono::Utc::now(),
        };

        apply_mark_running(&mut record);
        assert_eq!(record.status, TrackedJobStatus::Running);

        apply_set_progress(&mut record, 40, Some("40%".to_owned()));
        assert_eq!(record.progress_pct, Some(40));
        assert_eq!(record.progress_message.as_deref(), Some("40%"));

        apply_complete(&mut record, serde_json::json!({"ok": true}));
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(record.result, Some(serde_json::json!({"ok": true})));
        assert!(record.error.is_none());

        // A terminal record ignores further mark_running/set_progress calls.
        apply_mark_running(&mut record);
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        apply_set_progress(&mut record, 10, None);
        assert_eq!(record.progress_pct, Some(40));

        apply_fail(&mut record, "boom".to_owned());
        assert_eq!(record.status, TrackedJobStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("boom"));
        assert!(record.result.is_none());
    }

    #[tokio::test]
    async fn set_progress_clamps_above_100_and_persists_message() {
        let store = store();
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        store.mark_running("k1").await.unwrap();

        store
            .set_progress("k1", 250, Some("Rows 1200/5000".to_owned()))
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Running);
        assert_eq!(record.progress_pct, Some(100));
        assert_eq!(record.progress_message.as_deref(), Some("Rows 1200/5000"));
    }

    #[tokio::test]
    async fn complete_is_terminal_and_stores_result_json() {
        let store = store();
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        store.mark_running("k1").await.unwrap();
        store.set_progress("k1", 50, None).await.unwrap();

        store
            .complete("k1", serde_json::json!({"download_url": "/blob/abc.csv"}))
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(
            record.result,
            Some(serde_json::json!({"download_url": "/blob/abc.csv"}))
        );
        assert!(record.error.is_none());

        // A terminal record ignores further progress writes.
        store.set_progress("k1", 10, None).await.unwrap();
        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(record.progress_pct, Some(50));
    }

    #[tokio::test]
    async fn fail_stores_user_safe_error() {
        let store = store();
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();

        store
            .fail("k1", "The export could not be completed.".to_owned())
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("The export could not be completed.")
        );
        assert!(record.result.is_none());
    }

    #[tokio::test]
    async fn get_unknown_key_returns_none() {
        let store = store();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_expires_after_ttl() {
        let start = chrono::Utc::now();
        let store = InMemoryJobTrackingStore::new(10)
            .with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();

        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(11),
        )));
        assert!(store.get("k1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn terminal_write_refreshes_expiry() {
        let start = chrono::Utc::now();
        let store = InMemoryJobTrackingStore::new(10)
            .with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();

        // Complete at t=8s, before the original 10s TTL expires — this must
        // push expiry out to t=18s rather than leaving it at t=10s.
        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(8),
        )));
        store
            .complete("k1", serde_json::json!({"download_url": "/blob/abc.csv"}))
            .await
            .unwrap();

        // t=15s: past the original TTL, but within the refreshed window.
        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(15),
        )));
        let record = store.get("k1").await.unwrap();
        assert!(
            record.is_some(),
            "terminal write should have refreshed the TTL"
        );
    }

    #[tokio::test]
    async fn write_to_expired_key_is_a_no_op() {
        let start = chrono::Utc::now();
        let store =
            InMemoryJobTrackingStore::new(5).with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();

        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(6),
        )));
        // Expired: reads see it as gone, and writes must not resurrect it.
        store.set_progress("k1", 50, None).await.unwrap();
        assert!(store.get("k1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expired_entries_are_evicted_not_just_filtered() {
        let start = chrono::Utc::now();
        let store =
            InMemoryJobTrackingStore::new(1).with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store
            .create("expired-key", TrackedJobOwner::Anonymous)
            .await
            .unwrap();

        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(2),
        )));
        assert!(
            store.get("expired-key").await.unwrap().is_none(),
            "reads must already treat it as gone"
        );

        // Drive enough creates to cross the amortized sweep threshold. The
        // clock stays fixed, so every filler entry is still live when the
        // sweep runs — only the already-expired one should be removed.
        for i in 0..IN_MEMORY_SWEEP_INTERVAL {
            store
                .create(&format!("filler-{i}"), TrackedJobOwner::Anonymous)
                .await
                .unwrap();
        }

        assert_eq!(
            store.raw_entry_count(),
            usize::try_from(IN_MEMORY_SWEEP_INTERVAL).unwrap(),
            "the expired entry must actually be removed from the map, not just filtered on \
             read, or a long-running process leaks one entry per expired tracked job forever"
        );
    }

    // ── envelope ───────────────────────────────────────────────────────────

    #[test]
    fn wrap_then_take_roundtrips_key_and_inner_args() {
        let args = serde_json::json!({"account_id": 42});
        let wrapped = wrap_tracked_payload("abc123", &args);

        let (key, inner) = take_tracked_payload(wrapped);
        assert_eq!(key.as_deref(), Some("abc123"));
        assert_eq!(inner, args);
    }

    #[test]
    fn take_tracked_payload_on_untracked_payload_is_a_passthrough() {
        let args = serde_json::json!({"account_id": 42});
        let (key, inner) = take_tracked_payload(args.clone());
        assert!(key.is_none());
        assert_eq!(inner, args);
    }

    #[test]
    fn split_tracked_payload_borrows_inner_args_without_consuming() {
        let args = serde_json::json!({"account_id": 42});
        let wrapped = wrap_tracked_payload("abc123", &args);

        let (key, inner) = split_tracked_payload(&wrapped);
        assert_eq!(key, Some("abc123"));
        assert_eq!(inner, &args);
    }

    #[test]
    fn split_tracked_payload_on_untracked_payload_is_a_passthrough() {
        let args = serde_json::json!({"account_id": 42});
        let (key, inner) = split_tracked_payload(&args);
        assert!(key.is_none());
        assert_eq!(inner, &args);
    }

    #[test]
    fn reject_reserved_envelope_marker_rejects_a_colliding_top_level_field() {
        let colliding = serde_json::json!({"__autumn_tracked": {"k": "anything"}, "other": 1});
        let err =
            reject_reserved_envelope_marker(&colliding).expect_err("collision must be rejected");
        assert!(
            err.to_string().contains("__autumn_tracked"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reject_reserved_envelope_marker_allows_ordinary_payloads() {
        assert!(reject_reserved_envelope_marker(&serde_json::json!({"account_id": 42})).is_ok());
        assert!(reject_reserved_envelope_marker(&Value::Null).is_ok());
        // A field merely containing the substring, or not exactly matching
        // the reserved key, is not a collision.
        assert!(
            reject_reserved_envelope_marker(&serde_json::json!({"__autumn_tracked_at": 1})).is_ok()
        );
    }

    #[test]
    fn take_and_split_tracked_payload_agree_on_a_malformed_envelope_missing_args() {
        // The marker is present (so both functions detect a tracked
        // envelope) but there is no `args` field — a state `wrap_tracked_payload`
        // itself never produces, but the two unwrap functions must still
        // agree on how to handle it rather than silently diverging.
        let malformed = serde_json::json!({"__autumn_tracked": {"k": "abc123"}});

        let (split_key, split_inner) = split_tracked_payload(&malformed);
        assert_eq!(split_key, Some("abc123"));
        assert_eq!(split_inner, &Value::Null);

        let (take_key, take_inner) = take_tracked_payload(malformed);
        assert_eq!(take_key.as_deref(), Some("abc123"));
        assert_eq!(take_inner, Value::Null);
    }

    // ── JobContext ─────────────────────────────────────────────────────────

    #[test]
    fn job_context_current_outside_job_is_noop() {
        let ctx = JobContext::current();
        assert!(!ctx.is_tracked());
    }

    #[tokio::test]
    async fn noop_context_methods_never_panic_or_error() {
        let ctx = JobContext::none();
        assert!(ctx.set_progress(50, Some("halfway")).await.is_ok());
        ctx.set_result(serde_json::json!({"ok": true}));
        ctx.set_user_error("should be discarded");
        // Settling a no-op context must not panic even though nothing is stored.
        ctx.settle_success().await;
        ctx.settle_failure(GENERIC_FAILURE_MESSAGE).await;
    }

    #[tokio::test]
    async fn scope_makes_context_ambient_for_current() {
        let store: Arc<dyn JobTrackingStore> = Arc::new(InMemoryJobTrackingStore::new(60));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        let ctx = JobContext::tracked("k1".to_owned(), store.clone());

        let observed = scope(ctx, async { JobContext::current() }).await;
        assert!(observed.is_tracked());

        observed
            .set_progress(75, Some("almost done"))
            .await
            .unwrap();
        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.progress_pct, Some(75));
    }

    #[tokio::test]
    async fn settle_success_persists_ctx_result() {
        let store: Arc<dyn JobTrackingStore> = Arc::new(InMemoryJobTrackingStore::new(60));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        let ctx = JobContext::tracked("k1".to_owned(), store.clone());

        ctx.set_result(serde_json::json!({"download_url": "/blob/abc.csv"}));
        ctx.settle_success().await;

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(
            record.result,
            Some(serde_json::json!({"download_url": "/blob/abc.csv"}))
        );
    }

    #[tokio::test]
    async fn settle_success_without_set_result_stores_null() {
        let store: Arc<dyn JobTrackingStore> = Arc::new(InMemoryJobTrackingStore::new(60));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        let ctx = JobContext::tracked("k1".to_owned(), store.clone());

        ctx.settle_success().await;

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(record.result, Some(Value::Null));
    }

    #[tokio::test]
    async fn settle_failure_uses_set_user_error_over_default() {
        let store: Arc<dyn JobTrackingStore> = Arc::new(InMemoryJobTrackingStore::new(60));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        let ctx = JobContext::tracked("k1".to_owned(), store.clone());

        ctx.set_user_error("The export could not reach storage.");
        ctx.settle_failure(GENERIC_FAILURE_MESSAGE).await;

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("The export could not reach storage.")
        );
    }

    #[tokio::test]
    async fn settle_failure_without_set_user_error_uses_default_message() {
        let store: Arc<dyn JobTrackingStore> = Arc::new(InMemoryJobTrackingStore::new(60));
        store
            .create("k1", TrackedJobOwner::Anonymous)
            .await
            .unwrap();
        let ctx = JobContext::tracked("k1".to_owned(), store.clone());

        ctx.settle_failure(GENERIC_FAILURE_MESSAGE).await;

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Failed);
        assert_eq!(record.error.as_deref(), Some(GENERIC_FAILURE_MESSAGE));
    }

    // ── enqueue_tracked (error paths reachable without a running job runtime) ─

    #[tokio::test]
    async fn enqueue_tracked_errors_when_job_runtime_is_not_initialized() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let err = enqueue_tracked("export_orders", serde_json::json!({}))
            .await
            .expect_err("no job runtime should be an error, not a panic");
        assert!(
            err.to_string().contains("job runtime is not initialized"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn enqueue_failure_settles_the_orphaned_pending_record_instead_of_leaking_it() {
        struct KeyCapturingStore {
            inner: InMemoryJobTrackingStore,
            last_created_key: std::sync::Mutex<Option<String>>,
        }

        impl JobTrackingStore for KeyCapturingStore {
            fn create<'a>(
                &'a self,
                key: &'a str,
                owner: TrackedJobOwner,
            ) -> BoxFut<'a, AutumnResult<()>> {
                *self.last_created_key.lock().unwrap() = Some(key.to_owned());
                self.inner.create(key, owner)
            }
            fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>> {
                self.inner.mark_running(key)
            }
            fn set_progress<'a>(
                &'a self,
                key: &'a str,
                pct: u8,
                message: Option<String>,
            ) -> BoxFut<'a, AutumnResult<()>> {
                self.inner.set_progress(key, pct, message)
            }
            fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>> {
                self.inner.complete(key, result)
            }
            fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>> {
                self.inner.fail(key, error)
            }
            fn reset_for_retry<'a>(
                &'a self,
                key: &'a str,
                owner: TrackedJobOwner,
                expected_updated_at: DateTime<Utc>,
            ) -> BoxFut<'a, AutumnResult<()>> {
                self.inner.reset_for_retry(key, owner, expected_updated_at)
            }
            fn get<'a>(
                &'a self,
                key: &'a str,
            ) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>> {
                self.inner.get(key)
            }
        }

        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let store = Arc::new(KeyCapturingStore {
            inner: InMemoryJobTrackingStore::new(60),
            last_created_key: std::sync::Mutex::new(None),
        });

        let state = AppState::for_test().with_profile("dev");
        install_tracking_store(&state, store.clone());

        let shutdown = tokio_util::sync::CancellationToken::new();
        crate::job::start_local_runtime(
            vec![crate::job::JobInfo::new(
                "registered_job",
                1,
                10,
                |_state, _payload| Box::pin(async move { Ok(()) }),
            )],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        // "unregistered_job" is never registered on this runtime, so the
        // enqueue itself fails with an `Err` before the job ever reaches the
        // queue — the exact scenario that used to leak the `Pending` record
        // `store.create` had already written moments earlier.
        let err = enqueue_tracked("unregistered_job", serde_json::json!({}))
            .await
            .expect_err("enqueueing an unregistered job name must error");
        assert!(
            err.to_string().contains("is not registered"),
            "unexpected error: {err}"
        );

        let key = store
            .last_created_key
            .lock()
            .unwrap()
            .clone()
            .expect("store.create should have been called");
        let record = store.get(&key).await.unwrap().expect("record");
        assert_eq!(
            record.status,
            TrackedJobStatus::Failed,
            "the orphaned Pending record must be settled to Failed, not left dangling"
        );

        shutdown.cancel();
        crate::job::clear_global_job_client();
    }

    // ── Redis/Postgres store selection (no live service needed) ──────────────

    #[tokio::test]
    async fn ensure_tracking_store_installed_reinstalls_after_clear_even_with_a_stale_state_extension()
     {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let state = AppState::for_test();
        ensure_tracking_store_installed(&state);
        assert!(global_tracking_store().is_some(), "sanity: store installed");

        // Simulate a runtime restart that clears global state without ever
        // constructing a fresh AppState (e.g. an app's own restart helper
        // that always calls clear_global_job_client() before reinitializing
        // the runtime on the same, already-built AppState).
        crate::job::clear_global_job_client();
        assert!(
            global_tracking_store().is_none(),
            "sanity: clear really did reset the global"
        );

        // `state` still carries the stale JobTrackingStoreEntry extension
        // from before the clear; without the fix this call would see that
        // extension and skip reinstalling, leaving GLOBAL_TRACKING_STORE
        // stuck at None and enqueue_tracked permanently broken.
        ensure_tracking_store_installed(&state);
        assert!(
            global_tracking_store().is_some(),
            "the tracking store must be reinstalled after a clear even when \
             the same AppState (with its now-stale extension) is reused"
        );

        crate::job::clear_global_job_client();
    }

    #[tokio::test]
    async fn ensure_tracking_store_installed_from_config_reinstalls_after_clear_even_with_a_stale_state_extension()
     {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let state = AppState::for_test();
        let config = crate::config::JobConfig::default();
        ensure_tracking_store_installed_from_config(&state, &config);
        assert!(global_tracking_store().is_some(), "sanity: store installed");

        crate::job::clear_global_job_client();
        assert!(
            global_tracking_store().is_none(),
            "sanity: clear reset the global"
        );

        ensure_tracking_store_installed_from_config(&state, &config);
        assert!(
            global_tracking_store().is_some(),
            "start_runtime's installer must reinstall after a clear even when \
             the same AppState (with its now-stale extension) is reused"
        );

        crate::job::clear_global_job_client();
    }

    #[cfg(feature = "redis")]
    #[test]
    fn build_redis_tracking_store_is_none_without_a_url() {
        let config = crate::config::JobConfig::default();
        assert!(config.redis.url.is_none());
        assert!(build_redis_tracking_store(&config).is_none());

        let mut config = crate::config::JobConfig::default();
        config.redis.url = Some("   ".to_owned());
        assert!(build_redis_tracking_store(&config).is_none());
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    async fn build_redis_tracking_store_is_some_for_an_unreachable_but_well_formed_url() {
        // ConnectionManager::new_lazy_with_config never dials eagerly, so
        // construction succeeds even though nothing is listening on this port
        // — it just needs a Tokio runtime context to spawn its background
        // reconnect task onto.
        let mut config = crate::config::JobConfig::default();
        config.redis.url = Some("redis://127.0.0.1:19999".to_owned());
        assert!(build_redis_tracking_store(&config).is_some());
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    async fn store_for_config_falls_back_to_memory_when_redis_backend_has_no_url() {
        let config = crate::config::JobConfig {
            backend: "redis".to_owned(),
            ..Default::default()
        };
        let state = AppState::for_test();
        // Falling back must not panic; the resulting store is still usable.
        let store = store_for_config(&state, &config);
        assert!(store.get("nope").await.unwrap().is_none());
    }
}
