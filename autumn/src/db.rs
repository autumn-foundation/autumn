//! Database connection pool and extractor.
//!
//! This module provides async Postgres connectivity via `diesel-async` with
//! the `deadpool` connection pool. The pool is created at startup by
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

use axum::extract::FromRequestParts;
use diesel;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
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

pub(crate) fn reject_ambient_after_commit_registry_for_tx() -> Result<(), AutumnError> {
    if AFTER_COMMIT_REGISTRY.try_with(|_| ()).is_ok() {
        return Err(AutumnError::bad_request_msg(
            "Nested Db::tx calls are not supported",
        ));
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
pub trait DbState {
    /// Returns the database connection pool, if configured.
    fn pool(&self) -> Option<&Pool<AsyncPgConnection>>;

    /// Returns the metrics collector, if configured.
    fn metrics(&self) -> Option<&crate::middleware::MetricsCollector> {
        None
    }

    /// Returns the read/replica connection pool, if configured.
    fn replica_pool(&self) -> Option<&Pool<AsyncPgConnection>> {
        None
    }

    /// Returns the pool used for read-only work.
    ///
    /// Defaults to the replica role when present, otherwise the primary role.
    fn read_pool(&self) -> Option<&Pool<AsyncPgConnection>> {
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
    /// Returns the global statement timeout, if configured.
    fn statement_timeout(&self) -> Option<std::time::Duration> {
        None
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

/// Scrubs literals from a SQL string to create a safe fingerprint for logging and metrics.
///
/// This function exists to ensure that raw SQL queries—which may contain personally
/// identifiable information (PII) or sensitive data—can be safely aggregated and logged.
/// By replacing string and numeric literals with a placeholder (`?`), it preserves the
/// structure of the query while removing the dynamic, potentially sensitive values.
///
/// # Examples
///
/// ```
/// use autumn_web::db::scrub_sql;
///
/// let raw_sql = "SELECT * FROM users WHERE email = 'test@example.com' AND age > 18";
/// let scrubbed = scrub_sql(raw_sql);
///
/// assert_eq!(scrubbed, "SELECT * FROM users WHERE email = '?' AND age > ?");
/// ```
///
/// # Details
///
/// This function performs a best-effort, token-aware scrub rather than relying on a
/// full SQL parser or naive regular expressions. It handles:
///
/// - Standard single-quoted strings (`'foo'`)
/// - Escape strings (`E'foo\n'`)
/// - Dollar-quoted strings (`$$foo$$` or `$tag$foo$tag$`)
/// - Standalone numeric literals (`123`, `4.56`, `1e10`, `.5`)
///
/// Identifiers (like table or column names) and structural SQL elements are preserved.
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
    let start = std::time::Instant::now();
    let result = query().await;
    let elapsed = start.elapsed();
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

    // Record metrics regardless of success/failure
    let verb = sql.split_whitespace().next().unwrap_or("?");
    let metric_key = format!("{route_key} {verb}");
    metrics.record_db_query(&metric_key, elapsed_ms);

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

    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);

    while let Some(e) = source {
        // Try downcasting to tokio_postgres::Error
        if e.downcast_ref::<tokio_postgres::Error>()
            .and_then(|pg_err| pg_err.code())
            == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        {
            return true;
        }
        // Try downcasting to tokio_postgres::error::DbError
        if e.downcast_ref::<tokio_postgres::error::DbError>()
            .is_some_and(|db_err| db_err.code() == &tokio_postgres::error::SqlState::QUERY_CANCELED)
        {
            return true;
        }
        source = e.source();
    }
    false
}

/// Error type for pool creation failures.
///
/// Alias for the deadpool `BuildError`. Returned by [`create_pool`] when
/// the pool cannot be constructed (e.g., invalid max-size configuration).
pub type PoolError = diesel_async::pooled_connection::deadpool::BuildError;

/// Primary plus optional read-replica database pools.
#[derive(Clone)]
pub struct DatabaseTopology {
    primary: Pool<AsyncPgConnection>,
    replica: Option<Pool<AsyncPgConnection>>,
    /// Connection URL to target with startup migrations, when the provider
    /// resolved one at runtime that the static config doesn't carry (e.g. the
    /// managed-Postgres provider whose socket URL is only known after boot).
    /// Scoping it to the topology keeps it per-app instead of a process global.
    migration_url: Option<String>,
}

impl DatabaseTopology {
    /// Build a topology from explicit primary and optional replica pools.
    ///
    /// This is useful for custom [`DatabasePoolProvider`] implementations that
    /// need to create or decorate both roles themselves.
    #[must_use]
    pub const fn from_pools(
        primary: Pool<AsyncPgConnection>,
        replica: Option<Pool<AsyncPgConnection>>,
    ) -> Self {
        Self {
            primary,
            replica,
            migration_url: None,
        }
    }

    /// Build a topology from a primary pool only.
    #[must_use]
    pub const fn primary_only(primary: Pool<AsyncPgConnection>) -> Self {
        Self {
            primary,
            replica: None,
            migration_url: None,
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

    /// Primary/write role pool.
    #[must_use]
    pub const fn primary(&self) -> &Pool<AsyncPgConnection> {
        &self.primary
    }

    /// Optional read/replica role pool.
    #[must_use]
    pub const fn replica(&self) -> Option<&Pool<AsyncPgConnection>> {
        self.replica.as_ref()
    }

    /// Pool used for read-only work.
    #[must_use]
    pub fn read(&self) -> &Pool<AsyncPgConnection> {
        self.replica.as_ref().unwrap_or(&self.primary)
    }
}

fn build_pool(
    url: &str,
    pool_size: usize,
    connect_timeout_secs: u64,
) -> Result<Pool<AsyncPgConnection>, PoolError> {
    let timeout = Duration::from_secs(connect_timeout_secs);
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager)
        .max_size(pool_size.max(1))
        .wait_timeout(Some(timeout))
        .create_timeout(Some(timeout))
        .runtime(deadpool::Runtime::Tokio1)
        .build()
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
pub fn create_pool(config: &DatabaseConfig) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
    let Some(url) = config.effective_primary_url() else {
        return Ok(None);
    };

    let pool = build_pool(
        url,
        config.effective_primary_pool_size(),
        config.connect_timeout_secs,
    )?;

    Ok(Some(pool))
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

    let primary = build_pool(
        primary_url,
        config.effective_primary_pool_size(),
        config.connect_timeout_secs,
    )?;
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
    let primary = build_pool(
        &shard.primary_url,
        shard.effective_primary_pool_size(defaults),
        defaults.connect_timeout_secs,
    )?;
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

// ── Db extractor ─────────────────────────────────────────────

/// Connection type managed by the deadpool pool.
pub type PooledConnection = diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>;

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
/// Declare `db: Db` in a handler signature to get a pooled connection to
/// Postgres. The connection is returned to the pool when `Db` is dropped
/// at the end of the request.
///
/// `Db` implements [`Deref`](std::ops::Deref) and
/// [`DerefMut`](std::ops::DerefMut) to
/// `diesel_async::AsyncPgConnection`, so you can use it directly with
/// Diesel query methods.
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
///     // `db` dereferences to AsyncPgConnection
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
    start_time: std::time::Instant,
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

    /// Run an async closure inside a database transaction.
    ///
    /// Commits when the closure returns `Ok(_)`, rolls back when it returns
    /// `Err(_)`.
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
        use diesel_async::AsyncConnection as _;

        if self.tx_poisoned {
            return Err(crate::error::AutumnError::service_unavailable_msg(
                "Database connection is in an invalid transaction state",
            ));
        }
        if self.tx_depth > 0 {
            return Err(crate::error::AutumnError::bad_request_msg(
                "Nested Db::tx calls are not supported",
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

        let result = AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), self.conn.transaction::<T, E, _>(f))
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
}

impl std::ops::Deref for Db {
    type Target = AsyncPgConnection;
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
    pub pool: &'a Pool<AsyncPgConnection>,
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
}

impl Db {
    /// Check a connection out of `params.pool` with full instrumentation.
    ///
    /// This is the single code path behind the [`Db`] extractor and all
    /// shard-routed checkouts: span creation, checkout interceptors,
    /// `SET statement_timeout`, and the metrics captured for the
    /// slow-query warning on `Drop`.
    pub(crate) async fn checkout(params: DbCheckoutParams<'_>) -> Result<Self, AutumnError> {
        const PG_TIMEOUT_MAX_MS: u64 = i32::MAX as u64;
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

        // Postgres statement_timeout is a signed 32-bit integer (milliseconds).
        // Cap at i32::MAX to avoid a confusing 503 for very large configured values.
        let timeout_ms = params.statement_timeout.map_or(0u64, |d| {
            u64::try_from(d.as_millis())
                .unwrap_or(PG_TIMEOUT_MAX_MS)
                .min(PG_TIMEOUT_MAX_MS)
        });

        diesel::sql_query(format!("SET statement_timeout = {timeout_ms}"))
            .execute(&mut conn)
            .await
            .map_err(|e| {
                tracing::error!("Failed to set database statement_timeout to {timeout_ms}ms: {e}");
                AutumnError::service_unavailable_msg(format!("Database initialization error: {e}"))
            })?;

        let start_time = std::time::Instant::now();
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
            is_test_tx,
        })
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
        }
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
            let elapsed = self.start_time.elapsed();
            let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

            // Record DB query metric
            let metric_key = format!("{route_key} SELECT");
            metrics.record_db_query(&metric_key, elapsed_ms);

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
    ) -> impl std::future::Future<Output = Result<Option<Pool<AsyncPgConnection>>, PoolError>> + Send;

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
    ) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
        create_pool(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

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
        ) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
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

    // ── Pool creation tests ──────────────────────────────────────

    #[tokio::test]
    async fn default_pool_provider_respects_url_config() {
        let config = DatabaseConfig {
            url: Some("postgres://localhost/test".into()),
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
            url: Some("postgres://localhost/test".into()),
            ..Default::default()
        };
        let pool = create_pool(&config).expect("should build pool from valid config");
        assert!(pool.is_some());
    }

    #[test]
    fn pool_respects_max_size() {
        let config = DatabaseConfig {
            url: Some("postgres://localhost/test".into()),
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
            url: Some("postgres://localhost/test".into()),
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

    #[test]
    fn database_topology_builds_primary_and_replica_pools() {
        let config = DatabaseConfig {
            primary_url: Some("postgres://localhost/primary".into()),
            replica_url: Some("postgres://localhost/replica".into()),
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
            url: Some("postgres://localhost/single".into()),
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
            url: Some("postgres://localhost/test".into()),
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
        fn pool(&self) -> Option<&Pool<AsyncPgConnection>> {
            None
        }
    }

    #[derive(Clone)]
    struct TestReadState {
        primary: Pool<AsyncPgConnection>,
    }

    impl DbState for TestReadState {
        fn pool(&self) -> Option<&Pool<AsyncPgConnection>> {
            Some(&self.primary)
        }
    }

    #[test]
    fn database_topology_read_pool_falls_back_to_primary() {
        let config = DatabaseConfig {
            url: Some("postgres://localhost/read-fallback".into()),
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
            primary_url: Some("postgres://user:pass@localhost/db".to_string()),
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

    #[tokio::test]
    async fn database_topology_from_pools_retains_replica() {
        let config = DatabaseConfig {
            primary_url: Some("postgres://user:pass@localhost/db".to_string()),
            replica_url: Some("postgres://user:pass@localhost/db_replica".to_string()),
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
}
