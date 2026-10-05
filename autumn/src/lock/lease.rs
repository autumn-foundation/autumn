//! Fencing lease lock over a `PostgreSQL` row (issue #3053).
//!
//! Each lock name is one row in `autumn_lease_locks`. Each acquire, renew and
//! release is one autocommit statement, and every lease time comes from the
//! database `now()`. So the lock works through a transaction-mode pooler, and
//! app wall clocks do not decide who holds it.
//!
//! | Step    | Statement (guard)                                             |
//! | ------- | ------------------------------------------------------------- |
//! | acquire | upsert, `generation + 1`, if free or expired                  |
//! | renew   | extend `expires_at`, if same `generation` and not yet expired |
//! | release | `owner = NULL`, if same `generation`                          |
//!
//! The row stays after release, so the generation never goes down. The model
//! and its proofs are in `verification/lease_fencing.rs`.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use diesel::OptionalExtension as _;
use diesel_async::pooled_connection::deadpool::{Object, Pool};
use diesel_async::{AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _};
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{DEFAULT_LOCK_POLL_INTERVAL, FencingToken, LockError};

/// Default lease duration. A live holder renews every third of it.
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

/// Floor on the lease duration. A shorter lease cannot renew in time.
const MIN_LEASE_TTL: Duration = Duration::from_secs(1);

/// Ceiling on the lease duration. It keeps the `interval` arithmetic in range.
const MAX_LEASE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A holder renews every `ttl / RENEW_DIVISOR`.
const RENEW_DIVISOR: u32 = 3;

/// After a failed renewal, the holder tries again every `renew / RETRY_DIVISOR`.
const RETRY_DIVISOR: u32 = 4;

/// Floor on poll and retry intervals, so a zero value cannot busy-spin.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

const TABLE: &str = "autumn_lease_locks";

/// The table DDL. The framework migration holds the same text.
const CREATE_TABLE: &str =
    include_str!("../../migrations/20261005143012_create_lease_locks/up.sql");

const ACQUIRE: &str = "\
    INSERT INTO autumn_lease_locks AS l (name, owner, generation, expires_at, acquired_at) \
    VALUES ($1, $2, 1, now() + $3 * interval '1 millisecond', now()) \
    ON CONFLICT (name) DO UPDATE \
    SET owner = EXCLUDED.owner, \
        generation = l.generation + 1, \
        expires_at = EXCLUDED.expires_at, \
        acquired_at = EXCLUDED.acquired_at \
    WHERE l.owner IS NULL OR l.expires_at <= now() \
    RETURNING generation";

const RENEW: &str = "\
    UPDATE autumn_lease_locks SET expires_at = now() + $3 * interval '1 millisecond' \
    WHERE name = $1 AND generation = $2 AND owner IS NOT NULL AND expires_at > now()";

const RELEASE: &str = "\
    UPDATE autumn_lease_locks SET owner = NULL \
    WHERE name = $1 AND generation = $2 AND owner IS NOT NULL";

#[derive(diesel::QueryableByName)]
struct GenerationRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
}

/// Transaction-scoped advisory key that serializes the first-use DDL.
fn schema_lock_key() -> i64 {
    let digest = Sha256::digest(b"autumn:lease-locks:v1\0schema");
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// A diagnostic owner label: `host:pid#seq`. The generation, not the owner,
/// identifies a grant, so the label does not need to be unique.
fn next_owner() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| String::from("unknown"));
    format!("{host}:{}#{seq}", std::process::id())
}

fn ttl_ms(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

fn redacted(error: &impl std::fmt::Display) -> String {
    crate::db_url::redact_targets_in_message(&error.to_string())
}

fn db_error(error: impl std::fmt::Display) -> LockError {
    LockError::Database(redacted(&error))
}

fn pool_error(error: impl std::fmt::Display) -> LockError {
    LockError::PoolUnavailable(redacted(&error))
}

/// `true` when the error says that the lease table does not exist (`42P01`).
fn is_missing_table(error: &diesel::result::Error) -> bool {
    let message = error.to_string();
    message.contains(TABLE) && message.contains("does not exist")
}

/// Create the table.
///
/// The runtime calls this only after a statement finds no table, so a
/// database that ran the framework migration never needs `CREATE`. Two
/// concurrent `CREATE TABLE IF NOT EXISTS` can fail on the catalog, so the
/// DDL runs under a transaction-scoped advisory lock.
async fn ensure_table(conn: &mut AsyncPgConnection) -> Result<(), LockError> {
    // One simple-query message with two statements runs as one implicit
    // transaction, so the transaction-scoped lock covers the DDL.
    conn.batch_execute(&format!(
        "SELECT pg_advisory_xact_lock({}); {CREATE_TABLE}",
        schema_lock_key()
    ))
    .await
    .map_err(db_error)
}

/// A named lock that grants a lease with a [`FencingToken`].
///
/// Use it when a second holder must not corrupt a resource. The lock alone
/// cannot prevent that: a holder can pause or lose its connection, and its
/// lease can expire while it runs. Send the token with each write, and let the
/// resource reject a stale token. See the "Fencing tokens" section of
/// `docs/guide/distributed-locks.md`.
///
/// `PostgreSQL` only. The lock holds no connection while held.
#[derive(Clone)]
pub struct LeaseLock {
    pool: Pool<AsyncPgConnection>,
    name: Arc<str>,
    lease_ttl: Duration,
    poll_interval: Duration,
}

impl std::fmt::Debug for LeaseLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseLock")
            .field("name", &self.name)
            .field("lease_ttl", &self.lease_ttl)
            .finish_non_exhaustive()
    }
}

impl LeaseLock {
    /// Make a lock on `pool` with the name `name`. Use a primary pool.
    #[must_use]
    pub fn new(pool: Pool<AsyncPgConnection>, name: impl Into<String>) -> Self {
        Self {
            pool,
            name: Arc::from(name.into()),
            lease_ttl: DEFAULT_LEASE_TTL,
            poll_interval: DEFAULT_LOCK_POLL_INTERVAL,
        }
    }

    /// Make a lock from the primary pool of a [`DbState`](crate::db::DbState).
    ///
    /// # Errors
    ///
    /// Returns [`LockError::PoolUnavailable`] if the state has no primary pool.
    pub fn from_state<S: crate::db::DbState + ?Sized>(
        state: &S,
        name: impl Into<String>,
    ) -> Result<Self, LockError> {
        let pool = state.pool().ok_or_else(|| {
            LockError::PoolUnavailable("no primary database pool configured".to_string())
        })?;
        Ok(Self::new(pool.clone(), name))
    }

    /// The lock name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Set the lease duration (default 30s, floor 1s, ceiling 24h).
    ///
    /// A holder renews every third of it. A holder that cannot renew sees
    /// [`LeaseGuard::lease_lost`] after two thirds of it. The database gives
    /// the lease to a new holder only after the full duration.
    #[must_use]
    pub const fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl;
        self
    }

    /// Set the poll interval of [`Self::lock`] and [`Self::lock_timeout`]
    /// (default [`DEFAULT_LOCK_POLL_INTERVAL`]).
    #[must_use]
    pub const fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    fn ttl(&self) -> Duration {
        self.lease_ttl.clamp(MIN_LEASE_TTL, MAX_LEASE_TTL)
    }

    async fn checkout(&self) -> Result<Object<AsyncPgConnection>, LockError> {
        self.pool.get().await.map_err(pool_error)
    }

    /// Try to get the lease without waiting.
    ///
    /// Returns `Ok(None)` if another holder has a live lease. If you cancel
    /// this future after the database grants the lease, no holder has the
    /// lease. It stays taken until it expires (one TTL).
    ///
    /// # Errors
    ///
    /// Returns [`LockError`] if no connection is available or a query fails.
    pub async fn try_lock(&self) -> Result<Option<LeaseGuard>, LockError> {
        let sent = Instant::now();
        let conn = self.checkout().await?;
        self.try_lock_on(conn, sent).await
    }

    /// One acquire attempt. `sent` is the time before the checkout. The local
    /// validity starts there, so it ends before the database expiry.
    async fn try_lock_on(
        &self,
        mut conn: Object<AsyncPgConnection>,
        sent: Instant,
    ) -> Result<Option<LeaseGuard>, LockError> {
        let ttl = self.ttl();
        let row = match self.acquire(&mut conn, ttl).await {
            Err(error) if is_missing_table(&error) => {
                ensure_table(&mut conn).await?;
                self.acquire(&mut conn, ttl).await
            }
            other => other,
        }
        .map_err(db_error)?;
        drop(conn);
        let Some(row) = row else {
            return Ok(None);
        };
        let token = FencingToken::try_from(row.generation).map_err(db_error)?;
        Ok(Some(LeaseGuard::new(
            self.pool.clone(),
            Arc::clone(&self.name),
            token,
            ttl,
            sent,
        )))
    }

    async fn acquire(
        &self,
        conn: &mut AsyncPgConnection,
        ttl: Duration,
    ) -> Result<Option<GenerationRow>, diesel::result::Error> {
        diesel::sql_query(ACQUIRE)
            .bind::<diesel::sql_types::Text, _>(&*self.name)
            .bind::<diesel::sql_types::Text, _>(next_owner())
            .bind::<diesel::sql_types::BigInt, _>(ttl_ms(ttl))
            .get_result::<GenerationRow>(conn)
            .await
            .optional()
    }

    /// Wait for the lease. Polls at the poll interval.
    ///
    /// # Errors
    ///
    /// Returns [`LockError`] if no connection is available or a query fails.
    pub async fn lock(&self) -> Result<LeaseGuard, LockError> {
        loop {
            if let Some(guard) = self.try_lock().await? {
                return Ok(guard);
            }
            tokio::time::sleep(self.poll_interval.max(MIN_INTERVAL)).await;
        }
    }

    /// Wait for the lease for up to `timeout`.
    ///
    /// The pool checkout is in the budget, so an exhausted pool gives
    /// [`LockError::Timeout`] on time. An acquire query that is in flight at
    /// the deadline can run past it: cancelling that query could grant a lease
    /// that no holder has.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Timeout`] if the lease is not free in time, or
    /// another [`LockError`] if a query fails.
    pub async fn lock_timeout(&self, timeout: Duration) -> Result<LeaseGuard, LockError> {
        let start = Instant::now();
        let timed_out = || LockError::Timeout {
            name: self.name.to_string(),
            waited: start.elapsed(),
        };
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(timed_out());
            }
            let sent = Instant::now();
            let conn = tokio::time::timeout(remaining, self.checkout())
                .await
                .map_err(|_| timed_out())??;
            if let Some(guard) = self.try_lock_on(conn, sent).await? {
                return Ok(guard);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            tokio::time::sleep(self.poll_interval.max(MIN_INTERVAL).min(remaining)).await;
        }
    }

    /// Wait for the lease, then run `f` with it.
    ///
    /// `f` gets the [`Lease`], to read the fencing token. If the holder loses
    /// the lease, the lock drops `f` and returns [`LockError::LeaseLost`]. If
    /// `f` ends in the same poll as the loss, its output is returned.
    ///
    /// # Errors
    ///
    /// Returns [`LockError`] if the lease is not acquired or is lost.
    pub async fn with<F, Fut, T>(&self, f: F) -> Result<T, LockError>
    where
        F: FnOnce(Lease) -> Fut,
        Fut: Future<Output = T>,
    {
        let guard = self.lock().await?;
        run_guarded(guard, f).await
    }

    /// Like [`Self::with`], but waits for up to `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Timeout`] if the lease is not free in time, or
    /// [`LockError::LeaseLost`] if the holder loses it while `f` runs.
    pub async fn with_timeout<F, Fut, T>(&self, timeout: Duration, f: F) -> Result<T, LockError>
    where
        F: FnOnce(Lease) -> Fut,
        Fut: Future<Output = T>,
    {
        let guard = self.lock_timeout(timeout).await?;
        run_guarded(guard, f).await
    }

    /// Run `f` only if the lease is free now.
    ///
    /// Returns `Ok(None)` if another holder has it.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::LeaseLost`] if the holder loses the lease while
    /// `f` runs, or another [`LockError`] if a query fails.
    pub async fn try_with<F, Fut, T>(&self, f: F) -> Result<Option<T>, LockError>
    where
        F: FnOnce(Lease) -> Fut,
        Fut: Future<Output = T>,
    {
        let Some(guard) = self.try_lock().await? else {
            return Ok(None);
        };
        run_guarded(guard, f).await.map(Some)
    }
}

/// Run `f` until it ends or the lease is lost, then release.
async fn run_guarded<F, Fut, T>(guard: LeaseGuard, f: F) -> Result<T, LockError>
where
    F: FnOnce(Lease) -> Fut,
    Fut: Future<Output = T>,
{
    let lease = guard.lease.clone();
    let lost = lease.lease_lost();
    let out = tokio::select! {
        biased;
        out = f(lease) => Some(out),
        () = lost => None,
    };
    if let Some(out) = out {
        if let Err(e) = guard.release().await {
            tracing::warn!(error = %e, "lease release failed after guarded section");
        }
        return Ok(out);
    }
    Err(LockError::LeaseLost {
        name: guard.name().to_owned(),
        token: guard.fencing_token(),
    })
}

/// The local validity end of one lease, shared by the guard, its clones and
/// the renewal task.
#[derive(Debug)]
struct Deadline {
    origin: Instant,
    nanos: AtomicU64,
}

impl Deadline {
    fn new(until: Instant) -> Self {
        let deadline = Self {
            origin: Instant::now(),
            nanos: AtomicU64::new(0),
        };
        deadline.set(until);
        deadline
    }

    fn get(&self) -> Instant {
        self.origin + Duration::from_nanos(AtomicU64::load(&self.nanos, Ordering::Acquire))
    }

    fn set(&self, until: Instant) {
        let nanos = until.saturating_duration_since(self.origin).as_nanos();
        self.nanos
            .store(u64::try_from(nanos).unwrap_or(u64::MAX), Ordering::Release);
    }
}

/// A cheap, cloneable view of one lease grant.
///
/// Move it into a task to read the fencing token or to stop work when the
/// lease ends.
#[derive(Clone, Debug)]
pub struct Lease {
    name: Arc<str>,
    token: FencingToken,
    ended: CancellationToken,
    deadline: Arc<Deadline>,
}

impl Lease {
    /// The lock name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The fencing token of this grant. It does not change on renewal.
    #[must_use]
    pub const fn fencing_token(&self) -> FencingToken {
        self.token
    }

    /// Returns `true` when this holder cannot rely on the lease: a renewal
    /// found it gone, no renewal succeeded in time, or the holder released
    /// it.
    ///
    /// The check reads the local deadline too, so it is correct when the
    /// renewal task cannot run (for example, a busy `current_thread` runtime).
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.ended.is_cancelled() || Instant::now() >= self.deadline.get()
    }

    /// Resolves when [`Self::is_lost`] becomes `true`. Use it as a
    /// cancellation signal, for example in `tokio::select!`.
    pub fn lease_lost(&self) -> impl Future<Output = ()> + Send + 'static {
        let ended = self.ended.clone();
        let deadline = Arc::clone(&self.deadline);
        async move {
            loop {
                let until = deadline.get();
                tokio::select! {
                    () = ended.cancelled() => return,
                    () = tokio::time::sleep_until(until) => {
                        if Instant::now() >= deadline.get() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// A held lease. Releases on drop.
///
/// A background task renews the lease every third of its TTL. If a renewal
/// finds the lease gone, or no renewal succeeds for two thirds of the TTL,
/// the lease is lost. Call [`LeaseGuard::release`]. A dropped guard releases
/// in a background task. If that task fails, the lease expires after one TTL.
pub struct LeaseGuard {
    pool: Pool<AsyncPgConnection>,
    lease: Lease,
    renew: Option<tokio::task::JoinHandle<()>>,
    released: bool,
}

impl LeaseGuard {
    fn new(
        pool: Pool<AsyncPgConnection>,
        name: Arc<str>,
        token: FencingToken,
        ttl: Duration,
        sent: Instant,
    ) -> Self {
        let renewal = Renewal::new(ttl);
        let lease = Lease {
            name,
            token,
            ended: CancellationToken::new(),
            deadline: Arc::new(Deadline::new(sent + renewal.valid_for)),
        };
        let renew = renewal.spawn(pool.clone(), lease.clone(), sent);
        Self {
            pool,
            lease,
            renew,
            released: false,
        }
    }

    /// The lock name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.lease.name()
    }

    /// The fencing token of this grant.
    #[must_use]
    pub const fn fencing_token(&self) -> FencingToken {
        self.lease.token
    }

    /// A cloneable view of this lease.
    #[must_use]
    pub const fn lease(&self) -> &Lease {
        &self.lease
    }

    /// Returns `true` when the lease is lost. See [`Lease::is_lost`].
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lease.is_lost()
    }

    /// Resolves when the lease is lost. Stop work on the resource then.
    pub fn lease_lost(&self) -> impl Future<Output = ()> + Send + 'static {
        self.lease.lease_lost()
    }

    /// Release the lease.
    ///
    /// The release matches the generation, so a stale guard cannot free the
    /// lease of the next holder. If the update fails or you cancel this
    /// future, the guard tries again in a background task.
    ///
    /// # Errors
    ///
    /// Returns [`LockError`] if the update fails.
    pub async fn release(mut self) -> Result<(), LockError> {
        self.stop_renewal();
        let result = release(&self.pool, &self.lease).await;
        self.released = result.is_ok();
        result
    }

    /// Stop the renewal and end the lease locally.
    fn stop_renewal(&mut self) {
        if let Some(renew) = self.renew.take() {
            renew.abort();
        }
        self.lease.ended.cancel();
    }
}

async fn release(pool: &Pool<AsyncPgConnection>, lease: &Lease) -> Result<(), LockError> {
    let mut conn = pool.get().await.map_err(pool_error)?;
    let rows = diesel::sql_query(RELEASE)
        .bind::<diesel::sql_types::Text, _>(&*lease.name)
        .bind::<diesel::sql_types::BigInt, _>(lease.token.as_i64())
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;
    if rows == 0 {
        // The "lease lost" warning has already told the operator.
        tracing::debug!(
            lock_name = %lease.name,
            fencing_token = %lease.token,
            "lease was already lost when released"
        );
    }
    Ok(())
}

impl std::fmt::Debug for LeaseGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseGuard")
            .field("name", &self.lease.name)
            .field("fencing_token", &self.lease.token)
            .field("lost", &self.lease.is_lost())
            .finish_non_exhaustive()
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.stop_renewal();
        if tokio::runtime::Handle::try_current().is_ok() {
            let (pool, lease) = (self.pool.clone(), self.lease.clone());
            tokio::spawn(async move {
                let _ = release(&pool, &lease).await;
            });
        }
    }
}

/// The renewal schedule of one lease.
#[derive(Clone, Copy)]
struct Renewal {
    ttl: Duration,
    every: Duration,
    retry: Duration,
    /// How long a renewal sent at `t` keeps the lease valid locally:
    /// `ttl - every`. The rest of the TTL is margin for clock drift and for
    /// the holder to stop.
    valid_for: Duration,
}

impl Renewal {
    fn new(ttl: Duration) -> Self {
        let every = (ttl / RENEW_DIVISOR).max(MIN_INTERVAL);
        Self {
            ttl,
            every,
            retry: (every / RETRY_DIVISOR).max(MIN_INTERVAL),
            valid_for: ttl.saturating_sub(every),
        }
    }

    /// Start the loop. `None` without a runtime: the lease then expires, the
    /// same as when the process stops.
    fn spawn(
        self,
        pool: Pool<AsyncPgConnection>,
        lease: Lease,
        sent: Instant,
    ) -> Option<tokio::task::JoinHandle<()>> {
        tokio::runtime::Handle::try_current().ok()?;
        Some(tokio::spawn(self.run(pool, lease, sent)))
    }

    async fn run(self, pool: Pool<AsyncPgConnection>, lease: Lease, sent: Instant) {
        let mut next = sent + self.every;
        loop {
            let valid_until = lease.deadline.get();
            tokio::time::sleep_until(next.min(valid_until)).await;
            if Instant::now() >= valid_until {
                return lose(&lease, "no renewal succeeded in time");
            }
            let sent = Instant::now();
            match tokio::time::timeout_at(valid_until, self.renew_once(&pool, &lease)).await {
                Ok(Ok(true)) => {
                    lease.deadline.set(sent + self.valid_for);
                    next = sent + self.every;
                }
                Ok(Ok(false)) => return lose(&lease, "the lease expired or has a new holder"),
                Ok(Err(error)) => {
                    tracing::debug!(
                        lock_name = %lease.name,
                        error = %error,
                        "lease renewal failed; retrying"
                    );
                    next = Instant::now() + self.retry;
                }
                Err(_) => return lose(&lease, "no renewal succeeded in time"),
            }
        }
    }

    /// `Ok(true)` renewed, `Ok(false)` the lease is gone, `Err` no answer.
    async fn renew_once(
        self,
        pool: &Pool<AsyncPgConnection>,
        lease: &Lease,
    ) -> Result<bool, LockError> {
        let mut conn = pool.get().await.map_err(pool_error)?;
        let rows = diesel::sql_query(RENEW)
            .bind::<diesel::sql_types::Text, _>(&*lease.name)
            .bind::<diesel::sql_types::BigInt, _>(lease.token.as_i64())
            .bind::<diesel::sql_types::BigInt, _>(ttl_ms(self.ttl))
            .execute(&mut *conn)
            .await
            .map_err(db_error)?;
        Ok(rows > 0)
    }
}

fn lose(lease: &Lease, reason: &'static str) {
    tracing::warn!(
        lock_name = %lease.name,
        fencing_token = %lease.token,
        reason,
        "lease lost"
    );
    lease.ended.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_pool() -> Pool<AsyncPgConnection> {
        let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
            AsyncPgConnection,
        >::new("postgres://localhost/unused");
        Pool::builder(manager).build().expect("pool builds lazily")
    }

    fn lease_until(until: Instant) -> Lease {
        Lease {
            name: Arc::from("x"),
            token: FencingToken::try_from(1).expect("token"),
            ended: CancellationToken::new(),
            deadline: Arc::new(Deadline::new(until)),
        }
    }

    #[test]
    fn statements_carry_their_guards() {
        // The guards are the model in verification/lease_fencing.rs.
        assert!(ACQUIRE.contains("generation = l.generation + 1"));
        assert!(ACQUIRE.contains("WHERE l.owner IS NULL OR l.expires_at <= now()"));
        assert!(RENEW.contains("generation = $2 AND owner IS NOT NULL AND expires_at > now()"));
        assert!(RELEASE.contains("generation = $2 AND owner IS NOT NULL"));
        assert!(!RELEASE.contains("DELETE"), "release must keep the row");
    }

    #[test]
    fn schema_key_is_outside_the_app_lock_keyspace() {
        assert_ne!(
            schema_lock_key(),
            super::super::distributed_lock_key("autumn:lease-locks:v1\0schema")
        );
    }

    #[test]
    fn renewal_schedule_leaves_a_margin() {
        let renewal = Renewal::new(Duration::from_secs(30));
        assert_eq!(renewal.every, Duration::from_secs(10));
        assert_eq!(renewal.valid_for, Duration::from_secs(20));
        assert!(renewal.retry < renewal.every);
    }

    #[test]
    fn ttl_is_clamped() {
        let lock = LeaseLock::new(lazy_pool(), "x");
        assert_eq!(
            lock.clone().with_lease_ttl(Duration::ZERO).ttl(),
            MIN_LEASE_TTL
        );
        assert_eq!(lock.with_lease_ttl(Duration::MAX).ttl(), MAX_LEASE_TTL);
    }

    #[test]
    fn owner_label_names_host_and_process() {
        let owner = next_owner();
        assert!(
            owner.contains(&format!(":{}#", std::process::id())),
            "{owner}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lease_is_lost_at_the_local_deadline_without_the_renewal_task() {
        let lease = lease_until(Instant::now() + Duration::from_secs(2));
        assert!(!lease.is_lost());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(lease.is_lost(), "the deadline alone must end the lease");
    }

    #[tokio::test(start_paused = true)]
    async fn lease_lost_follows_a_moved_deadline() {
        let lease = lease_until(Instant::now() + Duration::from_secs(1));
        let lost = tokio::spawn(lease.lease_lost());
        lease.deadline.set(Instant::now() + Duration::from_secs(5));
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(!lost.is_finished(), "a renewed deadline must not fire");
        tokio::time::advance(Duration::from_secs(4)).await;
        lost.await.expect("lease_lost resolves at the new deadline");
    }

    #[tokio::test]
    async fn lease_lost_fires_on_cancel() {
        let lease = lease_until(Instant::now() + Duration::from_secs(60));
        let lost = lease.lease_lost();
        lease.ended.cancel();
        tokio::time::timeout(Duration::from_secs(1), lost)
            .await
            .expect("cancel ends the lease");
        assert!(lease.is_lost());
    }

    #[test]
    fn missing_table_error_is_detected() {
        let error = diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::Unknown,
            Box::new(String::from(
                "relation \"autumn_lease_locks\" does not exist",
            )),
        );
        assert!(is_missing_table(&error));
        assert!(!is_missing_table(&diesel::result::Error::NotFound));
    }
}
